//! Verifies the TextMate grammar at
//! `editors/vscode/syntaxes/aipl.tmLanguage.json` — the same file the VS
//! Code extension ships — assigns sensible scopes to every token of every
//! (non-error) `.aipl` test case and example.
//!
//! syntect doesn't load `.tmLanguage.json` directly (it speaks Sublime
//! YAML), so this test ships its own tiny TextMate interpreter — a
//! regex-driven, line-by-line scope walker covering the subset of
//! tmLanguage our grammar uses (`match`, `begin`/`end` with `patterns`,
//! `captures`, `contentName`, `include` into `repository`).
//!
//! For each source file we strip the `--- section ---` blocks the case
//! harness uses, lex the remaining AIPL with `aipl::lex_tokens`, then
//! highlight the *whole* file (markers and all) and check the scope the
//! grammar assigned to each lexed token's first byte matches the token's
//! kind. Section-marker lines are checked separately.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use aipl::{lex_tokens, parse_test_section_header, TokenKind};
use regex::Regex;
use serde_json::Value;

/// One compiled pattern from the tmLanguage JSON. `match` patterns set
/// `match_re`; `begin`/`end` patterns set `begin_re`/`end_re` and hold
/// nested pattern refs. Includes stay as references so self-referential
/// rules (e.g. nested block comments) don't blow up compilation.
struct Pattern {
    name: Option<String>,
    /// Per-capture-group scope name (capture index 0 = whole match).
    captures: HashMap<usize, String>,
    match_re: Option<Regex>,
    begin_re: Option<Regex>,
    end_re: Option<Regex>,
    /// Scope assigned to the content between `begin` and `end`.
    content_name: Option<String>,
    /// Nested patterns inside a begin/end block.
    inner: Vec<PatternRef>,
}

enum PatternRef {
    Inline(Pattern),
    /// `#name` — resolved against `Grammar::repository` at walk time.
    Include(String),
}

struct Grammar {
    scope_name: String,
    top: Vec<PatternRef>,
    /// Each repository entry is a list of pattern refs — a "patterns
    /// container" entry (no `match` / `begin`) expands to its `patterns`
    /// list, and a `match`/`begin` entry is a single-element list. Storing
    /// uniformly as Vec makes `include` resolution trivial.
    repository: HashMap<String, Vec<PatternRef>>,
}

fn compile_grammar(json: &Value) -> Grammar {
    let scope_name = json["scopeName"].as_str().expect("scopeName").to_string();
    let repo_map = json
        .get("repository")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut repository: HashMap<String, Vec<PatternRef>> = HashMap::new();
    for (name, entry) in &repo_map {
        let refs = if entry.get("match").is_some() || entry.get("begin").is_some() {
            vec![PatternRef::Inline(compile_one(entry))]
        } else if let Some(pats) = entry.get("patterns") {
            compile_pattern_refs(pats)
        } else {
            Vec::new()
        };
        repository.insert(name.clone(), refs);
    }
    let top = compile_pattern_refs(&json["patterns"]);
    Grammar {
        scope_name,
        top,
        repository,
    }
}

fn compile_pattern_refs(value: &Value) -> Vec<PatternRef> {
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in arr {
        if let Some(inc) = p.get("include").and_then(Value::as_str) {
            out.push(PatternRef::Include(inc.trim_start_matches('#').to_string()));
        } else {
            out.push(PatternRef::Inline(compile_one(p)));
        }
    }
    out
}

