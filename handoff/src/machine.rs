//! The part of the gate that is about the *machine* rather than the tests:
//! walking a process tree, killing one, and answering "is this hung, or is the
//! box just on its knees?".
//!
//! Everything here is best-effort and shells out to the platform tools
//! (`pgrep`, `ps`, `vm_stat`, `free`). Most of it is only consulted on a timeout —
//! a path that has already given up — and a missing tool degrades to a quieter
//! message rather than an error. The exception is the process-chain check
//! ([`ancestry`], [`chain_warning`]), which the gate runs once at startup and is
//! silent when it cannot read the chain.

use std::process::Command;

/// Stdout of `program args..`, or `None` if it couldn't run or exited non-zero.
fn capture(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Direct children of `pid`.
fn children_of(pid: u32) -> Vec<u32> {
    capture("pgrep", &["-P", &pid.to_string()])
        .into_iter()
        .flat_map(|s| {
            s.split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect::<Vec<u32>>()
        })
        .collect()
}

/// Every descendant of `pid`, **deepest first**, so a kill reaches the leaves
/// before their parents can be reparented away.
///
/// `cargo` fans out into rustc/link/test processes; killing only the direct
/// child leaves those running, and orphaned nextest children in particular go on
/// holding memory and file locks — which is what the startup sweep in `main`
/// exists to clean up after.
pub fn descendants(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for kid in children_of(pid) {
        out.extend(descendants(kid));
        out.push(kid);
    }
    out
}

/// Signal every pid in one `kill`, ignoring failures (a pid that already exited
/// is the common case, not an error).
pub fn signal(pids: &[u32], sig: &str) {
    if pids.is_empty() {
        return;
    }
    let mut cmd = Command::new("kill");
    cmd.arg(sig);
    for pid in pids {
        cmd.arg(pid.to_string());
    }
    let _ = cmd.output();
}

/// TERM the whole tree under `pid` (leaves first), give it a moment, then KILL
/// whatever is left — including `pid` itself.
///
/// Leaves first so nothing is reparented and left running: an orphaned
/// `cargo`/`nextest` tree holds memory and locks and makes the *next* attempt
/// slower for reasons that look nothing like the cause.
pub fn kill_tree(pid: u32) {
    let mut tree = descendants(pid);
    tree.push(pid);
    signal(&tree, "-TERM");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let mut tree = descendants(pid);
    tree.push(pid);
    signal(&tree, "-KILL");
}

/// Busiest descendant of `pid`, as `"<cpu>% <name>"`.
///
/// A step at 0% for minutes is *blocked* — memory thrash, or a first-exec
/// code-signing scan on a freshly-linked test binary — not computing. That
/// distinction is the whole reason this is printed on a timeout.
pub fn busiest_child(pid: u32) -> Option<String> {
    let pids = descendants(pid);
    if pids.is_empty() {
        return None;
    }
    let mut args = vec![
        "-o".to_string(),
        "%cpu=,comm=".to_string(),
        "-p".to_string(),
    ];
    args.extend(pids.iter().map(|p| p.to_string()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = capture("ps", &args)?;
    // Highest %cpu wins; `ps` gives `<cpu> <command>` per line.
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let cpu = it.next()?;
            let comm = it.next()?;
            Some((cpu.parse::<f64>().ok()?, comm.to_string()))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(cpu, comm)| format!("{cpu}% {comm}"))
}

/// One line of machine state, best-effort: free RAM and swap.
///
/// Printed only on a timeout, where the question is always "is this hung, or is
/// the machine just on its knees?" — memory pressure and swap answer it
/// immediately.
pub fn machine_state() -> String {
    let (mem, swap) = if cfg!(target_os = "macos") {
        (macos_free_ram(), macos_swap())
    } else {
        (linux_free_ram(), linux_swap())
    };
    match (mem, swap) {
        (Some(m), Some(s)) => format!("{m}, {s}"),
        (Some(m), None) => m,
        (None, Some(s)) => s,
        (None, None) => String::new(),
    }
}

/// `vm_stat` reports a page size in its header and free pages as a count, so
/// free RAM is the product.
fn macos_free_ram() -> Option<String> {
    let out = capture("vm_stat", &[])?;
    let mut page_size = None;
    let mut free_pages = None;
    for line in out.lines() {
        if line.contains("page size of") {
            page_size = line
                .split_whitespace()
                .find_map(|w| w.parse::<u64>().ok())
                .or(page_size);
        } else if line.starts_with("Pages free:") {
            free_pages = line
                .split_whitespace()
                .nth(2)
                .and_then(|w| w.trim_end_matches('.').parse::<u64>().ok());
        }
    }
    let (ps, free) = (page_size?, free_pages?);
    Some(format!("free RAM {} MB", ps * free / 1_048_576))
}

/// `vm.swapusage` is `total = <t>  used = <u>  free = <f>`, already
/// unit-suffixed.
fn macos_swap() -> Option<String> {
    let out = capture("sysctl", &["-n", "vm.swapusage"])?;
    let f: Vec<&str> = out.split_whitespace().collect();
    // total = $3, used = $6 (the `=` signs are fields of their own).
    Some(format!("swap {} used of {}", f.get(5)?, f.get(2)?))
}

fn linux_free_ram() -> Option<String> {
    let out = capture("free", &["-m"])?;
    let line = out.lines().find(|l| l.starts_with("Mem:"))?;
    Some(format!(
        "free RAM {} MB",
        line.split_whitespace().nth(6)? // "available"
    ))
}

fn linux_swap() -> Option<String> {
    let out = capture("free", &["-m"])?;
    let line = out.lines().find(|l| l.starts_with("Swap:"))?;
    let f: Vec<&str> = line.split_whitespace().collect();
    Some(format!("swap {} MB used of {} MB", f.get(2)?, f.get(1)?))
}

/// Pids of orphaned nextest listing children from a previous *interrupted* run.
///
/// nextest enumerates tests by running each binary with `--list --format terse`;
/// when its parent is killed those children survive blocked on a dead pipe,
/// holding memory and the build lock. They accumulate across retries and make
/// each attempt slower for reasons that look nothing like the cause. The pattern
/// is nextest's own argument string, so it cannot match anything else.
pub fn orphaned_listers() -> Vec<u32> {
    capture("pgrep", &["-f", "--", "--list --format terse"])
        .into_iter()
        .flat_map(|s| {
            s.split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect::<Vec<u32>>()
        })
        .collect()
}

/// One process in a chain: the last component of its command path, and the path
/// as `ps` printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub name: String,
    pub path: String,
}

/// The processes `pid` runs under, `pid` first and the process directly below
/// `launchd` last — or `None` when the walk could not reach the top (`ps` failed,
/// or the chain looped), where a partial chain would say nothing about its root.
pub fn ancestry(pid: u32) -> Option<Vec<Link>> {
    let mut chain = Vec::new();
    let mut at = pid;
    // Bounded, so a process table that loops on itself cannot hang the gate.
    for _ in 0..64 {
        let line = capture("ps", &["-o", "ppid=,comm=", "-p", &at.to_string()])?;
        let (parent, link) = parse_parent_line(&line)?;
        chain.push(link);
        if parent <= 1 {
            return Some(chain);
        }
        at = parent;
    }
    None
}

/// `<ppid> <command path>` as `ps -o ppid=,comm=` prints it: the parent's pid and
/// the process as a [`Link`]. The path can hold spaces, which is why the name is
/// taken from its last component and not from the first word.
fn parse_parent_line(line: &str) -> Option<(u32, Link)> {
    let (ppid, comm) = line.trim().split_once(char::is_whitespace)?;
    let path = comm.trim();
    let name = path.rsplit('/').next()?;
    if name.is_empty() {
        return None;
    }
    Some((
        ppid.parse().ok()?,
        Link {
            name: name.to_string(),
            path: path.to_string(),
        },
    ))
}

/// `a <- b <- c <- launchd`: the chain as one line.
pub fn render_chain(chain: &[Link]) -> String {
    let names: Vec<&str> = chain.iter().map(|l| l.name.as_str()).collect();
    format!("{} <- launchd", names.join(" <- "))
}

/// Whether the process at the top of `chain` is an application — a binary inside
/// a `.app` bundle — as a terminal is.
///
/// macOS exempts a process from assessing new binaries by the application it was
/// started from (the terminal listed under System Settings > Privacy & Security >
/// Developer Tools). A chain that does not end in one has no application to be
/// covered by.
fn starts_from_an_application(chain: &[Link]) -> bool {
    chain
        .last()
        .is_some_and(|root| root.path.contains(".app/Contents/MacOS/"))
}

/// Whether the person running the gate has said to run it under a chain that
/// [`chain_warning`] complains about: `HANDOFF_ALLOW_SLOW_LAUNCHES` set to anything
/// but empty or `0`.
pub fn slow_launches_allowed() -> bool {
    allowed(std::env::var("HANDOFF_ALLOW_SLOW_LAUNCHES").ok().as_deref())
}

fn allowed(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// What to tell the person running the gate about the chain it runs under —
/// `None` when it starts from an application.
pub fn chain_warning(chain: &[Link]) -> Option<String> {
    if starts_from_an_application(chain) {
        return None;
    }
    Some(format!(
        "This process does not run under an application:
      {}
  The Developer Tools exemption (System Settings > Privacy & Security >
  Developer Tools) covers processes started from the terminal app you add, so it
  cannot apply to this chain. macOS then assesses each freshly linked binary
  before it first runs, one at a time, and every case the suite links pays for
  it. Run `cargo handoff` from that terminal.",
        render_chain(chain)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(path: &str) -> Link {
        Link {
            name: path.rsplit('/').next().unwrap().to_string(),
            path: path.to_string(),
        }
    }

    /// This session's chain when it ran in a background host.
    fn background_host() -> Vec<Link> {
        vec![
            link("/bin/bash"),
            link("/Users/d/.local/share/claude/versions/2.1.303"),
            link("/Users/d/.local/share/claude/versions/2.1.303"),
        ]
    }

    /// The chain of a process started from a terminal.
    fn from_a_terminal() -> Vec<Link> {
        vec![
            link("/Users/d/Projects/aipl/target/debug/handoff"),
            link("-bash"),
            link("/usr/bin/login"),
            link("/Users/d/Library/Application Support/iTerm2/iTermServer-3.4.23"),
            link("/Applications/iTerm.app/Contents/MacOS/iTerm2"),
        ]
    }

    #[test]
    fn slow_launches_are_allowed_only_when_asked_for() {
        assert!(!allowed(None));
        assert!(!allowed(Some("")));
        assert!(!allowed(Some("0")));
        assert!(allowed(Some("1")));
    }

    #[test]
    fn a_parent_line_gives_the_pid_and_the_process() {
        assert_eq!(
            parse_parent_line("  4015 /Users/d/.local/share/claude/versions/2.1.303\n"),
            Some((4015, link("/Users/d/.local/share/claude/versions/2.1.303")))
        );
        // The path can hold spaces; only the last component is the name.
        let (ppid, parsed) = parse_parent_line(
            "97469 /Users/d/Library/Application Support/iTerm2/iTermServer-3.4.23",
        )
        .unwrap();
        assert_eq!(ppid, 97469);
        assert_eq!(parsed.name, "iTermServer-3.4.23");
        assert_eq!(
            parsed.path,
            "/Users/d/Library/Application Support/iTerm2/iTermServer-3.4.23"
        );
        // A login shell is spelled with a leading dash, and that is its name.
        assert_eq!(parse_parent_line("1234 -bash").unwrap().1.name, "-bash");
    }

    #[test]
    fn a_line_that_is_not_a_parent_line_gives_nothing() {
        assert_eq!(parse_parent_line(""), None);
        assert_eq!(parse_parent_line("   \n"), None);
        assert_eq!(parse_parent_line("notapid bash"), None);
        assert_eq!(parse_parent_line("1234"), None);
    }

    #[test]
    fn a_chain_reads_as_one_line_ending_at_launchd() {
        assert_eq!(
            render_chain(&from_a_terminal()),
            "handoff <- -bash <- login <- iTermServer-3.4.23 <- iTerm2 <- launchd"
        );
    }

    #[test]
    fn a_chain_from_a_terminal_has_nothing_to_warn_about() {
        assert_eq!(chain_warning(&from_a_terminal()), None);
    }

    #[test]
    fn a_chain_that_ends_in_a_background_host_is_named() {
        assert_eq!(
            chain_warning(&background_host()).as_deref(),
            Some(
                "This process does not run under an application:
      bash <- 2.1.303 <- 2.1.303 <- launchd
  The Developer Tools exemption (System Settings > Privacy & Security >
  Developer Tools) covers processes started from the terminal app you add, so it
  cannot apply to this chain. macOS then assesses each freshly linked binary
  before it first runs, one at a time, and every case the suite links pays for
  it. Run `cargo handoff` from that terminal."
            )
        );
    }

    #[test]
    fn only_the_root_decides_whether_a_chain_starts_from_an_application() {
        // An application further down the chain is not where it starts.
        let mut chain = from_a_terminal();
        chain.push(link("/Users/d/.local/share/claude/versions/2.1.303"));
        assert!(!starts_from_an_application(&chain));
        // A `.app` path that is not the bundle's executable is not an application.
        assert!(!starts_from_an_application(&[link(
            "/Applications/iTerm.app/Contents/Resources/helper"
        )]));
        assert!(!starts_from_an_application(&[]));
    }

    #[test]
    fn this_process_has_a_chain_of_named_links() {
        let chain = ancestry(std::process::id()).expect("ps could describe this process");
        assert!(chain.iter().all(|l| !l.name.is_empty()), "{chain:?}");
    }
}
