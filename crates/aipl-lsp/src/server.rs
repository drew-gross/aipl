//! The requests this server answers, and the open documents it answers them
//! against. Everything here is a pure function of a message and the document
//! store — no I/O beyond reading imported files — so the whole protocol
//! surface is testable by handing [`Server::handle`] a JSON value.
//!
//! Each answer is a thin adapter over something the compiler already computes:
//!
//! | request | answered by |
//! |---|---|
//! | `textDocument/definition` | the name resolutions the type checker recorded for the document ([`crate::navigation`]) |
//! | `textDocument/references` | the same resolutions, from every open document's analysis and from the analysis of each other file in the workspace that mentions the name |
//! | `textDocument/hover` | [`aipl_index::Symbol`]'s `detail` and `doc`, for a builtin from [`aipl_index::builtin`] |
//! | `textDocument/documentSymbol` | [`aipl_index::FileIndex::outline`] |
//! | `textDocument/formatting` | [`aipl_codegen::format_source`] |
//! | `textDocument/publishDiagnostics` | the loader and [`aipl_codegen::frontend`] |
//!
//! There is deliberately no second implementation of anything: a server that
//! resolved names its own way would answer a question the compiler does not
//! ask, and the two would drift.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use aipl_index::SymbolKind;
use aipl_syntax::{DebugOptions, Error};
use serde_json::{json, Value};

use crate::analysis::Analyses;
use crate::navigation::{Navigation, Site, Under};
use crate::text::{path_to_uri, uri_to_path, LineIndex};
use crate::workspace::{aipl_files, mentions_word};

/// One open document, as the editor last told us it reads.
struct Document {
    /// The file on disk this buffer belongs to, or `None` for a buffer that
    /// has no file yet (`untitled:`) — which is what makes imports, and so
    /// diagnostics, unanswerable for it.
    path: Option<PathBuf>,
    text: String,
    /// The editor's version counter, echoed back on the diagnostics for this
    /// document so a client can drop a batch that an edit has already
    /// invalidated.
    version: Option<i64>,
}

/// What the type checker made of one document, for the text it was given.
pub(crate) enum Typed {
    /// The document compiled; [`Navigation`] holds what its names resolve to.
    Ready {
        text: String,
        navigation: Navigation,
    },
    /// It did not, and this is the first thing the compiler said about it.
    Broken { text: String, message: String },
}

impl Typed {
    /// The text this was computed from.
    fn text(&self) -> &str {
        match self {
            Typed::Ready { text, .. } | Typed::Broken { text, .. } => text,
        }
    }
}

fn not_analysed() -> String {
    "the compiler has no analysis of the current text of this document".to_string()
}

/// A declaration site as an LSP `Location`, with its range taken against the
/// source the navigation was built from.
fn location(navigation: &Navigation, site: &Site) -> Value {
    let source = navigation.source(&site.file).unwrap_or_default();
    json!({
        "uri": path_to_uri(&site.file),
        "range": LineIndex::new(source).range(&site.span),
    })
}

/// What handling one message asks the transport to do. Returned rather than
/// acted on so that [`Server`] does no I/O and a test can read the decision.
#[derive(Default)]
pub struct Reaction {
    /// A message to write back — a response when the client sent a request,
    /// or a notification the handler produced on its own.
    pub message: Option<Value>,
    /// The URI whose diagnostics are now stale. The transport debounces these;
    /// the server only says that something changed.
    pub recheck: Option<String>,
    /// The client sent `exit`.
    pub exit: bool,
}

/// The open documents, and the answers derived from them.
#[derive(Default)]
pub struct Server {
    documents: HashMap<String, Document>,
    /// What has been worked out about files, so a request about text it has seen
    /// does not do that work again.
    analyses: Analyses,
    /// What the type checker made of each open document, by URI — see
    /// [`Typed`]. Written by [`Server::record`] when the diagnostics worker
    /// finishes a check.
    typed: HashMap<String, Typed>,
    /// The directories the client opened as the workspace, which is what a
    /// request about the whole project searches.
    roots: Vec<PathBuf>,
    /// What the type checker made of workspace files that are not open, by
    /// path, kept for as long as every file each was built from reads as it did.
    scanned: HashMap<PathBuf, Navigation>,
    /// The diagnostics of a check a request ran itself, by URI, with the text
    /// they are for: not yet published, and no longer worth computing again.
    /// Dropped by anything that makes the check stale — see
    /// [`Server::take_checked`].
    checked_on_demand: HashMap<String, (String, Value)>,
}