fn compile_one(p: &Value) -> Pattern {
    let name = p.get("name").and_then(Value::as_str).map(str::to_string);
    let content_name = p
        .get("contentName")
        .and_then(Value::as_str)
        .map(str::to_string);

    // tmLanguage uses string keys ("0", "1", ...) for captures.
    let mut captures: HashMap<usize, String> = HashMap::new();
    if let Some(map) = p.get("captures").and_then(Value::as_object) {
        collect_captures(map, &mut captures);
    }
    let mut begin_captures = HashMap::new();
    if let Some(map) = p.get("beginCaptures").and_then(Value::as_object) {
        collect_captures(map, &mut begin_captures);
    }

    let match_re = p
        .get("match")
        .and_then(Value::as_str)
        .map(|s| compile_re(s, "match"));
    let begin_re = p
        .get("begin")
        .and_then(Value::as_str)
        .map(|s| compile_re(s, "begin"));
    let end_re = p
        .get("end")
        .and_then(Value::as_str)
        .map(|s| compile_re(s, "end"));

    let inner = if begin_re.is_some() {
        p.get("patterns")
            .map(compile_pattern_refs)
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // For begin/end patterns, `beginCaptures` replaces `captures` for the
    // begin match.
    let captures = if begin_re.is_some() {
        begin_captures
    } else {
        captures
    };

    Pattern {
        name,
        captures,
        match_re,
        begin_re,
        end_re,
        content_name,
        inner,
    }
}

fn collect_captures(map: &serde_json::Map<String, Value>, out: &mut HashMap<usize, String>) {
    for (k, v) in map {
        if let (Ok(idx), Some(scope)) = (k.parse::<usize>(), v.get("name").and_then(Value::as_str))
        {
            out.insert(idx, scope.to_string());
        }
    }
}

fn compile_re(src: &str, kind: &str) -> Regex {
    // The grammar leans on multiline `^`/`$` semantics for line-anchored
    // patterns like the section-marker rule.
    Regex::new(&format!("(?m){src}"))
        .unwrap_or_else(|e| panic!("invalid {kind} regex {src:?}: {e}"))
}

/// Scope at each byte: the deepest scope stack assigned by the grammar.
/// `scopes[i]` is the list of scope names active at byte `i`, outermost
/// first. Bytes the grammar didn't match get just `[source.aipl]`.
struct Highlight {
    scopes: Vec<Vec<String>>,
}

impl Highlight {
    fn at(&self, byte: usize) -> &[String] {
        self.scopes.get(byte).map(Vec::as_slice).unwrap_or_default()
    }

    fn any_in_range_contains(&self, range: std::ops::Range<usize>, needle: &str) -> bool {
        (range.start..range.end.min(self.scopes.len()))
            .any(|i| self.at(i).iter().any(|s| s.contains(needle)))
    }
}

/// Apply the grammar to `text`, returning a per-byte scope stack.
fn highlight(grammar: &Grammar, text: &str) -> Highlight {
    let mut scopes: Vec<Vec<String>> = vec![vec![grammar.scope_name.clone()]; text.len()];
    walk_refs(&grammar.top, grammar, text, 0, text.len(), 0, &mut scopes);
    Highlight { scopes }
}

/// Self-referential rules (nested block comments) would infinite-loop
/// without a brake. Two levels handles real AIPL while staying bounded.
const MAX_INCLUDE_DEPTH: usize = 32;

/// Apply `refs` over `text[start..end]`. `Include` refs resolve against
/// the grammar's repository up to `MAX_INCLUDE_DEPTH` levels deep.
fn walk_refs(
    refs: &[PatternRef],
    grammar: &Grammar,
    text: &str,
    start: usize,
    end: usize,
    depth: usize,
    scopes: &mut Vec<Vec<String>>,
) {
    // Resolve all includes transitively up-front, yielding a flat list of
    // candidate Patterns to try at each position.
    let mut effective: Vec<&Pattern> = Vec::new();
    collect_effective(refs, grammar, depth, &mut effective);

    let mut pos = start;
    while pos < end {
        // Among the candidates at `pos`, pick the one whose match starts
        // earliest (TM's "first applicable rule" model).
        let mut best: Option<(usize, &Pattern, regex::Match)> = None;
        for pat in &effective {
            if let Some(m) = pattern_match_at(pat, text, pos) {
                if m.start() >= end {
                    continue;
                }
                if best.as_ref().is_none_or(|b| m.start() < b.0) {
                    best = Some((m.start(), pat, m));
                }
            }
        }
        let Some((mstart, pat, m)) = best else {
            break;
        };
        if pat.match_re.is_some() {
            apply_match(pat, text, &m, scopes);
            pos = m.end().max(pos + 1);
        } else {
            // begin/end pattern: scope the begin match, then walk inner
            // until end matches (or EOF).
            apply_match(pat, text, &m, scopes);
            let content_start = m.end();
            let (end_start, end_finish) = find_end(pat, text, content_start, end);
            if let Some(name) = &pat.content_name {
                push_scope(scopes, content_start..end_start, name);
            }
            if let Some(name) = &pat.name {
                push_scope(scopes, mstart..end_finish, name);
            }
            walk_refs(
                &pat.inner,
                grammar,
                text,
                content_start,
                end_start,
                depth + 1,
                scopes,
            );
            pos = end_finish.max(pos + 1);
        }
    }
}

fn collect_effective<'g>(
    refs: &'g [PatternRef],
    grammar: &'g Grammar,
    depth: usize,
    out: &mut Vec<&'g Pattern>,
) {
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    for r in refs {
        match r {
            PatternRef::Inline(p) => out.push(p),
            PatternRef::Include(name) => {
                if let Some(refs) = grammar.repository.get(name) {
                    collect_effective(refs, grammar, depth + 1, out);
                }
            }
        }
    }
}

