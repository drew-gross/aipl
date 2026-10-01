//! The AIPL language server: go-to-definition, hover, the document outline,
//! formatting, and diagnostics, over the Language Server Protocol.
//!
//! It exists so that an editor asks the *compiler* where a name is defined
//! rather than guessing from a regular expression. Everything it answers is
//! something the compiler already computes — [`aipl_index`] for the symbols
//! and imports, [`aipl_fmt`] for the canonical layout, the loader and
//! [`aipl_codegen::frontend`] for the diagnostics — so there is one answer to
//! each question and the editor's agrees with `aipl check` by construction.
//!
//! # Shape
//!
//! | module | what it is |
//! |---|---|
//! | [`jsonrpc`] | the base protocol: `Content-Length`-framed JSON |
//! | [`text`] | byte offsets ↔ line/character, paths ↔ `file://` URIs |
//! | [`server`] | the requests, and the open documents they are answered against |
//! | [`serve_stdio`] | the loop that wires those to a pipe, and the diagnostics worker |
//!
//! [`Server`] does no I/O of its own: `handle` takes a JSON value and returns
//! what to send. That is what makes the protocol testable without a
//! subprocess, and it is why the threading lives out here instead.
//!
//! # Why diagnostics get a thread
//!
//! Answering "where is this defined" costs one parse of one file — under a
//! millisecond — so it happens on the reading thread while the editor waits.
//! Type-checking costs the compiler's whole frontend over the import tree,
//! which on a file the size of `walker.aipl` (5200 lines) was measured at
//! **11 seconds**. That is too much to pay per keystroke and far too much to
//! make the editor wait for, so a change only *notes* that a document is
//! stale and a worker picks it up once the typing stops — see [`DEBOUNCE`].
//!
//! The worker never holds the server while it checks: it copies the document
//! out ([`Server::snapshot`]) and releases the lock, so those 11 seconds cost
//! the diagnostics and nothing else. Hover and go-to-definition keep answering
//! throughout. This is the one piece of the design that is about the
//! compiler's speed rather than the protocol, and it is why
//! [`server::diagnostics`] takes a snapshot instead of a `&Server`.
//!
//! While you are actually typing the file usually does not parse, and a parse
//! error comes back from the loader immediately — so the slow path is the one
//! that runs when the program is already well-formed.

pub mod jsonrpc;
pub mod server;
pub mod text;

use std::collections::BTreeSet;
use std::io::{self, Stdout};
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

pub use server::{Reaction, Server};

/// How long the typing has to stop before diagnostics are recomputed.
///
/// Long enough that a burst of keystrokes costs one frontend run rather than
/// one per character; short enough that it still feels like a response to what
/// you just typed.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// Stack for the diagnostics worker, matching what the CLI gives its own
/// worker: codegen and the checker recurse per AST node, deep enough to
/// overflow a default thread stack on a moderately sized program.
const WORKER_STACK: usize = 256 * 1024 * 1024;

/// Serve the protocol on stdin/stdout until the client exits or closes the
/// pipe. This is what `aipl lsp` runs.
///
/// Installs the parser hooks first: the parser reaches its dogfooded AIPL
/// through them and has no native fallback, and a server is a host — it owns
/// the process it is given.
pub fn serve_stdio() -> io::Result<()> {
    aipl_codegen::install_parser_hooks();

    let server = Arc::new(Mutex::new(Server::new()));
    let output = Arc::new(Mutex::new(io::stdout()));
    let (stale, pending) = mpsc::channel();

    let worker = std::thread::Builder::new()
        .name("aipl-lsp diagnostics".to_string())
        .stack_size(WORKER_STACK)
        .spawn({
            let server = Arc::clone(&server);
            let output = Arc::clone(&output);
            move || publish_diagnostics(pending, &server, &output)
        })?;

    let input = io::stdin();
    let mut input = input.lock();
    while let Some(message) = jsonrpc::read_message(&mut input)? {
        let reaction = handle(&server, &message);
        if let Some(reply) = reaction.message {
            send(&output, &reply)?;
        }
        if let Some(uri) = reaction.recheck {
            // The worker only ends when `stale` is dropped, so a send can fail
            // only if it panicked — in which case diagnostics are over but
            // navigation still works, and dropping the request is right.
            let _ = stale.send(uri);
        }
        if reaction.exit {
            break;
        }
    }

    // Closing the channel is what tells the worker to finish.
    drop(stale);
    let _ = worker.join();
    Ok(())
}