impl Server {
    pub fn new() -> Server {
        Server::default()
    }

    /// Handle one incoming message.
    ///
    /// Every *request* (a message with an `id`) gets a reply, including one
    /// this server does not implement: a client that asked is blocked until it
    /// hears back, so silence would read as a hang rather than as a decline.
    /// A *notification* it does not implement is ignored, which is what the
    /// protocol asks for.
    pub fn handle(&mut self, message: &Value) -> Reaction {
        let method = message["method"].as_str().unwrap_or_default();
        let id = message.get("id").cloned();
        let params = &message["params"];
        match method {
            "initialize" => {
                self.roots = workspace_roots(params);
                reply(id, initialize_result())
            }
            "shutdown" => reply(id, Value::Null),
            "exit" => Reaction {
                exit: true,
                ..Reaction::default()
            },
            "textDocument/didOpen" => {
                let document = &params["textDocument"];
                self.open(document);
                recheck(uri_of(document))
            }
            "textDocument/didChange" => {
                let uri = uri_of(&params["textDocument"]);
                self.change(&uri, params);
                recheck(uri)
            }
            // A save changes no text, but it does change what every *other*
            // file sees: this buffer's imports now resolve to what was just
            // written. Rechecking is the cheapest way to say so.
            "textDocument/didSave" => {
                let uri = uri_of(&params["textDocument"]);
                self.checked_on_demand.remove(&uri);
                recheck(uri)
            }
            "textDocument/didClose" => {
                let uri = uri_of(&params["textDocument"]);
                self.documents.remove(&uri);
                self.typed.remove(&uri);
                self.checked_on_demand.remove(&uri);
                // Diagnostics belong to the server until it says otherwise, so
                // a closed document's have to be withdrawn explicitly or the
                // editor keeps showing them.
                Reaction {
                    message: Some(publish_diagnostics(&uri, None, Vec::new())),
                    ..Reaction::default()
                }
            }
            "textDocument/definition" => answer(id, self.definition(params)),
            "textDocument/references" => answer(id, self.references(params)),
            "textDocument/hover" => reply(id, self.hover(params)),
            "textDocument/documentSymbol" => reply(id, self.document_symbols(params)),
            "textDocument/formatting" => reply(id, self.formatting(params)),
            _ => match id {
                Some(id) => Reaction {
                    message: Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": METHOD_NOT_FOUND, "message": format!("unsupported request {method:?}") },
                    })),
                    ..Reaction::default()
                },
                None => Reaction::default(),
            },
        }
    }

    /// Take a copy of one document to check, so that the check — which is the
    /// compiler frontend, and on a large file takes seconds — runs without
    /// holding anything the next request needs. See [`diagnostics`].
    ///
    /// `None` for a document this server is not holding, and for one with no
    /// file on disk: an unsaved buffer has no directory for its imports to
    /// resolve against, and a run that could not find them would report
    /// missing-import errors that say nothing about the code.
    pub fn snapshot(&self, uri: &str) -> Option<Snapshot> {
        let document = self.documents.get(uri)?;
        Some(Snapshot {
            uri: uri.to_string(),
            path: document.path.clone()?,
            text: document.text.clone(),
            version: document.version,
        })
    }

    /// Snapshot a document and check it in one step — what a caller with
    /// nothing else to do wants, and what the tests use. The transport splits
    /// the two instead; see [`Server::snapshot`].
    pub fn diagnostics(&mut self, uri: &str) -> Option<Value> {
        let analysis = analyse(&self.snapshot(uri)?);
        Some(self.record(analysis))
    }

    /// Keep what a check found out about a document, and hand back the
    /// diagnostics message to publish. The check itself ran without the server;
    /// this is the one step that needs it.
    pub(crate) fn record(&mut self, analysis: Analysis) -> Value {
        self.typed.insert(analysis.uri, analysis.typed);
        analysis.message
    }

    fn open(&mut self, document: &Value) {
        let uri = uri_of(document);
        self.checked_on_demand.remove(&uri);
        self.documents.insert(
            uri.clone(),
            Document {
                path: uri_to_path(&uri),
                text: document["text"].as_str().unwrap_or_default().to_string(),
                version: document["version"].as_i64(),
            },
        );
    }

    /// Apply a change notification. This server syncs whole documents (it says
    /// so in `initialize`), so each change carries the new text entire and
    /// there is no edit to apply — only the last one counts.
    fn change(&mut self, uri: &str, params: &Value) {
        self.checked_on_demand.remove(uri);
        let Some(document) = self.documents.get_mut(uri) else {
            return;
        };
        if let Some(text) = params["contentChanges"]
            .as_array()
            .and_then(|changes| changes.last())
            .and_then(|change| change["text"].as_str())
        {
            document.text = text.to_string();
        }
        document.version = params["textDocument"]["version"].as_i64();
    }

    /// What the compiler resolved the name under the cursor to, with the
    /// document and position the request named — or the reason the compiler
    /// has no answer, which is an error rather than an empty result so that
    /// "this does not compile" is never mistaken for "nothing refers to this".
    /// See [`crate::navigation`].
    ///
    /// A document the compiler has not checked at its current text is checked
    /// now ([`Server::ensure_analysed`]) rather than refused.
    fn under(&mut self, params: &Value) -> Result<(Under, PathBuf, String, usize), String> {
        let uri = uri_of(&params["textDocument"]);
        self.ensure_analysed(&uri);
        let Some((path, text, offset)) = locate(&self.documents, params) else {
            return Err(
                "this document has no file on disk, so its imports cannot be \
                        resolved — save it first"
                    .to_string(),
            );
        };
        let navigation = self.current_navigation(&uri, text)?;
        Ok((
            navigation.under(path, offset),
            path.to_path_buf(),
            text.to_string(),
            offset,
        ))
    }

    /// Check the document at `uri` if the compiler has not checked its current
    /// text, and keep what it found. The diagnostics worker does the same
    /// check in the background, a debounce after each edit; a request that
    /// arrives first does not wait for it to be scheduled, it runs the check
    /// itself, which takes as long as the frontend does on that file.
    fn ensure_analysed(&mut self, uri: &str) {
        let current = self
            .documents
            .get(uri)
            .zip(self.typed.get(uri))
            .is_some_and(|(document, typed)| document.text == typed.text());
        let Some(snapshot) = self.snapshot(uri).filter(|_| !current) else {
            return;
        };
        let analysis = on_large_stack("aipl-lsp analysis", || analyse(&snapshot));
        let text = snapshot.text.clone();
        let message = self.record(analysis);
        self.checked_on_demand.insert(snapshot.uri, (text, message));
    }

    /// The diagnostics a request already computed for the current text of `uri`,
    /// once: what the diagnostics worker publishes instead of checking the same
    /// text again. `None` when no request did, or when an edit, a save or a
    /// close has happened since — a save changes what the document's imports
    /// read as, so the check is worth redoing.
    pub fn take_checked(&mut self, uri: &str) -> Option<Value> {
        let (text, message) = self.checked_on_demand.remove(uri)?;
        (self.documents.get(uri)?.text == text).then_some(message)
    }

    /// The navigation of the document at `uri`, if it was built from exactly
    /// `text`.
    fn current_navigation(&self, uri: &str, text: &str) -> Result<&Navigation, String> {
        match self.typed.get(uri) {
            None => Err(not_analysed()),
            Some(typed) if typed.text() != text => Err(not_analysed()),
            Some(Typed::Broken { message, .. }) => Err(format!(
                "this document does not compile, so what its names refer to is unknown: {message}"
            )),
            Some(Typed::Ready { navigation, .. }) => Ok(navigation),
        }
    }

    /// The answer for a cursor the compiler resolved nothing at: nothing to
    /// say if it is not on a name, and an error if it is, because a name the
    /// compiler did not resolve is a name this server cannot honestly send
    /// anywhere.
    fn nothing_resolved(
        &mut self,
        path: &Path,
        text: &str,
        offset: usize,
    ) -> Result<Value, String> {
        match self.analyses.identifier_at(path, text, offset) {
            Some((name, _)) => Err(format!(
                "the compiler's name resolution does not cover `{name}` here: it is not a \
                 name that stands for a declaration written in a file (a primitive type, \
                 a keyword and the wildcard are not)"
            )),
            None => Ok(Value::Null),
        }
    }

    fn definition(&mut self, params: &Value) -> Result<Value, String> {
        let (under, path, text, offset) = self.under(params)?;
        match under {
            Under::Declared(sites) => {
                let navigation =
                    self.current_navigation(&uri_of(&params["textDocument"]), &text)?;
                // One place is a location; a name that is two things at once
                // (a shorthand field) is both.
                // A name that is also a declaration goes to what it reads, not to itself.
                let here = resolved(&path);
                let elsewhere: Vec<&Site> = sites
                    .iter()
                    .filter(|site| {
                        !(site.file == here && site.span.start <= offset && offset <= site.span.end)
                    })
                    .collect();
                let sites: Vec<&Site> = if elsewhere.is_empty() {
                    sites.iter().collect()
                } else {
                    elsewhere
                };
                let mut locations: Vec<Value> = sites
                    .iter()
                    .map(|site| location(navigation, site))
                    .collect();
                Ok(match locations.len() {
                    1 => locations.remove(0),
                    _ => Value::Array(locations),
                })
            }
            Under::Native(name) => Err(format!(
                "`{name}` is a builtin the compiler expands itself; it has no source to show"
            )),
            Under::Nothing => self.nothing_resolved(&path, &text, offset),
        }
    }

    /// Every place the name under the cursor is used.
    ///
    /// A use is a name the type checker resolved to the same declaration the
    /// cursor's name resolves to, so a field is never confused with a function
    /// that shares its spelling, and two structs with a field of the same name
    /// are told apart by the type the field is read off.
    ///
    /// Looks through the analysis of every open document, which covers the files
    /// each one imports, and then through every other file of the workspace that
    /// mentions the name (see [`Server::scan_workspace`]), so the list is the
    /// project's and not only what happens to be open. An open document whose
    /// analysis is missing, out of date or a compile error is an error here when
    /// it mentions the name, because its uses cannot be listed and a list without
    /// them would read as complete. A file that is not open and does not compile
    /// has no resolved names to list and is left out.
    fn references(&mut self, params: &Value) -> Result<Value, String> {
        let (under, path, text, offset) = self.under(params)?;
        let declarations = match under {
            Under::Declared(sites) => sites,
            Under::Native(name) => {
                return Err(format!(
                    "`{name}` is a builtin the compiler expands itself; its uses are not tracked"
                ))
            }
            Under::Nothing => return self.nothing_resolved(&path, &text, offset),
        };
        let include_declaration = params["context"]["includeDeclaration"]
            .as_bool()
            .unwrap_or(true);
        let uri = uri_of(&params["textDocument"]);
        let name = self
            .current_navigation(&uri, &text)?
            .source(&declarations[0].file)
            .and_then(|source| source.get(declarations[0].span.clone()))
            .unwrap_or_default()
            .to_string();
        let stale: Vec<String> = self
            .documents
            .iter()
            .filter(|(_, document)| mentions_word(&document.text, &name))
            .map(|(other_uri, _)| other_uri.clone())
            .collect();
        for other_uri in stale {
            self.ensure_analysed(&other_uri);
        }
        self.scan_workspace(&name);
        let here = self.current_navigation(&uri, &text)?;

        let mut locations: Vec<(PathBuf, usize, Value)> = Vec::new();
        let mut seen: HashSet<(PathBuf, usize, usize)> = HashSet::new();
        let mut add = |navigation: &Navigation, site: &Site| {
            if seen.insert((site.file.clone(), site.span.start, site.span.end)) {
                locations.push((
                    site.file.clone(),
                    site.span.start,
                    location(navigation, site),
                ));
            }
        };
        if include_declaration {
            for declaration in &declarations {
                add(here, declaration);
            }
        }
        for (other_uri, document) in &self.documents {
            match self.typed.get(other_uri) {
                Some(Typed::Ready { text, navigation }) if *text == document.text => {
                    for declaration in &declarations {
                        for site in navigation.uses_of(declaration) {
                            add(navigation, &site);
                        }
                    }
                }
                _ if !mentions_word(&document.text, &name) => {}
                // A file documenting a builtin holds no program, so it has no uses to list.
                _ if document
                    .path
                    .as_deref()
                    .is_some_and(crate::builtin_docs::is_documentation) => {}
                Some(Typed::Broken { text, message }) if *text == document.text => {
                    return Err(format!(
                        "cannot list every use of `{name}`: {other_uri} does not compile ({message})"
                    ))
                }
                _ => {
                    return Err(format!(
                        "cannot list every use of `{name}`: {other_uri} is still being analysed"
                    ))
                }
            }
        }
        for navigation in self.scanned.values() {
            for declaration in &declarations {
                for site in navigation.uses_of(declaration) {
                    add(navigation, &site);
                }
            }
        }
        locations.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        Ok(Value::Array(
            locations
                .into_iter()
                .map(|(_, _, location)| location)
                .collect(),
        ))
    }

    /// Type-check every workspace file that could use `name` and that no
    /// analysis already covers, keeping what each check found in
    /// [`Server::scanned`].
    ///
    /// A file is covered when an open document's analysis, or a kept one, was
    /// built from it: checking a program checks every file it imports, so one
    /// check often settles several. A file that does not mention `name` is never
    /// checked, and an open document is left to the open-document analysis, whose
    /// state is what [`Server::references`] reports on. Kept analyses are dropped
    /// when any file they were built from has changed.
    ///
    /// The check is the compiler's whole frontend, so it runs on a large stack
    /// ([`on_large_stack`]), and a file it panics on is left out like one that does
    /// not compile.
    fn scan_workspace(&mut self, name: &str) {
        let documents = &self.documents;
        let read = |path: &Path| source_in(documents, path);
        self.scanned
            .retain(|_, navigation| navigation.is_current(&read));
        let mut covered: HashSet<PathBuf> = self
            .scanned
            .values()
            .flat_map(|navigation| navigation.files().cloned())
            .collect();
        for (uri, typed) in &self.typed {
            if let (Typed::Ready { navigation, .. }, true) = (
                typed,
                documents
                    .get(uri)
                    .is_some_and(|document| document.text == typed.text()),
            ) {
                covered.extend(navigation.files().cloned());
            }
        }
        let open: HashSet<PathBuf> = documents
            .values()
            .filter_map(|document| document.path.as_deref().map(resolved))
            .collect();
        let candidates: Vec<(PathBuf, String)> = aipl_files(&self.roots)
            .iter()
            .map(|path| resolved(path))
            .filter(|path| !covered.contains(path) && !open.contains(path))
            .filter_map(|path| {
                let text = read(&path)?;
                mentions_word(&text, name).then_some((path, text))
            })
            .collect();
        if candidates.is_empty() {
            return;
        }
        let scanned = &mut self.scanned;
        let scan = || {
            for (path, text) in candidates {
                if covered.contains(&path) {
                    continue;
                }
                let checked =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&path, &text)));
                if let Ok(Ok(navigation)) = checked {
                    covered.extend(navigation.files().cloned());
                    scanned.insert(path, navigation);
                }
            }
        };
        on_large_stack("aipl-lsp workspace scan", scan);
    }

    fn hover(&mut self, params: &Value) -> Value {
        let Some((path, text, offset)) = locate(&self.documents, params) else {
            return Value::Null;
        };
        let Some((name, span)) = self.analyses.identifier_at(path, text, offset) else {
            return Value::Null;
        };
        let read = |imported: &Path| source_in(&self.documents, imported);
        let Some(found) = self.analyses.resolve(path, text, &name, offset, &read) else {
            return Value::Null;
        };
        let symbol = &found.symbol;
        // The signature as a code block, then the declaration's own `# ..`
        // lines as prose — the same two parts, in the same order, that `aipl
        // doc` prints and `aipl docs` renders.
        let mut markdown = format!("```aipl\n{}\n```", symbol.detail);
        if found.builtin {
            markdown.push_str("\n\nbuiltin");
        } else if let Some((target, _)) = &found.file {
            if !same_file(target, path) {
                let file = target.file_name().unwrap_or(target.as_os_str());
                markdown.push_str(&format!("\n\nfrom `{}`", file.to_string_lossy()));
            }
        }
        if let Some(doc) = &symbol.doc {
            markdown.push_str("\n\n");
            markdown.push_str(doc);
        }
        // The documented parameters of a function, each with its own lines, as the docs page
        // lists them.
        for slot in &symbol.slots {
            if let Some(doc) = &slot.doc {
                markdown.push_str(&format!(
                    "\n\n- `{}` — {}",
                    slot.detail,
                    doc.split_whitespace().collect::<Vec<_>>().join(" ")
                ));
            }
        }
        json!({
            "contents": { "kind": "markdown", "value": markdown },
            "range": LineIndex::new(text).range(&span),
        })
    }

    /// The document outline: every top-level declaration in source order, with
    /// a `variant`'s cases nested under it.
    fn document_symbols(&mut self, params: &Value) -> Value {
        let Some((path, text, _)) = locate_document(&self.documents, params) else {
            return Value::Null;
        };
        // A document mid-edit does not parse for most of the time anyone is
        // looking at it, so this is the outline of the last text that did —
        // laid out against that text — rather than an empty one that flickers
        // in and out as you type. No parse yet at all is an empty outline: the
        // alternative is an error popup per keystroke.
        let Some((file, indexed)) = self.analyses.file(path, text) else {
            return json!([]);
        };
        let lines = LineIndex::new(indexed);
        let mut outline: Vec<Value> = Vec::new();
        for symbol in file.outline() {
            let range = lines.range(&symbol.name_span);
            let node = json!({
                "name": symbol.name,
                "detail": symbol.detail,
                "kind": symbol_kind(symbol.kind),
                "range": range,
                "selectionRange": range,
                "children": [],
            });
            // A case follows its variant in the outline, so the variant is
            // whatever was added last. Its range has to grow to cover the
            // case, because a client may discard a child that its parent does
            // not contain.
            match (symbol.parent.is_some(), outline.last_mut()) {
                (true, Some(parent)) => {
                    parent["range"]["end"] = node["range"]["end"].clone();
                    parent["children"]
                        .as_array_mut()
                        .expect("every node is built with a children array")
                        .push(node);
                }
                _ => outline.push(node),
            }
        }
        Value::Array(outline)
    }

    /// Format the whole document, as one edit replacing all of it.
    ///
    /// The formatter is canonical, so there is nothing to negotiate with the
    /// client's own settings: tab size and insert-spaces arrive in `options`
    /// and are ignored, exactly as the formatter ignores them.
    fn formatting(&self, params: &Value) -> Value {
        let Some((_, text, _)) = locate_document(&self.documents, params) else {
            return Value::Null;
        };
        let Ok(formatted) = aipl_codegen::format_source(text, aipl_codegen::FORMAT_WIDTH) else {
            // Unparsable source has no canonical form. Declining leaves the
            // buffer alone, which is what a format-on-save user needs: the
            // alternative is losing the text that was mid-edit.
            return Value::Null;
        };
        if formatted == text {
            // Already canonical. An empty edit list is not the same as a
            // no-op edit: it leaves the undo history untouched.
            return json!([]);
        }
        let lines = LineIndex::new(text);
        json!([{
            "range": { "start": lines.position(0), "end": lines.position(text.len()) },
            "newText": formatted,
        }])
    }
}

