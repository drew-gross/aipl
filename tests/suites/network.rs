//! `http_get` against a real server: a `TcpListener` on an ephemeral port serving canned
//! responses, and one AIPL program asked for each of them by every way a program is run.
//!
//! The `.aipl` cases can shim the operation but cannot stand a server up, which is why this
//! is a Rust suite. The three ways a program runs — `aipl run` (a temporary native binary),
//! `aipl run --jit` and the executable `aipl build` writes — reach libcurl through two
//! runtimes that share one transport (`http_curl.rs`), and each is held to the same answers.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// Prints what `http_get` gave for the URL it is passed, one fact to a line, and exits 0 for any
/// response and 1 when there was none. The timeout is short so the slow route fails quickly.
const PROGRAM: &str = r#"import { print } from builtins;
import { http_get } from network;

fn main(args: str[]) !prints !http_get -> i64 {
    let some(url) = args[0] else { return 2; };
    match (url.http_get(timeout_seconds=1)) {
        ok(response) => {
            print(`status {response.status}`);
            for (let header : response.headers) {
                print(`{header.name}: {header.value}`);
            }
            print(`body [{response.body}]`);
            0
        },
        err(message) => {
            print(`err: {message}`);
            1
        },
    }
}
"#;

/// What a route answers: a status line, the headers before the ones every response carries, and
/// the body.
struct Route {
    status: &'static str,
    headers: &'static [(&'static str, &'static str)],
    body: &'static [u8],
}

fn route(path: &str) -> Option<Route> {
    Some(match path {
        "/ok" => Route {
            status: "200 OK",
            headers: &[("X-Test", "one"), ("X-Test", "two")],
            body: b"hello",
        },
        "/missing" => Route {
            status: "404 Not Found",
            headers: &[],
            body: b"nope",
        },
        "/redirect" => Route {
            status: "302 Found",
            headers: &[("Location", "/ok")],
            body: b"",
        },
        "/nul" => Route {
            status: "200 OK",
            headers: &[],
            body: b"a\0b",
        },
        "/binary" => Route {
            status: "200 OK",
            headers: &[],
            body: &[0xff, 0xfe],
        },
        _ => return None,
    })
}

/// Answer one connection. `/slow` accepts the request and says nothing for longer than the
/// program waits.
fn serve(mut stream: TcpStream) {
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&chunk[..n]),
        }
    }
    let request = String::from_utf8_lossy(&request);
    let path = request
        .split_whitespace()
        .nth(1)
        .expect("a request line names a path");
    if path == "/slow" {
        std::thread::sleep(Duration::from_secs(5));
        return;
    }
    let route = route(path).unwrap_or_else(|| panic!("no route for {path}"));
    let mut response = format!("HTTP/1.1 {}\r\n", route.status);
    for (name, value) in route.headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        route.body.len()
    ));
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(route.body);
}

/// A server on an ephemeral port, answering every connection on a thread of its own, and the
/// address of a port nothing listens on.
fn start_server() -> (String, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a port");
    let server = format!("http://{}", listener.local_addr().expect("address"));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || serve(stream));
        }
    });
    let closed = TcpListener::bind("127.0.0.1:0").expect("bind a port");
    let refused = format!("http://{}", closed.local_addr().expect("address"));
    drop(closed);
    (server, refused)
}

/// One of the ways a program is run.
#[derive(Clone, Copy)]
enum Mode {
    Run,
    RunJit,
    Built,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Run => "run",
            Mode::RunJit => "run_jit",
            Mode::Built => "built",
        }
    }
}

/// Runs [`PROGRAM`] in `mode`, from a directory of its own.
struct Runner {
    mode: Mode,
    directory: PathBuf,
}

impl Runner {
    fn new(mode: Mode) -> Runner {
        let directory =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("network_{}", mode.name()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create the scratch directory");
        std::fs::write(directory.join("main.aipl"), PROGRAM).expect("write the program");
        if let Mode::Built = mode {
            let status = Command::new(env!("CARGO_BIN_EXE_aipl"))
                .args(["build", "main.aipl", "-o", "app"])
                .current_dir(&directory)
                .status()
                .expect("spawn aipl build");
            assert!(status.success(), "aipl build failed");
        }
        Runner { mode, directory }
    }

    /// The program's standard output and exit code when asked for `url`.
    fn get(&self, url: &str) -> (String, i32) {
        let mut command = match self.mode {
            Mode::Run => {
                let mut c = Command::new(env!("CARGO_BIN_EXE_aipl"));
                c.args(["run", "--", url]);
                c
            }
            Mode::RunJit => {
                let mut c = Command::new(env!("CARGO_BIN_EXE_aipl"));
                c.args(["run", "--jit", "--", url]);
                c
            }
            Mode::Built => {
                let mut c = Command::new(self.directory.join("app"));
                c.arg(url);
                c
            }
        };
        let out = command
            .current_dir(&self.directory)
            .output()
            .expect("run the program");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.code().expect("an exit code"),
        )
    }
}

/// Every route, asked for through `mode`.
fn answers_every_route(mode: Mode) {
    let (server, refused) = start_server();
    let runner = Runner::new(mode);
    let get = |path: &str| runner.get(&format!("{server}{path}"));

    assert_eq!(
        get("/ok"),
        (
            "status 200\nX-Test: one\nX-Test: two\nContent-Length: 5\nConnection: close\nbody [hello]\n"
                .to_string(),
            0
        ),
        "a response's status, its headers in the order they came, and its body"
    );
    assert_eq!(
        get("/missing"),
        (
            "status 404\nContent-Length: 4\nConnection: close\nbody [nope]\n".to_string(),
            0
        ),
        "a 404 is a response, not an error"
    );
    assert_eq!(
        get("/redirect"),
        get("/ok"),
        "a redirect is followed, and the answer is the final response's"
    );

    let not_text = (
        "err: the response body is not text (it holds a NUL byte or invalid UTF-8)\n".to_string(),
        1,
    );
    assert_eq!(get("/nul"), not_text, "a body with a NUL byte");
    assert_eq!(get("/binary"), not_text, "a body that is not UTF-8");

    let (timed_out, code) = get("/slow");
    assert!(
        timed_out.starts_with("err: Operation timed out after "),
        "a server too slow for `timeout_seconds`, got {timed_out:?}"
    );
    assert_eq!(code, 1);

    let (unreachable, code) = runner.get(&refused);
    let port = refused.rsplit(':').next().expect("a port");
    assert!(
        unreachable.starts_with(&format!(
            "err: Failed to connect to 127.0.0.1 port {port} after "
        )) && unreachable.ends_with("Couldn't connect to server\n"),
        "a connection nothing accepts, got {unreachable:?}"
    );
    assert_eq!(code, 1);

    assert_eq!(
        runner.get("file:///etc/hosts"),
        ("err: Protocol \"file\" disabled\n".to_string(), 1),
        "only the web is fetched"
    );
}

#[test]
fn run_answers_every_route() {
    answers_every_route(Mode::Run);
}

#[test]
fn run_jit_answers_every_route() {
    answers_every_route(Mode::RunJit);
}

#[test]
fn a_built_binary_answers_every_route() {
    answers_every_route(Mode::Built);
}
