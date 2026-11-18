# aipl-curl

Fetches a URL with `http_get` and prints what the server answers, like a small `curl`.

```sh
$ ./aipl run -- http://example.com/
<!doctype html>…
$ ./aipl run -- -i http://example.com/
HTTP 200
Content-Type: text/html
…

<!doctype html>…
```

## Usage

```
aipl-curl [-i] [-t <seconds>] <url>
```

Prints the response's body. `-i` prints the status line and the headers first, then a blank
line. `-t` sets how long to wait (default 30 seconds).

Exits 0 for any response, a 404 included: the status is the server's answer, not a failure
(`-i` shows it). Exits 1 — after saying why — for a bad command line or when no response came
(the name did not resolve, the connection was refused, the request timed out, or the body was
not text).

`./aipl run -- <url>` builds and runs it in one step. `./aipl run --jit` runs it in-process, which
does not make requests yet: build it (`./aipl build main.aipl`) for that.

Not like `curl`: GET only, `http` and `https` only, redirects always followed, a body that is
not valid text is an error, and errors go to standard output.

## Layout

| file | what it is |
|---|---|
| `main.aipl` | `parse_args` and `render`, pure functions with their tests; and `main`, the shell around them |
| `aipl` | the compiler that builds this project — see [../README.md](../README.md) |
