//! A documentation site for AIPL source: static HTML and one stylesheet,
//! written to a directory.
//!
//! The first consumer of [`aipl_index`], and deliberately a thin one — it asks
//! the index for symbols and their documentation, and spends its own effort on
//! presentation. Anything it needed that the index could not answer would be a
//! gap in the index rather than something to work around here.
//!
//! It does *not* show a file's imports, though the index carries them. An
//! import list is about how a file is wired, which is a question for the
//! source; a docs page answers what a file offers. Nothing was learned from
//! reading them on the rendered site, and they pushed the declarations — the
//! reason to be on the page — below the fold.
//!
//! **A file's own `# ..` block is the page's introduction.** It documents the
//! file rather than any declaration in it, so it sits above both sections, and
//! its first paragraph is the one-line summary the index page shows beside the
//! file's name — the only prose there, and the only thing on that page that
//! says what a file is *for* rather than what it contains.
//!
//! **Public and private declarations are separate sections**, public first. A
//! private declaration is not importable, so it is not part of what a file
//! offers to anyone else; it stays on the page because a reader of *this* file
//! still wants it, and it stays out of the way for everyone else.
//!
//! # What it produces
//!
//! ```text
//! out/
//!   index.html      every file, and every item in it
//!   style.css       one stylesheet, shared
//!   <slug>.html     one page per source file
//! ```
//!
//! **Everything lands in one flat directory**, and pages are named after a
//! slug of the source path (`crates-aipl-codegen-src-grammar.html`) rather than
//! mirroring the tree. That makes every link inside the site a bare file name,
//! so the whole thing works opened straight off the filesystem, moved
//! somewhere else, or unzipped — with no base href, no relative-path
//! arithmetic and no server. The page's heading still shows the real path.
//!
//! **Doc text is prose, not Markdown** — but it is prose with habits, and the
//! renderer knows the four this repo actually writes: blank-line paragraphs,
//! `` `code spans` `` (delimited by a run of backticks, so a span can hold one),
//! `- ` lists, and `**bold**` / `*emphasis*`. An indented block stays verbatim
//! in a `<pre>`, since reflowing one would destroy the thing it is showing.
//! Anything else — headings, links, tables — renders as the characters it is
//! written with, which is the honest answer for text nobody wrote as Markdown.
//!
//! **No JavaScript, and no web fonts.** A docs tree should open from a
//! `file://` URL, over a slow link, and out of an archive; each of those rules
//! out fetching something else first.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use aipl_index::{FileIndex, Index, SymbolKind};
use askama::Template;

/// The stylesheet, written next to the pages. A plain file rather than a
/// template: nothing in it varies per site, and keeping it out of the template
/// engine keeps CSS braces from having to be escaped.
const STYLESHEET: &str = include_str!("style.css");

/// Anything that stopped a site being written.
#[derive(Debug)]
pub enum Error {
    /// A page could not be rendered — a template bug, not a source problem.
    Render(askama::Error),
    /// The output directory could not be created or written to.
    Io { path: PathBuf, err: std::io::Error },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Render(e) => write!(f, "rendering a page: {e}"),
            Error::Io { path, err } => write!(f, "{}: {err}", path.display()),
        }
    }
}

impl std::error::Error for Error {}

/// Write a documentation site for everything in `index` to `out`.
///
/// `root` is the directory paths are shown relative to — the project root, so a
/// page is headed `crates/aipl-codegen/src/grammar.aipl` rather than by an
/// absolute path. `project` is the site's title.
///
/// Existing files in `out` are overwritten; nothing else is removed, so
/// generating into a directory that holds an older site leaves that site's
/// stale pages behind. Point it at a directory of its own.
pub fn write_site(index: &Index, root: &Path, project: &str, out: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(out).map_err(|err| Error::Io {
        path: out.to_path_buf(),
        err,
    })?;

    // Slugs are assigned once, up front: a page's own links and the index's
    // links to it have to agree, and both are derived from this map.
    let slugs = slugs(index, root);

    let mut listings = Vec::new();
    let mut symbol_count = 0usize;
    for file in index.files() {
        let slug = &slugs[&file.path];
        let page = FilePage::build(file, root, project, slug);
        symbol_count += file.symbols.len();
        let summary = summary_html(file.module_doc.as_deref().unwrap_or_default());
        listings.push(FileListing {
            display: display_path(&file.path, root),
            href: format!("{slug}.html"),
            has_summary: !summary.is_empty(),
            summary,
            entries: page.entries(),
            private_count: page.private.len(),
        });
        write(out.join(format!("{slug}.html")), &render(&page)?)?;
    }

    let index_page = IndexPage {
        title: project.to_string(),
        project: project.to_string(),
        file_count: listings.len(),
        symbol_count,
        files: listings,
    };
    write(out.join("index.html"), &render(&index_page)?)?;
    write(out.join("style.css"), STYLESHEET)?;
    Ok(())
}