/// Run `work` on a thread with the stack the diagnostics worker has, because the
/// compiler's frontend recurses per AST node and overflows a default stack on a
/// moderately sized program. A panic in `work` is raised again here, where the
/// request handler catches it.
fn on_large_stack<T: Send>(name: &str, work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name(name.to_string())
            .stack_size(crate::WORKER_STACK)
            .spawn_scoped(scope, work)
            .expect("spawn a thread for the compiler")
            .join()
    })
    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// The directories the client opened as the workspace: its folders, or the
/// single root of an older client.
fn workspace_roots(params: &Value) -> Vec<PathBuf> {
    let folders: Vec<&str> = params["workspaceFolders"]
        .as_array()
        .map(|folders| folders.iter().filter_map(|f| f["uri"].as_str()).collect())
        .unwrap_or_default();
    let uris = if folders.is_empty() {
        params["rootUri"].as_str().into_iter().collect()
    } else {
        folders
    };
    uris.into_iter().filter_map(uri_to_path).collect()
}

/// The document a request names, and the byte offset of its `position`.
fn locate<'a>(
    documents: &'a HashMap<String, Document>,
    params: &Value,
) -> Option<(&'a Path, &'a str, usize)> {
    let (path, text, _) = locate_document(documents, params)?;
    let position = &params["position"];
    let line = position["line"].as_u64()? as u32;
    let character = position["character"].as_u64()? as u32;
    Some((path, text, LineIndex::new(text).offset(line, character)))
}

