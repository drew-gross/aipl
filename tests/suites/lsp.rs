//! The language server: the requests it answers, and the CLI that serves them.
//!
//! Most of these drive [`aipl::lsp::Server`] in process — it does no I/O of
//! its own, so a test hands it a JSON message and reads the answer, with no
//! subprocess and no protocol framing in the way. One test at the bottom does
//! spawn `aipl lsp` and hold a real conversation with it, because the framing
//! and the CLI wiring are exactly what the in-process tests cannot see.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use aipl::lsp::{Reaction, Server};
use serde_json::{json, Value};

/// A directory of `.aipl` files to answer requests against, removed when the
/// test ends.
///
/// Tagged as well as pid-stamped: under `cargo test` several of these live in
/// one process at once, so the pid alone would have them share a directory.
struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(tag: &str, files: &[(&str, &str)]) -> Workspace {
        let dir = std::env::temp_dir().join(format!("aipl-lsp-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create workspace");
        for (name, text) in files {
            fs::write(dir.join(name), text).expect("write source");
        }
        Workspace { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The URI an editor would send for one of these files. Built the way the
    /// server builds one, so a comparison between the two is meaningful: the
    /// directory is resolved, which on macOS turns `/var/...` into
    /// `/private/var/...`.
    fn uri(&self, name: &str) -> String {
        let path = fs::canonicalize(self.path(name)).expect("resolve source path");
        format!("file://{}", path.display())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A server with `name` open, holding exactly what is on disk.
fn opened(workspace: &Workspace, name: &str) -> Server {
    aipl::install_parser_hooks();
    let mut server = Server::new();
    let text = fs::read_to_string(workspace.path(name)).expect("read source");
    open(&mut server, &workspace.uri(name), &text);
    server
}

fn open(server: &mut Server, uri: &str, text: &str) {
    notify(
        server,
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": uri, "languageId": "aipl", "version": 1, "text": text } }),
    );
}

/// Send a request and return its `result`.
fn request(server: &mut Server, method: &str, params: Value) -> Value {
    let reaction = server.handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
    }));
    let message = reaction
        .message
        .unwrap_or_else(|| panic!("{method} left the request unanswered"));
    assert_eq!(message["id"], json!(1), "answered the request it was asked");
    assert!(
        message.get("error").is_none(),
        "{method} failed: {}",
        message["error"]
    );
    message["result"].clone()
}

/// Send a request that is expected to fail, and return the reason it gave.
fn request_error(server: &mut Server, method: &str, params: Value) -> String {
    let reaction = server.handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
    }));
    let message = reaction
        .message
        .unwrap_or_else(|| panic!("{method} left the request unanswered"));
    message["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("{method} did not fail: {message}"))
        .to_string()
}

/// Run the check the diagnostics worker would, which is what gives a document
/// names the server can answer about.
fn analysed(server: &mut Server, workspace: &Workspace, name: &str) {
    server
        .diagnostics(&workspace.uri(name))
        .expect("a document to check");
}