fn render<T: Template>(t: &T) -> Result<String, Error> {
    t.render().map_err(Error::Render)
}

fn write(path: PathBuf, contents: &str) -> Result<(), Error> {
    std::fs::write(&path, contents).map_err(|err| Error::Io { path, err })
}

/// A page name per indexed file: the path relative to `root`, with everything
/// that is not a letter, digit or `-`/`_` turned into `-`.
///
/// Two different paths can slug the same (`a/b.aipl` and `a-b.aipl`), so a
/// repeat gets a numeric suffix. That is rare enough not to be worth a prettier
/// scheme and common enough to be worth not silently overwriting a page.
fn slugs(index: &Index, root: &Path) -> BTreeMap<PathBuf, String> {
    let mut used: HashSet<String> = HashSet::new();
    let mut out = BTreeMap::new();
    for file in index.files() {
        let rel = display_path(&file.path, root);
        let rel = rel.strip_suffix(".aipl").unwrap_or(&rel);
        let base: String = rel
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let base = base.trim_matches('-').to_string();
        let base = if base.is_empty() {
            "file".to_string()
        } else {
            base
        };
        let mut slug = base.clone();
        let mut n = 2;
        while !used.insert(slug.clone()) {
            slug = format!("{base}-{n}");
            n += 1;
        }
        out.insert(file.path.clone(), slug);
    }
    out
}

/// `path` as the site shows it: relative to `root` where it can be, with
/// forward slashes so a page reads the same on every platform.
fn display_path(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

// ---------- template data ----------

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    title: String,
    project: String,
    file_count: usize,
    symbol_count: usize,
    files: Vec<FileListing>,
}

struct FileListing {
    display: String,
    href: String,
    /// The file's own documentation, first paragraph only — see
    /// [`summary_html`]. Pre-rendered HTML, marked `|safe` by the template.
    summary: String,
    has_summary: bool,
    /// Public declarations only. The index is a table of contents for what a
    /// project offers; a file's private helpers are one click away on its own
    /// page, and listing them here buried the rest.
    entries: Vec<Entry>,
    /// How many were left out, so the page never silently hides a declaration.
    private_count: usize,
}

struct Entry {
    name: String,
    anchor: String,
    kind: &'static str,
    kind_class: &'static str,
}

#[derive(Template)]
#[template(path = "file.html")]
struct FilePage {
    title: String,
    project: String,
    display: String,
    /// The file's own `# ..` block as HTML — see [`doc_html`]. Empty when the
    /// file has none, and then nothing is rendered in its place: a page whose
    /// declarations are documented is not an undocumented page, so the
    /// "Undocumented." an item gets would be a lie here.
    doc: String,
    has_doc: bool,
    /// Importable from another file, and so the part of this file that is
    /// anyone else's business. Shown first.
    public: Vec<SymbolSection>,
    private: Vec<SymbolSection>,
}

struct SymbolSection {
    name: String,
    anchor: String,
    kind: &'static str,
    kind_class: &'static str,
    /// Which of the page's two sections this belongs in. Not rendered on the
    /// item itself — the section heading says it once, so a `pub` badge on
    /// every public item would be noise.
    is_pub: bool,
    detail: String,
    /// Pre-rendered HTML — see [`doc_html`]. The template marks it `|safe`,
    /// which is why that function is the one place doc text is escaped.
    doc: String,
    has_doc: bool,
    cases: Vec<CaseRow>,
}

struct CaseRow {
    anchor: String,
    detail: String,
    /// The case's own `# ..` lines, rendered like a declaration's — see
    /// [`doc_html`]. Empty when it has none: a case without docs is listed
    /// bare rather than marked undocumented, since the variant's docs usually
    /// cover it.
    doc: String,
    has_doc: bool,
    /// The payload slots, listed only when one of them has docs of its own —
    /// see `aipl_index::Symbol::slots`.
    slots: Vec<SlotRow>,
}

struct SlotRow {
    detail: String,
    doc: String,
    has_doc: bool,
}