/// The document a request names. The third element is always 0 — it is there so
/// this shares a shape with [`locate`] and the callers that want no position can
/// ignore it.
fn locate_document<'a>(
    documents: &'a HashMap<String, Document>,
    params: &Value,
) -> Option<(&'a Path, &'a str, usize)> {
    let document = documents.get(uri_of(&params["textDocument"]).as_str())?;
    Some((document.path.as_deref()?, document.text.as_str(), 0))
}

/// The current text of a file: the open buffer if there is one, otherwise what
/// is on disk. The buffer wins because it is what the author is looking at —
/// jumping to a definition they just typed and have not saved should land on it,
/// not where it used to be.
fn source_in(documents: &HashMap<String, Document>, path: &Path) -> Option<String> {
    for document in documents.values() {
        let Some(open) = &document.path else {
            continue;
        };
        if same_file(open, path) {
            return Some(document.text.clone());
        }
    }
    std::fs::read_to_string(path).ok()
}

/// A path with its `.` components and symlinks resolved, or unchanged when it
/// names nothing on disk — which is the case for a buffer that has not been
/// saved, and not a reason to refuse to answer about it.
pub(crate) fn resolved(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether two paths name the same file.
///
/// Spelling is not enough: an import writes a relative path, the editor sends
/// an absolute one, and on macOS the same directory is reachable as both
/// `/tmp/x` and `/private/tmp/x`. Comparing resolved paths answers the
/// question that was meant; comparing the spellings answers a different one
/// that happens to agree most of the time.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        // One of them is not on disk, so there is nothing to resolve and the
        // spelling is all there is.
        _ => a == b,
    }
}

