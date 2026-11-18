# Networking

AIPL gets networking one small operation at a time, starting with an HTTP GET like
Python's `requests.get`.

## Context

Every effect is a row in `SHIMMABLE_EFFECTS` (`crates/aipl-syntax/src/lib.rs`): its
operations get shim slots, both runtimes size their slot arrays by `SHIM_SLOT_COUNT`, and
`shims::every_effect_is_shimmable` fails for one that is not listed. `execute_program` is
the closest existing operation — a native builtin with a keyword parameter that returns a
struct-or-error (`__builtin_ExecResult!Error`):

- declared in `BUILTIN_SIGNATURES` (`aipl-syntax/src/lib.rs`), with its struct type beside
  it and named in `IMPORTABLE_BUILTIN_TYPES`; its `Callee` variant and
  `__builtin_*` name are in `aipl-syntax/src/callee.rs`; keyword defaults are filled by
  `aipl-loader/src/kwargs.rs`;
- implemented twice, by hand and kept in step: `aipl_execute_program` in
  `crates/aipl-codegen/src/lib.rs` (the JIT runtime, ordinary Rust) and in
  `crates/aipl-linker/runtime/aipl_runtime.rs` (the AOT runtime: `no_std`, libc only,
  compiled by `clang` in `aipl-linker`), both writing the wide result layout field by
  field (`write_ok_wide`/`write_err_wide`), registered in the JIT's symbol table and
  in `aipl-artifact`'s signature table.

The AOT runtime cannot use a Rust HTTP crate, so the transport is the system libcurl
(`-lcurl` links on macOS with no headers; `extern "C"` declarations are enough).

## Decisions

- **Transport: libcurl through FFI, in both runtimes.** One implementation of redirects,
  HTTPS (SecureTransport), timeouts and proxies, instead of two hand-written clients. The
  project supports macOS only, where libcurl ships.
- **A new effect `http_get`, with one operation of the same name.** Effects are granular:
  one per kind of request, so a program that may fetch a page need not be allowed every
  future network operation. Shimmable like every effect, so a test stands in for the network
  with `shim http_get { http_get = .. }`. It has a real implementation, so `main` may declare
  it.
- **The builtins live in a `network` library**, like `sets` and `json`:
  `import { http_get, HttpResponse, HttpHeader } from network;`. The library is only an
  import grouping; the effect is `!http_get`.
- **API:** `fn http_get(self: str, timeout_seconds: u64 = 30) !http_get -> HttpResponse!Error`
  with `struct HttpResponse { status: u16, headers: HttpHeader[], body: str }` and
  `struct HttpHeader { name: str, value: str }`. `url.http_get()` reads as a method.
  A non-2xx status is an `ok` response, as in `requests`; `err(message)` is for a request
  that produced no response (DNS, connect, TLS, timeout, a body that is not a valid `str`).
- **Redirects are followed** (up to a fixed limit), so `status` is the final response's.
- **A body is text.** A `str` is NUL-terminated and UTF-8, so a body holding a NUL or
  invalid UTF-8 is an `err`, the constraint `read_file_to_string` and `execute_program`
  already have. Binary bodies wait for a bytes type.

## Open questions

- Request headers (`http_get(url, headers = [..])`): the first cut has none. They need the
  runtimes to read an array of structs they are handed; settle after stage 3.
- Whether `http_get` should take a `Header[]` of the same struct as the response or a
  dict, once more verbs exist.

## Stages

Each ends at a green `cargo handoff`. 4 depends on 3. The declaration, the `network`
library, the `http_get` effect and its shim slot are done, and so is the AOT runtime: a
built binary links `-lcurl` and performs the GET (`http_curl` in `aipl_runtime.rs`). The
layout of the `HttpResponse!Error` and the text checks on a response are shared with the JIT
in `crates/aipl-codegen/src/http_response.rs` (`write_http_response`, `write_http_error`,
`RawResponse`), so a runtime supplies only its transport. The JIT's `aipl_http_get` still
answers `err("http_get is not available")`.

### 3. libcurl in the JIT runtime

Goal: `aipl run` and `check` perform the GET with the same result as a build.

- [ ] `mod http_response;` in `aipl-codegen/src/lib.rs` (beside `arr24`), and the build
      script's `rerun-if-changed` already lists it for the AOT side
- [ ] `aipl_http_get` in `aipl-codegen/src/lib.rs` performs the transfer through the same
      `curl_easy_*` calls, `#[link(name = "curl")]` on the `aipl` binary's side, and hands
      `RawResponse` to `write_http_response` / `write_http_error`
- [ ] the curl declarations and option numbers shared with the AOT runtime if the file can be
      included (they are `extern "C"` and `core` only), otherwise kept identical and
      commented as such

Watch: the AOT `http_curl` module is the reference; the JIT may use `std` for its buffers.

### 4. Tests against a real server, docs

Goal: the behavior is pinned end to end.

- [ ] a Rust suite (`tests/suites/network.rs`, a `mod` line in `tests/compiler.rs`) that
      serves canned responses from a `TcpListener` on an ephemeral port and runs an AIPL
      program against it, through `aipl run` and a built binary: status, headers, body, a
      404 as `ok`, a redirect, a refused connection and a timeout as `err`, a NUL body as
      `err`
- [ ] the builtin documented (`aipl docs`), and `http_get`/`HttpResponse` added to the
      `write-aipl` skill's builtin list

## Done when

`"http://127.0.0.1:<port>/".http_get()` returns the served status, headers and body from
`aipl run` and from a built binary, a test can shim the whole operation, `main` must
declare `!http_get`, and `cargo handoff` is green.