impl FilePage {
    fn build(file: &FileIndex, root: &Path, project: &str, slug: &str) -> FilePage {
        let display = display_path(&file.path, root);
        let module_doc = file.module_doc.as_deref().unwrap_or_default();
        let mut symbols: Vec<SymbolSection> = Vec::new();
        for sym in &file.symbols {
            // A case is shown under the variant that declares it rather than as
            // a section of its own: it is one alternative of a type, not a
            // separate thing to read about.
            if sym.kind == SymbolKind::Case {
                if let Some(parent) = symbols.last_mut() {
                    let doc = sym.doc.as_deref().unwrap_or_default();
                    parent.cases.push(CaseRow {
                        anchor: anchor(&sym.name),
                        detail: sym.detail.clone(),
                        doc: doc_html(doc),
                        has_doc: !doc.trim().is_empty(),
                        slots: sym
                            .slots
                            .iter()
                            .map(|slot| {
                                let doc = slot.doc.as_deref().unwrap_or_default();
                                SlotRow {
                                    detail: slot.detail.clone(),
                                    doc: doc_html(doc),
                                    has_doc: !doc.trim().is_empty(),
                                }
                            })
                            .collect(),
                    });
                    continue;
                }
            }
            let doc = sym.doc.as_deref().unwrap_or_default();
            symbols.push(SymbolSection {
                name: sym.name.clone(),
                anchor: anchor(&sym.name),
                kind: kind_label(sym.kind),
                kind_class: kind_class(sym.kind),
                is_pub: sym.is_pub,
                detail: sym.detail.clone(),
                doc: doc_html(doc),
                has_doc: !doc.trim().is_empty(),
                cases: Vec::new(),
            });
        }
        let _ = slug;
        // Split after building rather than while: a case attaches to the
        // declaration before it, and that is a fact about source order, not
        // about visibility.
        let (public, private) = symbols.into_iter().partition(|s: &SymbolSection| s.is_pub);
        FilePage {
            title: format!("{display} — {project}"),
            project: project.to_string(),
            display,
            doc: doc_html(module_doc),
            has_doc: !module_doc.trim().is_empty(),
            public,
            private,
        }
    }

    /// The one-line-per-item summary the index page shows for this file.
    fn entries(&self) -> Vec<Entry> {
        self.public
            .iter()
            .map(|s| Entry {
                name: s.name.clone(),
                anchor: s.anchor.clone(),
                kind: s.kind,
                kind_class: s.kind_class,
            })
            .collect()
    }
}

fn kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "fn",
        SymbolKind::Struct => "struct",
        SymbolKind::Variant => "variant",
        SymbolKind::Case => "case",
        SymbolKind::Constant => "let",
    }
}

fn kind_class(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "kind-fn",
        SymbolKind::Struct => "kind-struct",
        SymbolKind::Variant => "kind-variant",
        SymbolKind::Case => "kind-case",
        SymbolKind::Constant => "kind-const",
    }
}

/// A URL fragment for `name`.
///
/// An AIPL name can be an operator spelling (`==`, `+++`), which is not a
/// fragment, so anything outside the identifier characters is percent-free
/// escaped to `-`. Names that differ only in punctuation would collide; that
/// cannot happen among a file's *declarations*, which are identifiers.
fn anchor(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("item-{cleaned}")
}

/// A declaration's documentation as HTML.
///
/// Deliberately not a Markdown renderer — AIPL doc text is prose, and the two
/// things it actually uses are blank-line paragraphs and `` `inline code` ``.
/// An indented block (four spaces or more, as the raw-string docs in this repo
/// write their examples) becomes a `<pre>`, since reflowing one would destroy
/// the thing it is showing.
///
/// Everything is escaped first, so this is the only place doc text becomes
/// markup and the templates can mark the result `|safe`.
fn doc_html(doc: &str) -> String {
    let mut out = String::new();
    for block in blocks(doc) {
        match block {
            Block::Code(lines) => {
                // The indentation is what *marked* the block as code; keeping
                // it would indent the rendered block again, since `<pre>`
                // preserves every space. Strip the common prefix and leave any
                // relative structure inside it alone.
                let indent = lines
                    .iter()
                    .map(|l| l.len() - l.trim_start().len())
                    .min()
                    .unwrap_or(0);
                out.push_str("<pre><code>");
                for line in lines {
                    let _ = writeln!(out, "{}", escape(&line[indent..]));
                }
                out.push_str("</code></pre>");
            }
            Block::Para(lines) => {
                let joined = lines.join(" ");
                let _ = write!(out, "<p>{}</p>", inline(&joined));
            }
            Block::List(items) => {
                out.push_str("<ul>");
                for item in items {
                    let _ = write!(out, "<li>{}</li>", inline(&item.join(" ")));
                }
                out.push_str("</ul>");
            }
        }
    }
    out
}

/// A documentation block's first paragraph, as inline HTML with no `<p>`
/// around it — what the index page shows beside a file's name.
///
/// The first paragraph rather than the first line: a doc block is prose, wrapped
/// wherever the author's column ran out, so a line is not a unit of anything.
/// Empty when the block opens with a code block, which is not a summary of
/// anything either.
fn summary_html(doc: &str) -> String {
    match blocks(doc).into_iter().next() {
        Some(Block::Para(lines)) => inline(&lines.join(" ")),
        Some(Block::Code(_)) | Some(Block::List(_)) | None => String::new(),
    }
}