fn notify(server: &mut Server, method: &str, params: Value) -> Reaction {
    server.handle(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
}

/// The LSP position of the start of `snippet`, which must occur exactly once in
/// `text` — so a test says where it is pointing by quoting the code rather than
/// by counting lines.
fn cursor_on(text: &str, snippet: &str) -> Value {
    let found = text
        .find(snippet)
        .unwrap_or_else(|| panic!("no {snippet:?}"));
    assert!(
        text[found + 1..].find(snippet).is_none(),
        "{snippet:?} is not unique in the source"
    );
    let line = text[..found].matches('\n').count();
    let line_start = text[..found].rfind('\n').map(|at| at + 1).unwrap_or(0);
    json!({ "line": line, "character": text[line_start..found].chars().count() })
}

/// The definition request, as a `(uri, start-line, start-character)` triple.
fn definition_at(server: &mut Server, uri: &str, position: Value) -> Value {
    request(
        server,
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": position }),
    )
}

/// Two files: a root that imports a helper, and the helper.
const ROOT: &str = "import { add } from \"./util.aipl\";\n\
                    \n\
                    fn main() {\n\
                    \u{20}   mut total = add(2, 3);\n\
                    \u{20}   set total = add(total, 1);\n\
                    }\n";

const UTIL: &str = "# Helpers for the root file.\n\
                    \n\
                    import { wrapping_add as + } from builtins;\n\
                    \n\
                    # Adds two integers.\n\
                    pub fn add(a: i64, b: i64) -> i64 {\n\
                    \u{20}   a + b\n\
                    }\n";

/// The lexer scope the server turns a cursor position into a name with. It is
/// read from the lexer rather than re-derived, so a rename there must be
/// noticed here — this is what notices it.
#[test]
fn an_identifier_still_lexes_as_variable_other() {
    aipl::install_parser_hooks();
    let source = "fn add(a: i64) -> i64 { a }";
    let tokens = aipl::token_scopes(source).expect("lex");
    let add = tokens
        .iter()
        .find(|token| source[token.span.clone()] == *"add")
        .expect("the declared name is a token");
    assert_eq!(
        add.scope, "variable.other.aipl",
        "aipl-lsp's IDENTIFIER_SCOPE names this scope; update it with the lexer"
    );
}

/// The references request, as `(uri, line, character)` triples in the order
/// the server gave them.
fn references_at(
    server: &mut Server,
    uri: &str,
    position: Value,
    include_declaration: bool,
) -> Vec<(String, u64, u64)> {
    let found = request(
        server,
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri },
            "position": position,
            "context": { "includeDeclaration": include_declaration },
        }),
    );
    found
        .as_array()
        .unwrap_or_else(|| panic!("no list of references: {found}"))
        .iter()
        .map(|location| {
            (
                location["uri"].as_str().expect("uri").to_string(),
                location["range"]["start"]["line"].as_u64().expect("line"),
                location["range"]["start"]["character"]
                    .as_u64()
                    .expect("character"),
            )
        })
        .collect()
}

fn triple(uri: &str, at: Value) -> (String, u64, u64) {
    (
        uri.to_string(),
        at["line"].as_u64().expect("line"),
        at["character"].as_u64().expect("character"),
    )
}

/// Two structs that share a field name, and a function that shares it too.
const SHARED_NAME: &str = "import { wrapping_add as + } from builtins;\n\
                           \n\
                           struct Pair { name: i64, other: i64 }\n\
                           \n\
                           struct Tag { name: i64 }\n\
                           \n\
                           fn name(self: Pair) -> i64 { self.other }\n\
                           \n\
                           fn read(pair: Pair, tag: Tag) -> i64 { pair.name + tag.name }\n\
                           \n\
                           fn call() -> i64 { read(Pair { name: 1, other: 2 }, Tag { name: 3 }) }\n\
                           \n\
                           fn built(n: i64) -> Pair { name: n, other: n }\n\
                           \n\
                           fn both(pair: Pair) -> i64 { name(pair) + pair.name() }\n\
                           \n\
                           fn mapped(pairs: Pair[]) -> i64[] { pairs$name }\n";

/// The position `past` characters after the start of `snippet`.
fn past_start(text: &str, snippet: &str, past: u64) -> Value {
    let mut at = cursor_on(text, snippet);
    at["character"] = json!(at["character"].as_u64().expect("character") + past);
    at
}

/// `SHARED_NAME` open and checked.
fn shared_name(tag: &str) -> (Workspace, Server) {
    let workspace = Workspace::new(tag, &[("main.aipl", SHARED_NAME)]);
    let mut server = opened(&workspace, "main.aipl");
    analysed(&mut server, &workspace, "main.aipl");
    (workspace, server)
}

/// Where the declaration `definition` jumps to starts, as `(line, character)`.
fn jumps_to(server: &mut Server, workspace: &Workspace, at: &str, past: u64) -> Value {
    let uri = workspace.uri("main.aipl");
    let location = definition_at(server, &uri, past_start(SHARED_NAME, at, past));
    assert_eq!(location["uri"], json!(uri));
    location["range"]["start"].clone()
}