/// Handle one message, surviving a panic in the compiler.
///
/// A language server that dies on a malformed program takes every feature with
/// it until the editor is restarted, and the program is malformed for most of
/// the time anyone is typing. So a panicking pass costs its own request and
/// nothing else: the client is told the request failed, and the next one is
/// answered normally.
///
/// The panic message still reaches stderr, where the editor's log keeps it.
fn handle(server: &Mutex<Server>, message: &Value) -> Reaction {
    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| lock(server).handle(message)));
    match caught {
        Ok(reaction) => reaction,
        // A request is waiting for an answer and must get one. A notification
        // is not, so there is nothing to say.
        Err(_) => match message.get("id") {
            Some(id) => Reaction {
                message: Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": server::INTERNAL_ERROR,
                        "message": "the compiler panicked handling this request; see the server log",
                    },
                })),
                ..Reaction::default()
            },
            None => Reaction::default(),
        },
    }
}

/// Recompute and publish diagnostics for whatever has gone stale, once the
/// edits stop arriving.
///
/// Each batch is collected into a set, so a document edited twenty times while
/// the worker was busy is checked once.
fn publish_diagnostics(pending: Receiver<String>, server: &Mutex<Server>, output: &Mutex<Stdout>) {
    while let Ok(first) = pending.recv() {
        let mut stale = BTreeSet::new();
        stale.insert(first);
        // Keep collecting until nothing has arrived for `DEBOUNCE`. A
        // disconnect ends the wait too, and the batch in hand is still worth
        // publishing — the outer `recv` then ends the loop.
        while let Ok(next) = pending.recv_timeout(DEBOUNCE) {
            stale.insert(next);
        }
        for uri in stale {
            // Copy the document out, then let go of the server: the check
            // below is the whole frontend, and a request arriving mid-check
            // must not wait for it.
            let Some(snapshot) = lock(server).snapshot(&uri) else {
                continue;
            };
            // A panic in the frontend costs this document's diagnostics, not
            // the worker: the next edit gets another try.
            let Ok(message) =
                std::panic::catch_unwind(AssertUnwindSafe(|| server::diagnostics(&snapshot)))
            else {
                continue;
            };
            if send(output, &message).is_err() {
                // The client is gone. Nothing left to publish to.
                return;
            }
        }
    }
}

/// Take a lock, ignoring poisoning.
///
/// A panic while the lock was held leaves it poisoned, and a server that then
/// refused every subsequent request would turn one bad keystroke into a dead
/// session. What the lock guards is the open documents — text the editor will
/// resend on the next change — so there is no torn invariant to protect.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn send(output: &Mutex<Stdout>, message: &Value) -> io::Result<()> {
    let mut output = lock(output);
    jsonrpc::write_message(&mut *output, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worker publishes one batch per pause, not one per notification.
    #[test]
    fn coalesces_a_burst_of_edits() {
        let (stale, pending) = mpsc::channel();
        for _ in 0..5 {
            stale.send("file:///x.aipl".to_string()).expect("send");
        }
        drop(stale);
        let mut batches = 0;
        while let Ok(first) = pending.recv() {
            let mut set = BTreeSet::new();
            set.insert(first);
            while let Ok(next) = pending.recv_timeout(DEBOUNCE) {
                set.insert(next);
            }
            assert_eq!(set.len(), 1, "five edits to one document are one check");
            batches += 1;
        }
        assert_eq!(batches, 1);
    }
}