enum Block<'a> {
    Para(Vec<&'a str>),
    Code(Vec<&'a str>),
    /// One `- ` run, as the lines of each item.
    List(Vec<Vec<&'a str>>),
}

/// Split doc text into paragraphs, `- ` lists and indented code blocks. A blank
/// line ends any of them; indentation of four spaces or more starts a code
/// block, and a line whose first non-space characters are `- ` starts a list
/// item.
///
/// A list is indented — two spaces, in the doc blocks this repo writes — and its
/// continuation lines are indented further still, past the four that would
/// otherwise mark code. So while a list is open, indentation continues the item
/// rather than opening a code block; the list ends where the blank line does.
/// That is also why the code rule is checked second: a four-space line is code
/// only when there is no list to belong to.
fn blocks(doc: &str) -> Vec<Block<'_>> {
    let indented_line = |l: &str| l.starts_with("    ") && !l.trim().is_empty();
    let item_line = |l: &str| !indented_line(l) && l.trim_start().starts_with("- ");
    let mut out: Vec<Block> = Vec::new();
    let mut open: Option<Block> = None;
    for line in doc.lines() {
        // A blank line ends whatever is open, and starts nothing.
        if line.trim().is_empty() {
            out.extend(open.take());
            continue;
        }
        // A paragraph swallows any following line, indented or not — an
        // indented continuation is a wrapped sentence, not a code block. A code
        // block ends the moment the indentation does, and a list the moment a
        // line is neither a new item nor indented under one.
        let continues = match &open {
            Some(Block::Para(_)) => !item_line(line),
            Some(Block::Code(_)) => indented_line(line),
            Some(Block::List(_)) => item_line(line) || indented_line(line),
            None => false,
        };
        if !continues {
            out.extend(open.take());
            open = Some(if item_line(line) {
                Block::List(Vec::new())
            } else if indented_line(line) {
                Block::Code(Vec::new())
            } else {
                Block::Para(Vec::new())
            });
        }
        match open.as_mut() {
            Some(Block::Code(lines)) => lines.push(line),
            Some(Block::Para(lines)) => lines.push(line.trim()),
            Some(Block::List(items)) => {
                let text = line.trim();
                match text.strip_prefix("- ") {
                    // A new item, with the marker off: it is what said "item",
                    // not part of what the item says.
                    Some(rest) => items.push(vec![rest.trim_start()]),
                    None => match items.last_mut() {
                        Some(item) => item.push(text),
                        None => items.push(vec![text]),
                    },
                }
            }
            None => {}
        }
    }
    out.extend(open);
    out
}

/// Escape `text`, then turn `` `spans` `` into `<code>` elements and
/// `**bold**` / `*emphasis*` into `<strong>` / `<em>`. Escaping first is what
/// makes this safe: by the time a delimiter is looked for, every `<`, `&` and
/// `"` in the text is already an entity.
///
/// Code spans are found first and become opaque, so nothing is looked for
/// inside one — a `` `xs: i64*` `` is a variadic parameter, not an open
/// emphasis. They are *atoms* rather than a separate pass, because prose here
/// routinely emphasizes across one (**`Member` is where the seam shows.**) and
/// two passes would each see only half of that.
///
/// A span is delimited by a *run* of backticks and closes at the next run of
/// the same length, so a span can hold a backtick by doubling its
/// delimiters — which this repo's prose does whenever it names one. Counting
/// single ticks instead would take the inner one for a delimiter and shift
/// every span after it in the paragraph.
fn inline(text: &str) -> String {
    let escaped = escape(text);
    let chars: Vec<char> = escaped.chars().collect();
    let mut pieces: Vec<Piece> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            let open = tick_run(&chars, i);
            if let Some(close) = next_tick_run(&chars, i + open, open) {
                let body: String = chars[i + open..close].iter().collect();
                pieces.push(Piece::Code(format!("<code>{}</code>", code_body(&body))));
                i = close + open;
                continue;
            }
        }
        // A run that never closes is a stray tick: it stays literal rather than
        // swallowing the rest of the paragraph into a `<code>`.
        pieces.push(Piece::Ch(chars[i]));
        i += 1;
    }
    emphasize(&pieces)
}

/// How many backticks in a row start at `at`.
fn tick_run(chars: &[char], at: usize) -> usize {
    chars[at..].iter().take_while(|c| **c == '`').count()
}

/// The next run of *exactly* `n` backticks at or after `from`. A longer run is
/// not a closer — it is content of a span this one cannot close.
fn next_tick_run(chars: &[char], from: usize, n: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] != '`' {
            i += 1;
            continue;
        }
        let run = tick_run(chars, i);
        if run == n {
            return Some(i);
        }
        i += run;
    }
    None
}

