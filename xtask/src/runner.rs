//! Running one labelled step: capture its output, bound it by a wall-clock
//! ceiling, time it, and report where the time went.

use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::machine;

pub const BOLD: &str = "\x1b[1m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const DIM: &str = "\x1b[2m";
pub const OFF: &str = "\x1b[0m";

/// The structured-output schema this gate parses. Pinned rather than left to
/// float: `--message-format-version` exists precisely so a consumer keeps a
/// stable shape across nextest upgrades, and the whole point of moving off text
/// parsing was to stop being coupled to a presentation format.
const MESSAGE_FORMAT_VERSION: &str = "0.1";

/// A command to run as a step: program, arguments, and any environment
/// additions. (`AIPL_CASE`, `AIPL_DOGFOOD_IR` and `AIPL_FMT_IR` are the only
/// ones the gate sets — the bash original spelled them with `env`.)
pub struct Cmd {
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
    /// This command emits the JSON event stream on stdout, so stdout is data
    /// rather than something to show a human.
    json: bool,
}

impl Cmd {
    pub fn new(program: &str) -> Self {
        Self {
            program: program.to_string(),
            args: Vec::new(),
            envs: Vec::new(),
            json: false,
        }
    }

    pub fn args<I: IntoIterator<Item = S>, S: AsRef<str>>(mut self, args: I) -> Self {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_string()));
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.envs.push((key.to_string(), value.to_string()));
        self
    }

    /// Ask nextest for the machine-readable event stream: one JSON object per
    /// line on **stdout**, with the ordinary human output still on **stderr**.
    ///
    /// Both halves matter. The JSON is what the gate parses — a failed test
    /// carries its own captured output in the record, so a mismatch message is
    /// attributed to the test that produced it instead of being grepped out of
    /// one flat interleaved blob. The human stderr is what a person watching
    /// sees (unchanged), *and* it is the only place an abnormal exit is
    /// reported: see [`crate::discovery::Run`].
    ///
    /// Still experimental in nextest, hence the opt-in env var. If that ever
    /// stops being accepted the run fails loudly rather than silently parsing
    /// nothing — see the empty-stream guard in [`crate::discovery::Run::parse`].
    pub fn json(mut self) -> Self {
        self.json = true;
        self.args.extend([
            "--message-format".to_string(),
            "libtest-json-plus".to_string(),
            "--message-format-version".to_string(),
            MESSAGE_FORMAT_VERSION.to_string(),
        ]);
        self.env("NEXTEST_EXPERIMENTAL_LIBTEST_JSON", "1")
    }
}

/// Whether each step's output is streamed as it runs, as well as captured.
///
/// This gate is run both by hand and by agents, and an agent pays for every line
/// it reads — capturing was the original point ("the whole sequence in a
/// fraction of the tokens"). But a human running it manually otherwise stares at
/// a step banner for minutes with no sign of life. So: stream when stderr is a
/// terminal, stay quiet when it isn't (piped into a tool, redirected to a file,
/// run by an agent). `HANDOFF_VERBOSE=1` forces streaming, `=0` forces quiet.
///
/// Streaming changes nothing about what is captured: the child's stdout is a
/// pipe either way, which no tool here distinguishes (`--color never` is passed
/// explicitly, and nextest keys its progress output on isatty — false for both).
fn verbose_from_env() -> bool {
    match std::env::var("HANDOFF_VERBOSE").as_deref() {
        Ok("1" | "true" | "yes") => true,
        Ok("0" | "false" | "no") => false,
        _ => std::io::stderr().is_terminal(),
    }
}

