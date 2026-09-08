//! A documentation site for AIPL source: static HTML and one stylesheet,
//! written to a directory.
//!
//! The first consumer of [`aipl_index`], and deliberately a thin one — it asks
//! the index for symbols, docs and imports, and spends its own effort on
//! presentation. Anything it needed that the index could not answer would be a
//! gap in the index rather than something to work around here.
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
        listings.push(FileListing {
            display: display_path(&file.path, root),
            href: format!("{slug}.html"),
            entries: page.entries(),
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
    entries: Vec<Entry>,
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
    imports: Vec<ImportRow>,
    symbols: Vec<SymbolSection>,
}

struct ImportRow {
    name: String,
    local: String,
    aliased: bool,
    from: String,
}

struct SymbolSection {
    name: String,
    anchor: String,
    kind: &'static str,
    kind_class: &'static str,
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
}

impl FilePage {
    fn build(file: &FileIndex, root: &Path, project: &str, slug: &str) -> FilePage {
        let display = display_path(&file.path, root);
        let mut symbols: Vec<SymbolSection> = Vec::new();
        for sym in &file.symbols {
            // A case is shown under the variant that declares it rather than as
            // a section of its own: it is one alternative of a type, not a
            // separate thing to read about.
            if sym.kind == SymbolKind::Case {
                if let Some(parent) = symbols.last_mut() {
                    parent.cases.push(CaseRow {
                        anchor: anchor(&sym.name),
                        detail: sym.detail.clone(),
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
        let imports = file
            .imports
            .iter()
            .map(|i| ImportRow {
                name: i.name.clone(),
                local: i.local.clone(),
                aliased: i.local != i.name,
                from: i.from.clone().unwrap_or_else(|| "builtins".to_string()),
            })
            .collect();
        let _ = slug;
        FilePage {
            title: format!("{display} — {project}"),
            project: project.to_string(),
            display,
            imports,
            symbols,
        }
    }

    /// The one-line-per-item summary the index page shows for this file.
    fn entries(&self) -> Vec<Entry> {
        self.symbols
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
    }
}

fn kind_class(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "kind-fn",
        SymbolKind::Struct => "kind-struct",
        SymbolKind::Variant => "kind-variant",
        SymbolKind::Case => "kind-case",
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
        }
    }
    out
}

enum Block<'a> {
    Para(Vec<&'a str>),
    Code(Vec<&'a str>),
}

/// Split doc text into paragraphs and indented code blocks. A blank line ends
/// either; indentation of four spaces or more starts a code block.
fn blocks(doc: &str) -> Vec<Block<'_>> {
    let indented_line = |l: &str| l.starts_with("    ") && !l.trim().is_empty();
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
        // block ends the moment the indentation does.
        let continues = match &open {
            Some(Block::Para(_)) => true,
            Some(Block::Code(_)) => indented_line(line),
            None => false,
        };
        if !continues {
            out.extend(open.take());
            open = Some(if indented_line(line) {
                Block::Code(Vec::new())
            } else {
                Block::Para(Vec::new())
            });
        }
        match open.as_mut() {
            Some(Block::Code(lines)) => lines.push(line),
            Some(Block::Para(lines)) => lines.push(line.trim()),
            None => {}
        }
    }
    out.extend(open);
    out
}

/// Escape `text`, then turn `` `spans` `` into `<code>` elements. Escaping
/// first is what makes this safe: by the time a backtick is looked for, every
/// `<`, `&` and `"` in the text is already an entity.
fn inline(text: &str) -> String {
    let escaped = escape(text);
    let mut out = String::new();
    // Splitting on the delimiter puts the spans at the odd indices: `a `b` c`
    // is ["a ", "b", " c"]. An odd number of backticks leaves the final piece at
    // an odd index and so wrapped — a stray backtick swallows the rest of the
    // paragraph into a `<code>`, which looks wrong but is at least well-formed.
    for (i, piece) in escaped.split('`').enumerate() {
        if i % 2 == 1 {
            out.push_str("<code>");
            out.push_str(piece);
            out.push_str("</code>");
        } else {
            out.push_str(piece);
        }
    }
    out
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

    const SRC: &str = r#"import { print } from builtins;

struct Point { x: i64, y: i64 }

variant Shape = Circle(r: i64) | Empty

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
    fn a_page_shows_signatures_kinds_and_visibility() {
        let (_d, out) = site();
        let page = read(&out, "src-shapes.html");
        assert!(page.contains("pub fn area(s: Shape) -&#62; i64"), "{page}");
        assert!(page.contains("struct Point"));
        assert!(page.contains("variant Shape"));
        // A case is listed under its variant rather than as a section of its own.
        assert!(page.contains("Circle(r: i64)"));
        // `pub` is badged; the private helper is not.
        assert_eq!(page.matches("<span class=\"badge\">pub</span>").count(), 3);
        assert!(page.contains("private_helper"));
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

    #[test]
    fn an_unbalanced_backtick_still_produces_well_formed_markup() {
        let html = doc_html("a ` stray tick");
        assert_eq!(
            html.matches("<code>").count(),
            html.matches("</code>").count()
        );
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