fn pattern_match_at<'t>(pat: &Pattern, text: &'t str, pos: usize) -> Option<regex::Match<'t>> {
    if let Some(re) = &pat.match_re {
        re.find_at(text, pos)
    } else if let Some(re) = &pat.begin_re {
        re.find_at(text, pos)
    } else {
        None
    }
}

fn apply_match(pat: &Pattern, text: &str, m: &regex::Match, scopes: &mut [Vec<String>]) {
    if let Some(name) = &pat.name {
        push_scope(scopes, m.start()..m.end(), name);
    }
    if pat.captures.is_empty() {
        return;
    }
    // Re-run the regex with captures so we can pin each group's range.
    // (regex::Match doesn't carry capture info.)
    let re = pat
        .match_re
        .as_ref()
        .or(pat.begin_re.as_ref())
        .expect("pattern has a regex");
    if let Some(caps) = re.captures_at(text, m.start()) {
        for (idx, scope) in &pat.captures {
            if let Some(g) = caps.get(*idx) {
                push_scope(scopes, g.start()..g.end(), scope);
            }
        }
    }
}

fn push_scope(scopes: &mut [Vec<String>], range: std::ops::Range<usize>, name: &str) {
    for i in range.start..range.end.min(scopes.len()) {
        scopes[i].push(name.to_string());
    }
}

/// Walk forward from `from` looking for `pat.end`. Returns the
/// `(end_start, end_finish)` byte offsets — `end_start` is where the
/// content stops; `end_finish` is where the next pattern resumes.
fn find_end(pat: &Pattern, text: &str, from: usize, limit: usize) -> (usize, usize) {
    let Some(re) = &pat.end_re else {
        return (limit, limit);
    };
    if let Some(m) = re.find_at(text, from) {
        if m.start() < limit {
            return (m.start(), m.end());
        }
    }
    // `\z` and friends: end never literally matches inside the text but
    // we still want to close the block at the limit (EOF).
    (limit, limit)
}

fn load_grammar() -> Grammar {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("editors")
        .join("vscode")
        .join("syntaxes")
        .join("aipl.tmLanguage.json");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let json: Value = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse JSON: {e}"));
    compile_grammar(&json)
}

/// Walk `tests/cases/**` and `examples/*.aipl`, returning every file path.
fn collect_files() -> Vec<(PathBuf, &'static str)> {
    let mut out = Vec::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    visit_aipl(&root.join("tests").join("cases"), "cases", &mut out);
    visit_aipl(&root.join("examples"), "examples", &mut out);
    out
}

fn visit_aipl(dir: &Path, prefix: &'static str, out: &mut Vec<(PathBuf, &'static str)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_aipl(&path, prefix, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("aipl") {
            out.push((path, prefix));
        }
    }
}

/// True if the file declares an `--- errors ---` section — i.e. it
/// intentionally fails to compile, so its source may have parse-level
/// issues we shouldn't expect the grammar to tokenize cleanly.
fn declares_errors(contents: &str) -> bool {
    contents
        .lines()
        .filter_map(parse_test_section_header)
        .any(|h| h == "errors")
}

fn expected_category(kind: TokenKind) -> &'static [&'static str] {
    match kind {
        TokenKind::Keyword => &["keyword"],
        TokenKind::Constant => &["constant.language"],
        TokenKind::BuiltinType => &["support.type"],
        TokenKind::Identifier => &["variable", "entity"],
        TokenKind::Number => &["constant.numeric"],
        TokenKind::Str => &["string.quoted.double"],
        TokenKind::Char => &["string.quoted.single"],
        TokenKind::Operator => &["keyword.operator"],
        TokenKind::Punctuation => &["punctuation"],
    }
}