/// The name after `.` or `$` is a field of the type read from, so it goes to
/// that type's field — never to `fn name`, and never to another struct's field
/// of the same name.
#[test]
fn a_field_goes_to_the_field_of_the_type_it_is_read_from() {
    let (workspace, mut server) = shared_name("a_field_goes_to_the_fiel");
    let pair_name = cursor_on(SHARED_NAME, "name: i64, other");
    let tag_name = cursor_on(SHARED_NAME, "name: i64 }\n\nfn name");
    assert_eq!(
        jumps_to(&mut server, &workspace, "name + tag", 0),
        pair_name
    );
    assert_eq!(
        jumps_to(&mut server, &workspace, "name }\n\nfn call", 0),
        tag_name
    );
    // `pairs$name` maps the field over the array.
    assert_eq!(jumps_to(&mut server, &workspace, "$name", 1), pair_name);
}

#[test]
fn a_call_goes_to_the_function_not_to_a_field_of_the_same_name() {
    let (workspace, mut server) = shared_name("a_call_goes_to_the_funct");
    let function = cursor_on(SHARED_NAME, "name(self");
    assert_eq!(
        jumps_to(&mut server, &workspace, "name(pair) +", 0),
        function
    );
    assert_eq!(jumps_to(&mut server, &workspace, "name() }", 0), function);
}

#[test]
fn a_field_in_a_struct_literal_goes_to_the_field_of_that_struct() {
    let (workspace, mut server) = shared_name("a_field_in_a_struct_lite");
    assert_eq!(
        jumps_to(&mut server, &workspace, "name: 1,", 0),
        cursor_on(SHARED_NAME, "name: i64, other")
    );
    assert_eq!(
        jumps_to(&mut server, &workspace, "name: 3 }", 0),
        cursor_on(SHARED_NAME, "name: i64 }\n\nfn name")
    );
    assert_eq!(
        jumps_to(&mut server, &workspace, "name: n,", 0),
        cursor_on(SHARED_NAME, "name: i64, other"),
        "in the body shorthand too"
    );
}

/// Every place `Pair`'s field is used — read, mapped, and written in a
/// literal — and not `Tag`'s field or the function that share its spelling.
#[test]
fn the_references_of_a_field_are_that_fields_uses() {
    let (workspace, mut server) = shared_name("the_references_of_a_fiel");
    let uri = workspace.uri("main.aipl");
    let at = |snippet: &str| triple(&uri, cursor_on(SHARED_NAME, snippet));
    let uses = vec![
        at("name + tag"),
        at("name: 1,"),
        at("name: n,"),
        triple(&uri, past_start(SHARED_NAME, "$name", 1)),
    ];
    let declaration = at("name: i64, other");

    // Asked from a use, and asked from the declaration, it is the same list.
    for from in ["name + tag", "name: i64, other"] {
        let mut everything = vec![declaration.clone()];
        everything.extend(uses.clone());
        assert_eq!(
            references_at(&mut server, &uri, cursor_on(SHARED_NAME, from), true),
            everything,
            "from {from:?}"
        );
        assert_eq!(
            references_at(&mut server, &uri, cursor_on(SHARED_NAME, from), false),
            uses,
            "includeDeclaration: false leaves out the declaration only"
        );
    }
}

#[test]
fn the_references_of_a_function_are_its_calls() {
    let (workspace, mut server) = shared_name("the_references_of_a_func");
    let uri = workspace.uri("main.aipl");
    let at = |snippet: &str| triple(&uri, cursor_on(SHARED_NAME, snippet));
    assert_eq!(
        references_at(&mut server, &uri, cursor_on(SHARED_NAME, "name(self"), true),
        vec![at("name(self"), at("name(pair) +"), at("name() }")]
    );
}

/// Until the compiler has finished with the current text there is nothing it
/// can say, and the request fails with that reason: an empty answer would read
/// as "nothing refers to this".
#[test]
fn a_request_before_the_check_has_finished_says_so() {
    let workspace = Workspace::new("not-yet", &[("main.aipl", SHARED_NAME)]);
    let mut server = opened(&workspace, "main.aipl");
    let uri = workspace.uri("main.aipl");
    let params =
        json!({ "textDocument": { "uri": uri }, "position": cursor_on(SHARED_NAME, "name + tag") });
    assert!(
        request_error(&mut server, "textDocument/definition", params.clone())
            .starts_with("still analysing")
    );

    analysed(&mut server, &workspace, "main.aipl");
    let edited = format!("{SHARED_NAME}\n");
    change(&mut server, &uri, 2, &edited);
    assert!(
        request_error(&mut server, "textDocument/definition", params)
            .starts_with("still analysing"),
        "an edit makes the last check out of date"
    );
}