/// A code span's text: one space comes off each end when both are there, which
/// is what lets `` ` `` hold a backtick without the delimiters touching it.
/// A span of nothing but spaces keeps them — there is nothing else in it.
fn code_body(body: &str) -> &str {
    match (body.strip_prefix(' '), body.strip_suffix(' ')) {
        (Some(_), Some(_)) if !body.trim().is_empty() => &body[1..body.len() - 1],
        _ => body,
    }
}

/// One atom of an inline run: a rendered code span, which nothing looks inside,
/// or one character of ordinary text.
enum Piece {
    Code(String),
    Ch(char),
}

impl Piece {
    fn is_star(&self) -> bool {
        matches!(self, Piece::Ch('*'))
    }

    /// A code span is not whitespace — it is what a delimiter beside it is
    /// wrapping.
    fn is_space(&self) -> bool {
        matches!(self, Piece::Ch(c) if c.is_whitespace())
    }
}

/// `**bold**` and `*emphasis*`, longest marker first so `**` never reads as two
/// `*`. Recursive, so a `*word*` inside a `**lead-in**` is both.
///
/// A delimiter only opens when a non-space follows it and only closes when a
/// non-space precedes it, which keeps arithmetic prose (`a * b * c`) and a lone
/// marker out of it. An unpaired delimiter stays literal rather than running to
/// the end of the paragraph — what it would swallow is prose someone wrote, not
/// markup they meant.
fn emphasize(pieces: &[Piece]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < pieces.len() {
        if pieces[i].is_star() {
            let open = star_run(pieces, i).min(2);
            if let Some(close) = closer(pieces, i + open, open) {
                let (before, after) = if open == 2 {
                    ("<strong>", "</strong>")
                } else {
                    ("<em>", "</em>")
                };
                out.push_str(before);
                out.push_str(&emphasize(&pieces[i + open..close]));
                out.push_str(after);
                i = close + open;
                continue;
            }
        }
        match &pieces[i] {
            Piece::Code(html) => out.push_str(html),
            Piece::Ch(c) => out.push(*c),
        }
        i += 1;
    }
    out
}

/// How many `*` in a row start at `at`.
fn star_run(pieces: &[Piece], at: usize) -> usize {
    pieces[at..].iter().take_while(|p| p.is_star()).count()
}

