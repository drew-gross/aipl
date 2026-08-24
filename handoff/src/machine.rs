//! The part of the gate that is about the *machine* rather than the tests:
//! walking a process tree, killing one, and answering "is this hung, or is the
//! box just on its knees?".
//!
//! Everything here is best-effort and shells out to the platform tools
//! (`pgrep`, `ps`, `vm_stat`, `free`). It is only ever consulted on a timeout —
//! a path that has already given up — so a missing tool degrades to a quieter
//! message rather than an error.

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