#[test]
fn a_document_that_does_not_compile_says_so() {
    let broken = format!("{SHARED_NAME}\nfn bad() -> i64 {{ missing }}\n");
    let workspace = Workspace::new("broken", &[("main.aipl", &broken)]);
    let mut server = opened(&workspace, "main.aipl");
    analysed(&mut server, &workspace, "main.aipl");
    let reason = request_error(
        &mut server,
        "textDocument/definition",
        json!({
            "textDocument": { "uri": workspace.uri("main.aipl") },
            "position": cursor_on(&broken, "name + tag"),
        }),
    );
    assert!(reason.contains("does not compile"), "{reason}");
}

/// A name the compiler resolves nothing for is an error, not an empty result:
/// the cursor is on something, and the server cannot say what.
#[test]
fn a_name_the_compiler_did_not_resolve_is_an_error_but_whitespace_is_not() {
    let (workspace, mut server) = shared_name("a_name_the_compiler_did_");
    let uri = workspace.uri("main.aipl");
    let params = |position: Value| json!({ "textDocument": { "uri": uri }, "position": position });
    let reason = request_error(
        &mut server,
        "textDocument/definition",
        params(cursor_on(SHARED_NAME, "n: i64) -> Pair")),
    );
    assert!(reason.contains("does not cover `n`"), "{reason}");
    assert_eq!(
        request(
            &mut server,
            "textDocument/definition",
            params(json!({ "line": 1, "character": 0 }))
        ),
        Value::Null,
        "a blank line is not a name"
    );
}

#[test]
fn hover_shows_the_signature_then_the_docs() {
    let workspace = Workspace::new("hover", &[("main.aipl", ROOT), ("util.aipl", UTIL)]);
    let mut server = opened(&workspace, "main.aipl");

    let hover = request(
        &mut server,
        "textDocument/hover",
        json!({
            "textDocument": { "uri": workspace.uri("main.aipl") },
            "position": cursor_on(ROOT, "add(2, 3)"),
        }),
    );
    assert_eq!(hover["contents"]["kind"], json!("markdown"));
    assert_eq!(
        hover["contents"]["value"],
        json!("```aipl\npub fn add(a: i64, b: i64) -> i64\n```\n\nfrom `util.aipl`\n\nAdds two integers."),
    );
    // The range is the name under the cursor, so the editor underlines `add`
    // rather than the whole line.
    assert_eq!(hover["range"]["start"], cursor_on(ROOT, "add(2, 3)"));
}

/// A file using one builtin written in AIPL, one the compiler expands, and an
/// operator imported under its alias.
const USES_BUILTINS: &str = "import { equal as ==, first, len } from builtins;\n\
                             \n\
                             fn main() {\n\
                             \u{20}   let xs = [1, 2];\n\
                             \u{20}   let head = xs.first();\n\
                             \u{20}   let count = xs.len();\n\
                             }\n";

fn hover_at(server: &mut Server, uri: &str, position: Value) -> Value {
    request(
        server,
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": position }),
    )
}