/// Wall-clock ceiling per step, in seconds; 0 disables.
///
/// Every step here is a `cargo nextest` invocation, and a wedged nextest
/// produces no output and no progress — indistinguishable from a slow build
/// until you go looking. Without a ceiling this just hangs, which is worse than
/// failing: the reader abandons it and starts hand-driving the sequence, which
/// is the one thing the gate exists to prevent.
///
/// The default is generous on purpose — a false timeout costs a whole re-run.
/// The slowest step is the cold build (~1500s observed), so 1800s bounds a hang
/// to half an hour while still clearing a genuine cold build.
fn timeout_from_env() -> Option<Duration> {
    let secs: u64 = std::env::var("HANDOFF_STEP_TIMEOUT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1800);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// One step's captured output, with the two streams kept apart.
///
/// They carry different things once a step runs under [`Cmd::json`]: **stdout**
/// is the JSON event stream, **stderr** is the human view — including the one
/// fact the JSON cannot express, an abnormal exit. `merged` is both in arrival
/// order, for the saved failure log and for steps that emit no JSON at all
/// (`cargo fmt`, the `--no-run` builds).
#[derive(Default)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub merged: String,
}

pub struct Runner {
    verbose: bool,
    step_timeout: Option<Duration>,
    /// `(seconds, label)` per step, reported slowest-first at exit.
    timings: Vec<(u64, String)>,
    started: Instant,
    /// The last step's captured output.
    pub out: Output,
    /// Where [`Runner::save_out`] last wrote it, so a failure message can point
    /// the reader at the full log.
    saved: Option<PathBuf>,
}

impl Runner {
    pub fn new() -> Self {
        Self {
            verbose: verbose_from_env(),
            step_timeout: timeout_from_env(),
            timings: Vec::new(),
            started: Instant::now(),
            out: Output::default(),
            saved: None,
        }
    }

    /// Run a labelled step, capturing its combined output into [`Runner::out`]
    /// (and, when verbose, streaming it too). The command's exit status is
    /// returned untouched for the caller to inspect.
    ///
    /// A step that outlives the ceiling never returns: it reports why and stops
    /// the whole gate, because there is nothing useful to do with a wedged run.
    pub fn step(&mut self, label: &str, cmd: Cmd) -> bool {
        eprintln!("\n{BOLD}==> {label}{OFF}");
        let start = Instant::now();
        let (ok, timed_out) = self.spawn_and_wait(&cmd);
        let secs = start.elapsed().as_secs();

        if timed_out {
            self.timings.push((secs, format!("{label} (timed out)")));
            self.save_out();
            let limit = self.step_timeout.map(|d| d.as_secs()).unwrap_or_default();
            self.fail(
                &format!("{label} (timed out after {limit}s)"),
                "No step here should take that long, so this is almost always the machine rather
than the tests — memory pressure, or a first-exec code-signing scan on a
freshly-linked test binary. The line above says which.

  * Machine starved (low free RAM, swap in use): free some and re-run.
  * Genuinely a slow box: raise the ceiling, e.g.
        HANDOFF_STEP_TIMEOUT=3600 cargo handoff
    or disable it with HANDOFF_STEP_TIMEOUT=0.

Re-run this gate — don't hand-drive the sequence it was about to do.",
            );
        }

        eprintln!("{DIM}    ({secs}s){OFF}");
        self.timings.push((secs, label.to_string()));
        ok
    }

    /// Spawn the step, pump its output, and wait — killing the tree if the
    /// ceiling expires. Returns `(succeeded, timed_out)`.
    fn spawn_and_wait(&mut self, cmd: &Cmd) -> (bool, bool) {
        self.out = Output::default();
        let mut child = match Command::new(&cmd.program)
            .args(&cmd.args)
            .envs(cmd.envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                self.out.merged = format!("failed to spawn {}: {e}", cmd.program);
                return (false, false);
            }
        };

        // Tagged lines into one buffer, so the two streams can be told apart
        // afterwards *and* still be replayed in arrival order.
        //
        // Only stderr is echoed for a JSON step: stdout there is the machine
        // stream, and dumping it at whoever is watching would bury the human
        // output that nextest is still writing to stderr.
        let lines = Arc::new(Mutex::new(Vec::new()));
        let verbose = self.verbose;
        let pumps = [
            child
                .stdout
                .take()
                .map(|s| pump(s, Stream::Out, Arc::clone(&lines), verbose && !cmd.json)),
            child
                .stderr
                .take()
                .map(|s| pump(s, Stream::Err, Arc::clone(&lines), verbose)),
        ];

        // The gate holds the child's pid directly, so the watchdog can target
        // *that* tree. (The bash original had to kill the whole script's
        // descendants instead: backgrounding the step to get a pid meant reading
        // its status out of a file, and that race was flaky — 11 runs in 40 read
        // the file before the write landed and reported a timeout for a plain
        // failure. Nothing here needs that workaround.)
        let pid = child.id();
        let deadline = self.step_timeout.map(|d| Instant::now() + d);
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(_) => break None,
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                timed_out = true;
                // Say why *before* killing: the diagnostics are about processes
                // that are still alive.
                let limit = self.step_timeout.map(|d| d.as_secs()).unwrap_or_default();
                eprintln!("\n{RED}    timed out after {limit}s{OFF}");
                let state = machine::machine_state();
                if !state.is_empty() {
                    eprintln!("{DIM}    {state}{OFF}");
                }
                if let Some(busy) = machine::busiest_child(pid) {
                    eprintln!("{DIM}    busiest child: {busy}{OFF}");
                }
                machine::kill_tree(pid);
                break child.wait().ok();
            }
            std::thread::sleep(Duration::from_millis(200));
        };

        // The pumps end when their pipes close, which the exit above guarantees.
        for pump in pumps.into_iter().flatten() {
            let _ = pump.join();
        }
        let tagged = std::mem::take(&mut *lines.lock().expect("output buffer"));
        for (stream, line) in tagged {
            let target = match stream {
                Stream::Out => &mut self.out.stdout,
                Stream::Err => &mut self.out.stderr,
            };
            target.push_str(&line);
            target.push('\n');
            self.out.merged.push_str(&line);
            self.out.merged.push('\n');
        }
        (status.is_some_and(|s| s.success()), timed_out)
    }

    /// Preserve the current step's output past the next step, so a failure
    /// message can point the reader at the full log.
    pub fn save_out(&mut self) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "handoff-fail-{}-{}.log",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if std::fs::write(&path, &self.out.merged).is_ok() {
            self.saved = Some(path);
        }
    }

    /// Point the "(full output: ..)" line at something other than a step log —
    /// used by the startup checks, which have no step output of their own.
    pub fn set_saved(&mut self, path: PathBuf) {
        self.saved = Some(path);
    }

    /// Abort with a step name and (optionally) the salient excerpt.
    pub fn fail(&self, label: &str, detail: &str) -> ! {
        eprintln!("\n{RED}{BOLD}HANDOFF FAILED at: {label}{OFF}");
        if !detail.is_empty() {
            eprintln!("{detail}");
        }
        if let Some(saved) = &self.saved {
            eprintln!("{DIM}(full output: {}){OFF}", saved.display());
        }
        self.timing_report();
        std::process::exit(1);
    }

    /// Where the wall-clock went, slowest first. Printed on success *and*
    /// failure — a handoff that stops late has still spent the time, and that is
    /// worth seeing.
    pub fn timing_report(&self) {
        if self.timings.is_empty() {
            return;
        }
        eprintln!(
            "\n{DIM}time: {}s total{OFF}",
            self.started.elapsed().as_secs()
        );
        let mut sorted: Vec<&(u64, String)> = self.timings.iter().collect();
        sorted.sort_by_key(|(secs, _)| std::cmp::Reverse(*secs));
        for (secs, label) in sorted {
            if *secs > 0 {
                eprintln!("{DIM}  {secs:5}s  {label}{OFF}");
            }
        }
    }
}

/// Which of a child's two streams a captured line came from.
#[derive(Clone, Copy, PartialEq)]
enum Stream {
    Out,
    Err,
}

/// Read `src` a line at a time into `lines`, tagged with its stream, echoing to
/// stderr when `echo`.
fn pump<R: Read + Send + 'static>(
    src: R,
    stream: Stream,
    lines: Arc<Mutex<Vec<(Stream, String)>>>,
    echo: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for line in BufReader::new(src).lines() {
            let Ok(line) = line else { break };
            if echo {
                // To stderr, like everything else the gate prints, so
                // `2>&1 | less` sees banners and step output in order.
                let mut err = std::io::stderr().lock();
                let _ = writeln!(err, "{line}");
            }
            lines.lock().expect("output buffer").push((stream, line));
        }
    })
}