#[test]
fn grammar_highlights_every_lexed_token() {
    // Lexing the raw-string cases de-dents through the dogfooded AIPL `dedent`
    // (FFI), whose parser hook has no native fallback.
    aipl::install_parser_hooks();

    let grammar = load_grammar();
    let files = collect_files();
    assert!(!files.is_empty(), "no .aipl files found to highlight");

    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for (path, prefix) in &files {
        let display = format!(
            "{}/{}",
            prefix,
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        let src = fs::read_to_string(path).expect("read aipl");
        if declares_errors(&src) {
            continue;
        }
        checked += 1;

        let hl = highlight(&grammar, &src);
        let tokens = match lex_tokens(&src) {
            Ok(t) => t,
            Err(e) => {
                failures.push(format!("{display}: lex failed: {e}"));
                continue;
            }
        };
        for (kind, span) in tokens {
            let categories = expected_category(kind);
            let ok = categories
                .iter()
                .any(|c| hl.any_in_range_contains(span.start..span.end, c));
            if !ok {
                let snippet = &src[span.start..span.end];
                let scopes_here = hl.at(span.start).join(" ");
                failures.push(format!(
                    "{display}: token {kind:?} {snippet:?} at {}..{} got scopes [{scopes_here}], \
                     expected one of {categories:?}",
                    span.start, span.end
                ));
            }
        }
    }

    assert!(
        checked > 10,
        "expected to check many files, only got {checked}"
    );
    if !failures.is_empty() {
        let shown = failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} highlighter mismatch(es) across {checked} file(s):\n{shown}",
            failures.len()
        );
    }
}

#[test]
fn section_markers_are_scoped_as_comments() {
    // `parse_test_section_header` is the dogfooded AIPL with no native fallback.
    aipl::install_parser_hooks();
    let grammar = load_grammar();
    let files = collect_files();
    let mut checked_markers = 0usize;
    for (path, _) in &files {
        let src = fs::read_to_string(path).expect("read aipl");
        // Error cases can have unterminated literals that swallow whatever
        // follows them — including the section marker. Skip them; they're
        // exercised separately as compiler-error fixtures.
        if declares_errors(&src) {
            continue;
        }
        let hl = highlight(&grammar, &src);
        let mut offset = 0usize;
        for line in src.split_inclusive('\n') {
            let trimmed_len = line.len() - line.chars().rev().take_while(|c| *c == '\n').count();
            let line_no_nl = &line[..trimmed_len];
            if parse_test_section_header(line_no_nl.trim()).is_some() {
                // Check the marker's body bytes — skip whitespace-only
                // edges — are scoped as a comment.
                let body_start = offset + line_no_nl.find("---").unwrap();
                let body_end = offset + line_no_nl.rfind("---").unwrap() + 3;
                assert!(
                    hl.any_in_range_contains(body_start..body_end, "comment"),
                    "{}: section marker {line_no_nl:?} not scoped as comment; got {:?}",
                    path.display(),
                    hl.at(body_start)
                );
                checked_markers += 1;
            }
            offset += line.len();
        }
    }
    assert!(
        checked_markers > 5,
        "expected to see section markers in many cases, only saw {checked_markers}"
    );
}

#[test]
fn grammar_handles_plain_files_without_section_markers() {
    // A "normal" .aipl source — no section markers — should still highlight
    // cleanly: every token gets the same scope it would in a case file.
    // `lex_tokens` strips sections via the dogfooded hook (no native fallback).
    aipl::install_parser_hooks();
    let grammar = load_grammar();
    let src = "fn main() -> i64 {\n    let x = 42;\n    x\n}\n";
    let hl = highlight(&grammar, src);
    for (kind, span) in lex_tokens(src).expect("lex") {
        let categories = expected_category(kind);
        let ok = categories
            .iter()
            .any(|c| hl.any_in_range_contains(span.start..span.end, c));
        assert!(
            ok,
            "token {kind:?} {:?} got scopes {:?}, expected one of {categories:?}",
            &src[span.start..span.end],
            hl.at(span.start)
        );
    }
}
