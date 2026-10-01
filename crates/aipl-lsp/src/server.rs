//! The requests this server answers, and the open documents it answers them
//! against. Everything here is a pure function of a message and the document
//! store — no I/O beyond reading imported files — so the whole protocol
//! surface is testable by handing [`Server::handle`] a JSON value.
//!
//! Each answer is a thin adapter over something the compiler already computes:
//!
//! | request | answered by |
//! |---|---|
//! | `textDocument/definition` | [`aipl_index::Index::definition_of`] |
//! | `textDocument/hover` | [`aipl_index::Symbol`]'s `detail` and `doc` |
//! | `textDocument/documentSymbol` | [`aipl_index::FileIndex::outline`] |
//! | `textDocument/formatting` | [`aipl_fmt::format_source`] |
//! | `textDocument/publishDiagnostics` | the loader and [`aipl_codegen::frontend`] |
//!
//! There is deliberately no second implementation of anything: a server that
//! resolved names its own way would answer a question the compiler does not
//! ask, and the two would drift.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aipl_index::{Index, Symbol, SymbolKind};
use aipl_syntax::{DebugOptions, Error, Span};
use serde_json::{json, Value};

use crate::text::{path_to_uri, uri_to_path, LineIndex};

/// The lexer scope every identifier token carries — the rule in
/// `lex_aipl.aipl` that matches a name. Turning a cursor position into a name
/// is a lexical question, so it is asked of the lexer rather than re-derived
/// from character classes here. `identifier_scope_is_current` pins it.
const IDENTIFIER_SCOPE: &str = "variable.other.aipl";

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
            "initialize" => reply(id, initialize_result()),
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
            "textDocument/didSave" => recheck(uri_of(&params["textDocument"])),
            "textDocument/didClose" => {
                let uri = uri_of(&params["textDocument"]);
                self.documents.remove(&uri);
                // Diagnostics belong to the server until it says otherwise, so
                // a closed document's have to be withdrawn explicitly or the
                // editor keeps showing them.
                Reaction {
                    message: Some(publish_diagnostics(&uri, None, Vec::new())),
                    ..Reaction::default()
                }
            }
            "textDocument/definition" => reply(id, self.definition(params)),
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
    pub fn diagnostics(&self, uri: &str) -> Option<Value> {
        Some(diagnostics(&self.snapshot(uri)?))
    }

    fn open(&mut self, document: &Value) {
        let uri = uri_of(document);
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

    fn definition(&self, params: &Value) -> Value {
        let Some((path, text, offset)) = self.locate(params) else {
            return Value::Null;
        };
        let Some((name, _)) = name_at(text, offset) else {
            return Value::Null;
        };
        let Some((target, symbol, source)) = self.resolve(path, text, &name) else {
            return Value::Null;
        };
        json!({
            // The index keys an imported file by the path the import spells
            // (`../lib/util.aipl`), which is the right key and the wrong URI:
            // resolved is what an editor compares against the buffer it
            // already has open.
            "uri": path_to_uri(&resolved(&target)),
            "range": LineIndex::new(&source).range(&symbol.name_span),
        })
    }

    fn hover(&self, params: &Value) -> Value {
        let Some((path, text, offset)) = self.locate(params) else {
            return Value::Null;
        };
        let Some((name, span)) = name_at(text, offset) else {
            return Value::Null;
        };
        let Some((target, symbol, _)) = self.resolve(path, text, &name) else {
            return Value::Null;
        };
        // The signature as a code block, then the declaration's own `# ..`
        // lines as prose — the same two parts, in the same order, that `aipl
        // doc` prints and `aipl docs` renders.
        let mut markdown = format!("```aipl\n{}\n```", symbol.detail);
        if !same_file(&target, path) {
            let file = target.file_name().unwrap_or(target.as_os_str());
            markdown.push_str(&format!("\n\nfrom `{}`", file.to_string_lossy()));
        }
        if let Some(doc) = &symbol.doc {
            markdown.push_str("\n\n");
            markdown.push_str(doc);
        }
        json!({
            "contents": { "kind": "markdown", "value": markdown },
            "range": LineIndex::new(text).range(&span),
        })
    }

    /// The document outline: every top-level declaration in source order, with
    /// a `variant`'s cases nested under it.
    fn document_symbols(&self, params: &Value) -> Value {
        let Some((path, text, _)) = self.locate_document(params) else {
            return Value::Null;
        };
        let mut index = Index::new();
        if index.add(path, text).is_err() {
            // A document mid-edit does not parse for most of the time anyone
            // is looking at it. An empty outline is the honest answer; the
            // alternative is an error popup per keystroke.
            return json!([]);
        }
        let Some(file) = index.file(path) else {
            return json!([]);
        };
        let lines = LineIndex::new(text);
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
    /// and are ignored, exactly as `aipl fmt` ignores them.
    fn formatting(&self, params: &Value) -> Value {
        let Some((_, text, _)) = self.locate_document(params) else {
            return Value::Null;
        };
        let Ok(formatted) = aipl_fmt::format_source(text, &aipl_fmt::FmtOptions::default()) else {
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

    /// The document a request names, and the byte offset of its `position`.
    fn locate(&self, params: &Value) -> Option<(&Path, &str, usize)> {
        let (path, text, _) = self.locate_document(params)?;
        let position = &params["position"];
        let line = position["line"].as_u64()? as u32;
        let character = position["character"].as_u64()? as u32;
        Some((path, text, LineIndex::new(text).offset(line, character)))
    }

    /// The document a request names. The third element is always 0 — it is
    /// there so this shares a shape with [`Server::locate`] and the callers
    /// that want no position can ignore it.
    fn locate_document(&self, params: &Value) -> Option<(&Path, &str, usize)> {
        let document = self
            .documents
            .get(uri_of(&params["textDocument"]).as_str())?;
        Some((document.path.as_deref()?, document.text.as_str(), 0))
    }

    /// Where `name` is declared, as the file at `path` sees it: the defining
    /// file, its declaration, and that file's source (which the caller needs
    /// to turn the declaration's span into a range).
    fn resolve(&self, path: &Path, text: &str, name: &str) -> Option<(PathBuf, Symbol, String)> {
        let (index, sources) = self.index_with_imports(path, text);
        let file = index.file(path)?;
        let (target, symbol) = index.definition_of(file, name)?;
        let source = sources.get(target)?;
        Some((target.to_path_buf(), symbol.clone(), source.clone()))
    }

    /// Index the document and every file it imports directly, so a lookup can
    /// cross a file boundary.
    ///
    /// Only the direct imports: a name this file can refer to is declared
    /// either here or in a file it imports, and nothing further out is
    /// reachable — `import .. as` is the whole namespace, so there is no
    /// transitive re-export to follow.
    ///
    /// Each imported file is keyed by the path the import spells, relative to
    /// this file's directory, because that is the key `definition_of` looks it
    /// up under.
    fn index_with_imports(&self, path: &Path, text: &str) -> (Index, HashMap<PathBuf, String>) {
        let mut index = Index::new();
        let mut sources = HashMap::new();
        if index.add(path, text).is_err() {
            return (index, sources);
        }
        sources.insert(path.to_path_buf(), text.to_string());
        let directory = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let Some(file) = index.file(path) else {
            return (index, sources);
        };
        let targets: Vec<PathBuf> = file
            .imports
            .iter()
            .filter_map(|import| import.from.as_ref())
            .map(|from| directory.join(from))
            .collect();
        for target in targets {
            if sources.contains_key(&target) {
                continue;
            }
            let Some(source) = self.source_of(&target) else {
                continue;
            };
            if index.add(&target, &source).is_ok() {
                sources.insert(target, source);
            }
        }
        (index, sources)
    }

    /// The current text of a file: the open buffer if there is one, otherwise
    /// what is on disk. The buffer wins because it is what the author is
    /// looking at — jumping to a definition they just typed and have not saved
    /// should land on it, not where it used to be.
    fn source_of(&self, path: &Path) -> Option<String> {
        for document in self.documents.values() {
            let Some(open) = &document.path else {
                continue;
            };
            if same_file(open, path) {
                return Some(document.text.clone());
            }
        }
        std::fs::read_to_string(path).ok()
    }
}

/// A path with its `.` components and symlinks resolved, or unchanged when it
/// names nothing on disk — which is the case for a buffer that has not been
/// saved, and not a reason to refuse to answer about it.
fn resolved(path: &Path) -> PathBuf {
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

/// The `textDocument/publishDiagnostics` notification for a snapshot:
/// everything the loader and the type checker report against it.
///
/// Takes no `&Server`, so the caller can let go of the server first. That
/// matters: the frontend over a few thousand lines of AIPL takes seconds, and
/// go-to-definition has to keep answering while it does.
pub fn diagnostics(snapshot: &Snapshot) -> Value {
    let lines = LineIndex::new(&snapshot.text);
    let found = match check(&snapshot.path, &snapshot.text) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .iter()
            .map(|error| diagnostic(&lines, error))
            .collect(),
    };
    publish_diagnostics(&snapshot.uri, snapshot.version, found)
}

/// Load the document and type-check it, collecting every finding.
///
/// This is the compiler's own frontend, not a second opinion about the same
/// source: the loader resolves the imports and runs the lints, and
/// [`aipl_codegen::frontend`] runs the checker. What it skips is everything
/// after the checker — monomorphization, optimization, Cranelift — none of
/// which reports anything an editor can point at, and all of which costs more
/// than the rest put together.
fn check(path: &Path, text: &str) -> Result<(), Vec<Error>> {
    let debug = DebugOptions::new(false);
    let program = aipl_loader::load_program_overlay(path, text, debug)?;
    aipl_codegen::frontend(&program)?;
    Ok(())
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

/// The identifier token at `offset`, with its span.
///
/// A cursor sitting immediately after a name counts as being in it — that is
/// where it lands when you double-click a word and then invoke a command, and
/// treating it as "between two tokens" would answer nothing for the commonest
/// gesture there is.
fn name_at(text: &str, offset: usize) -> Option<(String, Span)> {
    let tokens = aipl_parser::token_scopes(text).ok()?;
    let identifiers = || {
        tokens
            .iter()
            .filter(|token| token.scope == IDENTIFIER_SCOPE)
    };
    let found = identifiers()
        .find(|token| token.span.contains(&offset))
        .or_else(|| identifiers().find(|token| token.span.end == offset))?;
    Some((text[found.span.clone()].to_string(), found.span.clone()))
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