#[test]
fn hover_documents_every_kind_of_builtin() {
    let workspace = Workspace::new("builtin-hover", &[("main.aipl", USES_BUILTINS)]);
    let mut server = opened(&workspace, "main.aipl");
    let uri = workspace.uri("main.aipl");

    let hover = hover_at(&mut server, &uri, cursor_on(USES_BUILTINS, "first()"));
    assert_eq!(
        hover["contents"]["value"],
        json!(
            "```aipl\npub fn first<T: any>(self: T[]) -> T?\n```\n\nbuiltin\n\n\
             The first element, or `none` if the array is empty."
        ),
        "an AIPL-implemented builtin shows its own file's signature and docs"
    );

    let hover = hover_at(&mut server, &uri, cursor_on(USES_BUILTINS, "len()"));
    assert_eq!(
        hover["contents"]["value"],
        json!(
            "```aipl\nfn len<T: any>(self: T[]) -> u64\n```\n\nbuiltin\n\n\
             Element/byte count. Unsigned: a length is never negative, so `u64` keeps\n\
             `len`-derived arithmetic (`a.len() - b.len()`, capacity math) in the\n\
             saturating-at-zero unsigned world rather than silently going negative.\n\
             Index/slice bounds accept either signedness, so `xs[xs.len() - 1]` still\n\
             works without a conversion."
        ),
        "one the compiler expands shows its declaration in the signature list"
    );

    // An operator has no declaration to read, so what it shows is described by
    // the builtin table. `equal` in `equal as ==` is not a name the file binds, but it is what the
    // author is pointing at.
    let hover = hover_at(&mut server, &uri, cursor_on(USES_BUILTINS, "equal as"));
    assert_eq!(
        hover["contents"]["value"],
        json!(
            "```aipl\nfn equal<T: any>(a: T, b: T) -> bool\n```\n\nbuiltin\n\n\
             Whether the two values are equal. Import it as the operator: `equal as ==`."
        ),
    );
}

fn change(server: &mut Server, uri: &str, version: i64, text: &str) {
    notify(
        server,
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{ "text": text }],
        }),
    );
}

fn symbol_names(server: &mut Server, uri: &str) -> Vec<String> {
    request(
        server,
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": uri } }),
    )
    .as_array()
    .expect("an outline")
    .iter()
    .map(|node| node["name"].as_str().expect("a name").to_string())
    .collect()
}

/// While a buffer is being typed in it does not parse, and the outline would
/// otherwise empty and refill with every keystroke. It keeps showing the last
/// text that did parse.
#[test]
fn the_outline_survives_an_edit_that_does_not_parse() {
    let source = "fn first() {}\nfn second() {}\n";
    let workspace = Workspace::new("outline-broken", &[("main.aipl", source)]);
    let mut server = opened(&workspace, "main.aipl");
    let uri = workspace.uri("main.aipl");
    assert_eq!(symbol_names(&mut server, &uri), ["first", "second"]);

    change(
        &mut server,
        &uri,
        2,
        "fn first() {}\nfn second() {}\nfn third(",
    );
    assert_eq!(
        symbol_names(&mut server, &uri),
        ["first", "second"],
        "the last good outline, not an empty one"
    );

    change(
        &mut server,
        &uri,
        3,
        "fn first() {}\nfn second() {}\nfn third() {}\n",
    );
    assert_eq!(
        symbol_names(&mut server, &uri),
        ["first", "second", "third"],
        "and a text that parses again replaces it"
    );
}

#[test]
fn the_outline_nests_cases_under_their_variant() {
    let source = "variant Shape = Circle(i64) | Point | Rect(i64, i64)\n\
                  \n\
                  struct Canvas { width: i64 }\n\
                  \n\
                  fn main() {}\n";
    let workspace = Workspace::new("outline", &[("main.aipl", source)]);
    let mut server = opened(&workspace, "main.aipl");

    let outline = request(
        &mut server,
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": workspace.uri("main.aipl") } }),
    );
    let top: Vec<(&str, i64, usize)> = outline
        .as_array()
        .expect("an outline")
        .iter()
        .map(|node| {
            (
                node["name"].as_str().expect("name"),
                node["kind"].as_i64().expect("kind"),
                node["children"].as_array().expect("children").len(),
            )
        })
        .collect();
    assert_eq!(
        top,
        vec![("Shape", 10, 3), ("Canvas", 23, 0), ("main", 12, 0)],
        "variant (Enum), struct, fn — and the three cases are the variant's children"
    );

    let cases = &outline[0]["children"];
    assert_eq!(cases[0]["name"], json!("Circle"));
    assert_eq!(cases[0]["kind"], json!(22), "EnumMember");
    assert_eq!(cases[0]["detail"], json!("Circle(i64)"));
    assert_eq!(cases[1]["detail"], json!("Point"), "a case with no payload");

    // A client may drop a child its parent does not contain, so the variant's
    // range has to have grown to cover the last case.
    let last_case_end = &cases[2]["range"]["end"];
    assert_eq!(&outline[0]["range"]["end"], last_case_end);
}