const REQUEST_FAILED: i64 = -32803;
/// JSON-RPC's code for a method the server does not implement.
const METHOD_NOT_FOUND: i64 = -32601;

/// JSON-RPC's code for a failure inside the server — what a panicking compiler
/// pass is reported as, so the client stops waiting. See `crate::serve_stdio`.
pub(crate) const INTERNAL_ERROR: i64 = -32603;

/// What this server can do, in the shape `initialize` answers with.
fn initialize_result() -> Value {
    json!({
        "capabilities": {
            // The protocol's default, stated rather than assumed: every
            // position this server emits counts UTF-16 code units.
            "positionEncoding": "utf-16",
            "textDocumentSync": {
                "openClose": true,
                // 1 = full: each change carries the whole document. Nothing
                // here is incremental — the index re-parses one file per
                // request, which is cheaper than maintaining an edit log — so
                // asking for incremental changes would only add a way to be
                // out of step with the editor.
                "change": 1,
                "save": true,
            },
            "definitionProvider": true,
            "referencesProvider": true,
            "hoverProvider": true,
            "documentSymbolProvider": true,
            "documentFormattingProvider": true,
        },
        "serverInfo": { "name": "aipl-lsp", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn reply(id: Option<Value>, result: Value) -> Reaction {
    // A response needs the id it answers. A request without one is malformed,
    // and there is nowhere to send the complaint.
    let Some(id) = id else {
        return Reaction::default();
    };
    Reaction {
        message: Some(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
        ..Reaction::default()
    }
}

/// A reply that is the result, or a failed request carrying the reason. The
/// client shows the reason, which is how "still analysing" and "does not
/// compile" reach a person as themselves rather than as an empty answer.
fn answer(id: Option<Value>, result: Result<Value, String>) -> Reaction {
    match result {
        Ok(result) => reply(id, result),
        Err(message) => {
            let Some(id) = id else {
                return Reaction::default();
            };
            Reaction {
                message: Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": REQUEST_FAILED, "message": message },
                })),
                ..Reaction::default()
            }
        }
    }
}

fn recheck(uri: String) -> Reaction {
    Reaction {
        recheck: Some(uri),
        ..Reaction::default()
    }
}

fn uri_of(document: &Value) -> String {
    document["uri"].as_str().unwrap_or_default().to_string()
}

fn publish_diagnostics(uri: &str, version: Option<i64>, diagnostics: Vec<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "version": version, "diagnostics": diagnostics },
    })
}