/// Where the span whose content starts at `from` closes: the next run of at
/// least `open` stars that has something other than a space in front of it.
/// `None` when the span never opened (a space right after the marker, or
/// nothing at all) or never closes.
fn closer(pieces: &[Piece], from: usize, open: usize) -> Option<usize> {
    match pieces.get(from) {
        None => return None,
        Some(p) if p.is_space() => return None,
        Some(_) => {}
    }
    let mut i = from;
    while i < pieces.len() {
        if !pieces[i].is_star() {
            i += 1;
            continue;
        }
        let run = star_run(pieces, i);
        if run >= open && i > from && !pieces[i - 1].is_space() {
            return Some(i);
        }
        i += run;
    }
    None
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"# Shapes, and the areas of them.
#
# A second paragraph, which is on the file's page but not in the one-line
# summary the index shows.

import { print } from builtins;

struct Point { x: i64, y: i64 }

# A shape.
variant Shape =
    # A circle of radius `r`.
    | Circle(r: i64)
    | Empty
    | Rect(
        # The width.
        w: i64,
        h: i64
    )

# The area of `s`.
#
# Worked example:
#
#     let a = area(Circle(2));
#
# Rounded down, always.
pub fn area(s: Shape) -> i64 { 0 }

fn private_helper() -> i64 { 1 }
"#;

    fn site() -> (tempdir::Dir, PathBuf) {
        aipl_codegen::install_parser_hooks();
        let dir = tempdir::Dir::new("aipl-docs");
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("mkdir");
        std::fs::write(src_dir.join("shapes.aipl"), SRC).expect("write");
        let mut index = Index::new();
        index
            .add(src_dir.join("shapes.aipl"), SRC)
            .expect("indexes");
        let out = dir.path().join("out");
        write_site(&index, dir.path(), "demo", &out).expect("writes");
        (dir, out)
    }

    fn read(out: &Path, name: &str) -> String {
        std::fs::read_to_string(out.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    #[test]
    fn writes_an_index_a_page_and_a_stylesheet() {
        let (_d, out) = site();
        assert!(out.join("index.html").is_file());
        assert!(out.join("style.css").is_file());
        // The page is named after a slug of the path relative to the root.
        assert!(out.join("src-shapes.html").is_file());
        // Nothing else: no JavaScript, no per-page assets.
        let mut names: Vec<_> = std::fs::read_dir(&out)
            .expect("readdir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["index.html", "src-shapes.html", "style.css"]);
    }

    /// Every link the index page makes has to land on a file that exists and an
    /// anchor that is in it. That is the property a flat output directory buys,
    /// and the one worth asserting.
    #[test]
    fn every_index_link_resolves() {
        let (_d, out) = site();
        let index = read(&out, "index.html");
        let mut checked = 0;
        for href in hrefs(&index) {
            let (file, anchor) = match href.split_once('#') {
                Some((f, a)) => (f, Some(a)),
                None => (href.as_str(), None),
            };
            let page = read(&out, file);
            if let Some(anchor) = anchor {
                assert!(
                    page.contains(&format!("id=\"{anchor}\"")),
                    "{file} has no anchor {anchor}"
                );
            }
            checked += 1;
        }
        assert!(checked >= 4, "only checked {checked} links");
    }

    #[test]
    fn a_page_shows_signatures_and_kinds() {
        let (_d, out) = site();
        let page = read(&out, "src-shapes.html");
        assert!(page.contains("pub fn area(s: Shape) -&#62; i64"), "{page}");
        assert!(page.contains("struct Point"));
        assert!(page.contains("variant Shape"));
        // A case is listed under its variant rather than as a section of its own.
        assert!(page.contains("Circle(r: i64)"));
    }

    /// A case's own `# ..` lines are rendered under it, the same way a
    /// declaration's are; a case without any is listed bare.
    #[test]
    fn a_case_shows_its_own_docs() {
        let (_d, out) = site();
        let page = read(&out, "src-shapes.html");
        let circle = page.find("Circle(r: i64)").expect("the case");
        let empty = page.find(">Empty<").expect("the bare case");
        let doc = page
            .find("<p>A circle of radius <code>r</code>.</p>")
            .expect("the case doc");
        assert!(circle < doc && doc < empty, "{page}");
        // The variant's own docs are above its cases, not merged into one.
        assert!(page.contains("<p>A shape.</p>"), "{page}");
        assert!(!page.contains("A shape. A circle"), "{page}");
    }

    /// A case with a documented slot lists its slots one by one under the
    /// case, each with its docs; an undocumented neighbour is listed bare, and
    /// a case with no slot docs has no list at all.
    #[test]
    fn a_case_lists_its_documented_slots() {
        let (_d, out) = site();
        let page = read(&out, "src-shapes.html");
        let rect = page.find("Rect(w: i64, h: i64)").expect("the case");
        let w = page
            .find("<code>w: i64</code>")
            .expect("the documented slot");
        let doc = page.find("<p>The width.</p>").expect("the slot doc");
        let h = page.find("<code>h: i64</code>").expect("the bare slot");
        assert!(rect < w && w < doc && doc < h, "{page}");
        // `Circle` documents no slot, so its `r` is not listed on its own.
        assert!(!page.contains("<code>r: i64</code>"), "{page}");
    }

    /// The two sections, and which side of the line each declaration lands on.
    /// `SRC` has three public declarations and one private helper.
    #[test]
    fn public_and_private_are_separate_sections() {
        let (_d, out) = site();
        let page = read(&out, "src-shapes.html");
        let public = page.find(">Public<").expect("a public section");
        let private = page.find(">Private<").expect("a private section");
        assert!(public < private, "public comes first:\n{page}");
        // The private helper is on the page, below the divide; the public ones
        // are above it.
        assert!(page.find("private_helper").expect("the helper") > private);
        for name in ["area", "Point", "Shape"] {
            let at = page.find(name).unwrap_or_else(|| panic!("{name}:\n{page}"));
            assert!(at < private, "{name} should be public:\n{page}");
        }
    }

    /// A file with nothing private gets no empty second section.
    #[test]
    fn a_section_with_nothing_in_it_is_not_rendered() {
        aipl_codegen::install_parser_hooks();
        let dir = tempdir::Dir::new("aipl-docs-pub");
        std::fs::write(dir.path().join("p.aipl"), "pub fn f() -> i64 { 1 }\n").expect("write");
        let mut index = Index::new();
        index
            .add(dir.path().join("p.aipl"), "pub fn f() -> i64 { 1 }\n")
            .expect("indexes");
        let out = dir.path().join("out");
        write_site(&index, dir.path(), "demo", &out).expect("writes");
        let page = read(&out, "p.html");
        assert!(page.contains(">Public<"), "{page}");
        assert!(!page.contains(">Private<"), "{page}");
    }

    /// The file's own `# ..` block leads the page, above both sections, and is
    /// rendered like any other documentation.
    #[test]
    fn a_page_shows_the_files_own_documentation() {
        let (_d, out) = site();
        let page = read(&out, "src-shapes.html");
        let doc = page
            .find("<p>Shapes, and the areas of them.</p>")
            .expect("the module doc");
        let public = page.find(">Public<").expect("a public section");
        assert!(doc < public, "the file's docs lead the page:\n{page}");
        // Every paragraph of it, not just the summary the index takes.
        assert!(page.contains("A second paragraph, which is on"), "{page}");
        // It documents no declaration, so it is not one of the items.
        assert!(page.contains("class=\"module-doc\""), "{page}");
    }

    /// The index page's one line per file is that file's own documentation,
    /// first paragraph only.
    #[test]
    fn the_index_summarises_a_file_from_its_own_documentation() {
        let (_d, out) = site();
        let index = read(&out, "index.html");
        assert!(
            index.contains("<p class=\"file-summary\">Shapes, and the areas of them.</p>"),
            "{index}"
        );
        // The rest of the block stays on the file's own page.
        assert!(!index.contains("A second paragraph"), "{index}");
    }

    /// A file with no `# ..` block of its own gets no introduction and no
    /// summary — not an empty one, and not an "Undocumented." the way an item
    /// does: a file whose declarations are documented is not undocumented.
    #[test]
    fn a_file_without_its_own_documentation_shows_none() {
        aipl_codegen::install_parser_hooks();
        let dir = tempdir::Dir::new("aipl-docs-nomod");
        let src = "# Just this one.\npub fn f() -> i64 { 1 }\n";
        std::fs::write(dir.path().join("p.aipl"), src).expect("write");
        let mut index = Index::new();
        index.add(dir.path().join("p.aipl"), src).expect("indexes");
        let out = dir.path().join("out");
        write_site(&index, dir.path(), "demo", &out).expect("writes");
        // The block is adjacent to the declaration, so it documents it.
        let page = read(&out, "p.html");
        assert!(page.contains("<p>Just this one.</p>"), "{page}");
        assert!(!page.contains("module-doc"), "{page}");
        assert!(!read(&out, "index.html").contains("file-summary"));
    }

    /// The summary is the first *paragraph*, since a doc block is prose wrapped
    /// wherever the author's column ran out.
    #[test]
    fn a_summary_is_the_first_paragraph() {
        assert_eq!(
            summary_html("One sentence\nwrapped over two lines.\n\nA second paragraph."),
            "One sentence wrapped over two lines."
        );
        // Inline code is rendered, and escaped, like anywhere else.
        assert_eq!(
            summary_html("see `Rule<K>`"),
            "see <code>Rule&lt;K&gt;</code>"
        );
        // Nothing to summarise: no docs, or a block opening with an example.
        assert_eq!(summary_html(""), "");
        assert_eq!(summary_html("    let a = 1;"), "");
    }

    /// Imports are indexed but deliberately not rendered.
    #[test]
    fn imports_are_not_shown() {
        let (_d, out) = site();
        let page = read(&out, "src-shapes.html");
        assert!(!page.contains("Imports"), "{page}");
        // `print` is imported by `SRC` and declared by nothing in it, so its
        // name appearing at all would mean the import list came back.
        assert!(!page.contains("print"), "{page}");
    }

    /// The doc renderer: paragraphs on blank lines, indented blocks kept as
    /// code, inline backticks as `<code>`.
    #[test]
    fn renders_doc_text_as_paragraphs_and_code() {
        let (_d, out) = site();
        let page = read(&out, "src-shapes.html");
        assert!(
            page.contains("<p>The area of <code>s</code>.</p>"),
            "{page}"
        );
        assert!(
            page.contains("<pre><code>let a = area(Circle(2));\n</code></pre>"),
            "{page}"
        );
        assert!(page.contains("<p>Rounded down, always.</p>"));
        // The undocumented ones say so rather than showing an empty block.
        assert!(page.contains("Undocumented."));
    }

    /// Doc text is data, not markup: a `<script>` in a doc comment must come
    /// out as text.
    #[test]
    fn escapes_doc_text() {
        assert_eq!(
            doc_html("a <script>alert(1)</script> & \"quoted\""),
            "<p>a &lt;script&gt;alert(1)&lt;/script&gt; &amp; &quot;quoted&quot;</p>"
        );
        // Escaping happens before backticks are looked for, so a `<` inside an
        // inline span is escaped too rather than closing the element.
        assert_eq!(
            doc_html("see `Rule<K>` please"),
            "<p>see <code>Rule&lt;K&gt;</code> please</p>"
        );
    }

    /// A `- ` run is a list, and its continuation lines belong to their item
    /// rather than opening a code block — the shape every file header in this
    /// repo writes.
    #[test]
    fn renders_dash_runs_as_lists() {
        assert_eq!(
            doc_html(
                "Three of them:\n\n  - one, which runs\n    over two lines.\n  - two.\n\nAfter."
            ),
            "<p>Three of them:</p><ul><li>one, which runs over two lines.</li>\
             <li>two.</li></ul><p>After.</p>"
        );
        // A four-space line with no list open is still a code block.
        assert_eq!(
            doc_html("Example:\n\n    let a = 1;"),
            "<p>Example:</p><pre><code>let a = 1;\n</code></pre>"
        );
        // A list is not a summary — the index shows prose or nothing.
        assert_eq!(summary_html("  - one\n  - two"), "");
    }

    /// `**bold**` and `*emphasis*`, which is what the prose uses to lead a
    /// paragraph and to stress a word.
    #[test]
    fn renders_emphasis() {
        assert_eq!(
            doc_html("**It is a PEG.** The driver has *no* left recursion."),
            "<p><strong>It is a PEG.</strong> The driver has <em>no</em> left recursion.</p>"
        );
        // Not markup: a lone marker, one with a space after it, and one inside
        // a code span — `i64*` is a variadic parameter, not an open emphasis.
        assert_eq!(doc_html("2*3 and a * b * c"), "<p>2*3 and a * b * c</p>");
        assert_eq!(
            doc_html("takes `xs: i64*` and `ys: i64*`"),
            "<p>takes <code>xs: i64*</code> and <code>ys: i64*</code></p>"
        );
        // An unpaired marker stays literal rather than swallowing the rest.
        assert_eq!(doc_html("a *stray marker"), "<p>a *stray marker</p>");
        // Emphasis routinely spans a code span, which is why code spans are
        // atoms of one pass rather than a pass of their own.
        assert_eq!(
            doc_html("**`Member` is the seam.** So is `Build<A>`."),
            "<p><strong><code>Member</code> is the seam.</strong> So is \
             <code>Build&lt;A&gt;</code>.</p>"
        );
        // And a `*word*` inside a `**lead-in**` is both.
        assert_eq!(
            doc_html("**A rule set is written against a token *kind* type.**"),
            "<p><strong>A rule set is written against a token <em>kind</em> type.\
             </strong></p>"
        );
    }

    #[test]
    fn an_unbalanced_backtick_still_produces_well_formed_markup() {
        let html = doc_html("a ` stray tick");
        assert_eq!(
            html.matches("<code>").count(),
            html.matches("</code>").count()
        );
        // It stays where it was written, too, rather than turning the rest of
        // the paragraph into code.
        assert_eq!(html, "<p>a ` stray tick</p>");
    }

    /// Doubled delimiters are how prose names a backtick, and a span reading
    /// them as two spans would shift every span after it in the paragraph.
    #[test]
    fn a_doubled_delimiter_holds_a_backtick() {
        assert_eq!(
            doc_html("a lone `` ` `` or a `RawTemplate*` kind"),
            "<p>a lone <code>`</code> or a <code>RawTemplate*</code> kind</p>"
        );
        // Only a run of the same length closes it.
        assert_eq!(doc_html("`` a ` b ``"), "<p><code>a ` b</code></p>");
    }

    #[test]
    fn slugs_are_unique_even_when_paths_collide() {
        aipl_codegen::install_parser_hooks();
        let src = "fn f() -> i64 { 1 }\n";
        let mut index = Index::new();
        // `a/b.aipl` and `a-b.aipl` both slug to `a-b`.
        index.add(PathBuf::from("root/a/b.aipl"), src).expect("a/b");
        index.add(PathBuf::from("root/a-b.aipl"), src).expect("a-b");
        let slugs = slugs(&index, Path::new("root"));
        let mut values: Vec<_> = slugs.values().cloned().collect();
        values.sort();
        assert_eq!(values, ["a-b", "a-b-2"]);
    }

    #[test]
    fn an_empty_index_still_writes_a_site() {
        let dir = tempdir::Dir::new("aipl-docs-empty");
        let out = dir.path().join("out");
        write_site(&Index::new(), dir.path(), "nothing", &out).expect("writes");
        assert!(read(&out, "index.html").contains("No <code>.aipl</code> files found."));
        assert!(out.join("style.css").is_file());
    }

    /// Every `href="..."` in `html`.
    fn hrefs(html: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = html;
        while let Some(i) = rest.find("href=\"") {
            rest = &rest[i + 6..];
            let Some(end) = rest.find('"') else { break };
            let href = &rest[..end];
            // The stylesheet is not a page, and neither is the header's
            // self-link, which `index.html` resolves to itself.
            if href != "style.css" {
                out.push(href.to_string());
            }
            rest = &rest[end..];
        }
        out
    }

    /// A temp directory that removes itself. Small enough to keep here rather
    /// than take a dependency for.
    mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new(tag: &str) -> Dir {
                let base = std::env::temp_dir().join(format!(
                    "{tag}-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&base);
                std::fs::create_dir_all(&base).expect("temp dir");
                Dir(base)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