#[test]
fn formatting_replaces_the_whole_document() {
    // Imports out of order and a misindented body: the formatter fixes both.
    let source = "import { print } from builtins;\n\
                  import { wrapping_add as + } from builtins;\n\
                  fn main() !prints {\n\
                  print(`{1 + 2}`);\n\
                  }\n";
    let workspace = Workspace::new("format", &[("main.aipl", source)]);
    let mut server = opened(&workspace, "main.aipl");

    let edits = request(
        &mut server,
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": workspace.uri("main.aipl") },
            "options": { "tabSize": 2, "insertSpaces": true },
        }),
    );
    let edits = edits.as_array().expect("edits");
    assert_eq!(edits.len(), 1, "one edit, covering everything");
    assert_eq!(
        edits[0]["range"]["start"],
        json!({"line": 0, "character": 0})
    );
    let formatted = edits[0]["newText"].as_str().expect("new text");
    assert_eq!(
        formatted,
        &aipl::fmt::format_source(source, &aipl::fmt::FmtOptions::default()).expect("format"),
        "the server formats with the formatter, not with the client's tabSize",
    );

    // Re-opened with that text, there is nothing left to do.
    open(&mut server, &workspace.uri("main.aipl"), formatted);
    let edits = request(
        &mut server,
        "textDocument/formatting",
        json!({ "textDocument": { "uri": workspace.uri("main.aipl") }, "options": {} }),
    );
    assert_eq!(edits, json!([]), "canonical source gets no edit at all");
}

/// Mid-edit source does not parse, and losing the buffer to a format-on-save
/// at that moment is the one unrecoverable thing a formatter can do.
#[test]
fn formatting_declines_unparsable_source() {
    let workspace = Workspace::new("format-broken", &[("main.aipl", "fn main() {")]);
    let mut server = opened(&workspace, "main.aipl");
    let edits = request(
        &mut server,
        "textDocument/formatting",
        json!({ "textDocument": { "uri": workspace.uri("main.aipl") }, "options": {} }),
    );
    assert_eq!(edits, Value::Null);
}

#[test]
fn diagnostics_report_a_type_error_where_it_is() {
    let source = "import { print } from builtins;\n\
                  \n\
                  fn main() !prints {\n\
                  \u{20}   print(nope);\n\
                  }\n";
    let workspace = Workspace::new("diagnostics", &[("main.aipl", source)]);
    let mut server = opened(&workspace, "main.aipl");
    let uri = workspace.uri("main.aipl");

    let published = server.diagnostics(&uri).expect("diagnostics");
    assert_eq!(
        published["method"],
        json!("textDocument/publishDiagnostics")
    );
    assert_eq!(published["params"]["uri"], json!(uri));
    assert_eq!(
        published["params"]["version"],
        json!(1),
        "echoed back so a client can drop a stale batch"
    );
    let found = published["params"]["diagnostics"]
        .as_array()
        .expect("a list");
    assert_eq!(found.len(), 1, "one error: {found:?}");
    assert_eq!(found[0]["severity"], json!(1), "error");
    assert_eq!(found[0]["source"], json!("aipl"));
    assert_eq!(
        found[0]["range"]["start"],
        cursor_on(source, "nope"),
        "pointing at the undefined name"
    );
}

/// The whole reason the loader grew an overlay: the editor's buffer is the
/// source of truth for the file being typed into, and the copy on disk is
/// whatever it was when it was last saved.
#[test]
fn diagnostics_check_the_buffer_not_the_file_on_disk() {
    let good = "fn main() {}\n";
    let workspace = Workspace::new("overlay", &[("main.aipl", good)]);
    let mut server = opened(&workspace, "main.aipl");
    let uri = workspace.uri("main.aipl");

    let clean = server.diagnostics(&uri).expect("diagnostics");
    assert_eq!(clean["params"]["diagnostics"], json!([]), "disk is fine");

    notify(
        &mut server,
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "fn main() -> i64 { \"not an int\" }\n" }],
        }),
    );
    assert_eq!(
        fs::read_to_string(workspace.path("main.aipl")).expect("read"),
        good,
        "nothing was written — the edit is unsaved, which is the point"
    );
    let published = server.diagnostics(&uri).expect("diagnostics");
    assert_eq!(published["params"]["version"], json!(2));
    assert!(
        !published["params"]["diagnostics"]
            .as_array()
            .expect("a list")
            .is_empty(),
        "the unsaved text is what got checked"
    );
}