/// One document as it stood when its diagnostics were asked for.
///
/// It is a copy rather than a borrow because checking it takes long enough
/// that nothing else should be waiting on it — the editor is free to replace
/// the document underneath, and this run simply describes the version it was
/// given, which is what the `version` it publishes says.
pub struct Snapshot {
    uri: String,
    path: PathBuf,
    text: String,
    version: Option<i64>,
}

/// What checking one snapshot produced: the diagnostics to publish, and what the
/// checker made of the text for [`Server::record`] to keep.
pub(crate) struct Analysis {
    uri: String,
    message: Value,
    typed: Typed,
}

/// The `textDocument/publishDiagnostics` notification for a snapshot:
/// everything the loader and the type checker report against it.
///
/// Takes no `&Server`, so the caller can let go of the server first. That
/// matters: the frontend over a few thousand lines of AIPL takes seconds, and
/// go-to-definition has to keep answering while it does.
pub(crate) fn analyse(snapshot: &Snapshot) -> Analysis {
    let lines = LineIndex::new(&snapshot.text);
    if crate::builtin_docs::is_documentation(&snapshot.path) {
        return Analysis {
            uri: snapshot.uri.clone(),
            message: publish_diagnostics(&snapshot.uri, snapshot.version, Vec::new()),
            typed: Typed::Broken {
                text: snapshot.text.clone(),
                message: "this file documents a builtin; it is not a program".to_string(),
            },
        };
    }
    let (found, typed) = match check(&snapshot.path, &snapshot.text) {
        Ok(navigation) => (
            Vec::new(),
            Typed::Ready {
                text: snapshot.text.clone(),
                navigation,
            },
        ),
        Err(errors) => (
            errors
                .iter()
                .map(|error| diagnostic(&lines, error))
                .collect(),
            Typed::Broken {
                text: snapshot.text.clone(),
                message: errors
                    .first()
                    .map(|error| error.message.clone())
                    .unwrap_or_default(),
            },
        ),
    };
    Analysis {
        uri: snapshot.uri.clone(),
        message: publish_diagnostics(&snapshot.uri, snapshot.version, found),
        typed,
    }
}