/// An error raised against an imported file indexes *that* file's source, so
/// its span would land on an unrelated line here. It is reported at the top of
/// the document with the other file named instead.
#[test]
fn diagnostics_name_the_imported_file_an_error_came_from() {
    let broken_util = "pub fn add(a: i64, b: i64) -> i64 {\n    nope\n}\n";
    let workspace = Workspace::new(
        "imported-error",
        &[("main.aipl", ROOT), ("util.aipl", broken_util)],
    );
    let mut server = opened(&workspace, "main.aipl");

    let published = server
        .diagnostics(&workspace.uri("main.aipl"))
        .expect("diagnostics");
    let found = published["params"]["diagnostics"]
        .as_array()
        .expect("a list");
    assert!(!found.is_empty(), "the import does not compile");
    let message = found[0]["message"].as_str().expect("message");
    let util = workspace
        .path("util.aipl")
        .canonicalize()
        .expect("canonical path");
    assert_eq!(
        message,
        format!("{}: unknown identifier \"nope\"", util.display()),
        "names the file it came from, by its full path"
    );
    assert_eq!(
        found[0]["range"],
        json!({ "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0} }),
        "at the top of this file, since the span belongs to another one"
    );
}

/// Checking a document must not hold the server, because the frontend over a
/// few thousand lines takes seconds and go-to-definition has to keep
/// answering meanwhile. A snapshot is what makes that possible, and this is
/// the test that stops a `&Server` parameter from creeping back in: it would
/// no longer compile.
#[test]
fn a_snapshot_is_checked_without_the_server() {
    let workspace = Workspace::new("snapshot", &[("main.aipl", "fn main() {}\n")]);
    let server = opened(&workspace, "main.aipl");
    let snapshot = server
        .snapshot(&workspace.uri("main.aipl"))
        .expect("a snapshot");
    drop(server);

    let published = aipl::lsp::server::diagnostics(&snapshot);
    assert_eq!(published["params"]["diagnostics"], json!([]));
    assert_eq!(
        published["params"]["uri"],
        json!(workspace.uri("main.aipl"))
    );
}

#[test]
fn closing_a_document_withdraws_its_diagnostics() {
    let workspace = Workspace::new("close", &[("main.aipl", "fn main() {}\n")]);
    let mut server = opened(&workspace, "main.aipl");
    let uri = workspace.uri("main.aipl");

    let reaction = notify(
        &mut server,
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    let message = reaction.message.expect("a notification");
    assert_eq!(message["method"], json!("textDocument/publishDiagnostics"));
    assert_eq!(message["params"]["diagnostics"], json!([]));
    assert!(
        server.diagnostics(&uri).is_none(),
        "the document is gone, so there is nothing to recheck"
    );
}

/// A client that asked a question waits for an answer. Dropping a request it
/// does not implement would read as a hang rather than as a decline.
#[test]
fn an_unsupported_request_is_declined_rather_than_dropped() {
    aipl::install_parser_hooks();
    let mut server = Server::new();
    let reaction = server.handle(&json!({
        "jsonrpc": "2.0", "id": 7, "method": "textDocument/inlayHint", "params": {},
    }));
    let message = reaction.message.expect("an answer");
    assert_eq!(message["id"], json!(7));
    assert_eq!(message["error"]["code"], json!(-32601), "method not found");

    // A *notification* it does not implement is ignored, as the protocol asks.
    let reaction = notify(&mut server, "$/setTrace", json!({ "value": "off" }));
    assert!(reaction.message.is_none());
}

#[test]
fn an_edit_asks_for_a_recheck_and_a_request_does_not() {
    let workspace = Workspace::new("recheck", &[("main.aipl", "fn main() {}\n")]);
    let uri = workspace.uri("main.aipl");
    aipl::install_parser_hooks();
    let mut server = Server::new();

    let reaction = notify(
        &mut server,
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": uri, "languageId": "aipl", "version": 1, "text": "fn main() {}\n" } }),
    );
    assert_eq!(reaction.recheck.as_deref(), Some(uri.as_str()), "on open");

    let reaction = notify(
        &mut server,
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert_eq!(reaction.recheck.as_deref(), Some(uri.as_str()), "on save");

    let reaction = server.handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "textDocument/documentSymbol",
        "params": { "textDocument": { "uri": uri } },
    }));
    assert_eq!(
        reaction.recheck, None,
        "reading the document changes nothing"
    );
}

/// End to end, through the CLI: the framing, the subcommand, and the hook
/// installation that an in-process test gets for free.
#[test]
fn the_cli_serves_the_protocol_over_a_pipe() {
    let workspace = Workspace::new("cli", &[("main.aipl", ROOT), ("util.aipl", UTIL)]);
    let uri = workspace.uri("main.aipl");

    let conversation = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"capabilities": {}}}),
        json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
        json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
            "textDocument": {"uri": uri, "languageId": "aipl", "version": 1, "text": ROOT},
        }}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "textDocument/hover", "params": {
            "textDocument": {"uri": uri}, "position": cursor_on(ROOT, "add(2, 3)"),
        }}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": null}),
        json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
    ];

    let mut child = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("lsp")
        // Spawned exactly as the VS Code client spawns it:
        // `vscode-languageclient` appends `--stdio` for `TransportKind.stdio`.
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `aipl lsp`");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for message in &conversation {
            let body = serde_json::to_vec(message).expect("serialize");
            write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
            stdin.write_all(&body).expect("write body");
        }
    }
    // Closing stdin is the other way a client says it is done, and it keeps
    // this test from deadlocking if `exit` is ever not handled.
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "`aipl lsp` exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );

    let replies = unframe(&output.stdout);
    let capabilities = &replies.get("1").expect("an initialize result")["capabilities"];
    for provider in [
        "definitionProvider",
        "hoverProvider",
        "documentSymbolProvider",
        "documentFormattingProvider",
    ] {
        assert_eq!(capabilities[provider], json!(true), "{provider} announced");
    }
    assert!(
        replies.get("2").expect("a hover result")["contents"]["value"]
            .as_str()
            .is_some_and(|hover| hover.contains("from `util.aipl`")),
        "and it answered across the import"
    );
}

/// `--stdio` is a client announcing the transport, and is accepted. Anything
/// else is a person running this by hand expecting output, and is told so
/// rather than left waiting on a pipe that will never carry a message.
#[test]
fn the_cli_refuses_an_argument_that_is_not_a_transport() {
    let output = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .args(["lsp", "--socket=1234"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn `aipl lsp`");
    assert!(!output.status.success(), "a transport it cannot speak");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        complaint.contains("--socket=1234") && complaint.contains("`--stdio`"),
        "names the argument and what is accepted: {complaint}"
    );
}

/// Split a framed reply stream into `id -> result`, dropping the
/// notifications (which carry no id).
fn unframe(stream: &[u8]) -> BTreeMap<String, Value> {
    let mut replies = BTreeMap::new();
    let mut rest = stream;
    while !rest.is_empty() {
        let split = find(rest, b"\r\n\r\n").expect("a header block");
        let headers = std::str::from_utf8(&rest[..split]).expect("ascii headers");
        let length: usize = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length:"))
            .expect("a Content-Length")
            .trim()
            .parse()
            .expect("a length");
        let body = &rest[split + 4..split + 4 + length];
        let message: Value = serde_json::from_slice(body).expect("a JSON body");
        if let Some(id) = message.get("id") {
            replies.insert(
                id.to_string().trim_matches('"').to_string(),
                message["result"].clone(),
            );
        }
        rest = &rest[split + 4 + length..];
    }
    replies
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