/// [`analyse`], keeping only the diagnostics to publish: what a caller with no
/// server to record into wants.
pub fn diagnostics(snapshot: &Snapshot) -> Value {
    analyse(snapshot).message
}

/// Load the document and type-check it, collecting every finding.
///
/// This is the compiler's own frontend, not a second opinion about the same
/// source: the loader resolves the imports and runs the lints, and
/// [`aipl_codegen::frontend`] runs the checker. What it skips is everything
/// after the checker — monomorphization, optimization, Cranelift — none of
/// which reports anything an editor can point at, and all of which costs more
/// than the rest put together.
fn check(path: &Path, text: &str) -> Result<Navigation, Vec<Error>> {
    let debug = DebugOptions::new(false);
    let program = aipl_loader::load_program_overlay(path, None, text, debug)?;
    let frontend = aipl_codegen::frontend(&program)?;
    // The `.test` blocks are checked as functions of their own when tests run, and
    // are names the author writes like any other, so they are part of what the
    // compiler resolves here.
    let tests = aipl_codegen::frontend(&aipl_codegen::build_test_program(&program))?;
    Ok(Navigation::new(&[&frontend, &tests], path, text))
}

/// One compiler [`Error`] as an LSP diagnostic.
///
/// An error raised against an *imported* file carries that file's source with
/// it, so its span indexes text this document does not have. Rather than point
/// at an unrelated line, those are reported at the top of the document with the
/// other file named in the message — which is also how `aipl check` reads.
fn diagnostic(lines: &LineIndex, error: &Error) -> Value {
    let (message, range, notes) = match (&error.origin, &error.span) {
        (Some(origin), _) => (
            format!("{}: {}", origin.label, error.message),
            lines.empty_range_at(0),
            &[][..],
        ),
        (None, Some(span)) => (
            error.message.clone(),
            lines.range(span),
            error.notes.as_slice(),
        ),
        (None, None) => (
            error.message.clone(),
            lines.empty_range_at(0),
            error.notes.as_slice(),
        ),
    };
    json!({
        "range": range,
        // Everything the compiler reports stops the build, so everything it
        // reports is an error. There is no warning level to map: a lint hit
        // fails the load too.
        "severity": 1,
        "source": "aipl",
        "message": message,
        "relatedInformation": notes
            .iter()
            .map(|(note, span)| json!({
                "location": { "uri": Value::Null, "range": lines.range(span) },
                "message": note,
            }))
            .collect::<Vec<_>>(),
    })
}

/// An AIPL declaration as one of the kinds the protocol defines, which is what
/// decides the icon an outline draws beside it.
fn symbol_kind(kind: SymbolKind) -> i64 {
    match kind {
        SymbolKind::Function => 12,
        SymbolKind::Struct => 23,
        // A `variant` is a sum type and its cases are its members, so the pair
        // the protocol has for exactly that — Enum and EnumMember — is the
        // right one even though AIPL's cases carry payloads.
        SymbolKind::Variant => 10,
        SymbolKind::Case => 22,
        SymbolKind::Constant => 14,
    }
}
