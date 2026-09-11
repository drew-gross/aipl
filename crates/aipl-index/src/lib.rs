//! What a *tool* needs to know about AIPL source, as opposed to what a compiler
//! needs: where each name is declared, what it is, what it says about itself,
//! and where else it is mentioned.
//!
//! A docs site generator, an editor extension with go-to-definition, and a
//! language server all want the same three things, and none of them wants to
//! parse AIPL to get them. So they are gathered once here:
//!
//! - [`Symbol`] — every top-level declaration: its kind, its rendered
//!   signature, its `# ..` documentation, whether it is `pub`, and the span of
//!   the *name* (what an editor jumps to).
//! - [`Import`] — every imported name, its local spelling when aliased, and
//!   where it came from. This is what makes go-to-definition cross files.
//! - [`Reference`] — every identifier occurrence in the file, so a cursor
//!   offset can be turned into a name.
//!
//! # Where the positions come from
//!
//! The AST is the source of names, kinds, signatures and docs. It is *not* the
//! source of positions: no item in `aipl_syntax::ast` carries a span — only
//! `ImportName` does — because the compiler never needed one. Rather than widen
//! the AST, positions are read from the token stream, which has them exactly.
//!
//! That works because a declaration's shape is unmistakable *in tokens*: an item
//! is introduced by the keyword `fn`, `struct` or `variant`, and the identifier
//! after it is the declared name. A keyword token is a keyword — the lexer has
//! already decided, so the `fn` inside a string or a comment is not one — and
//! none of the three can appear anywhere but at the head of an item. Introducers
//! and AST items are then both in source order, so they zip: the *n*th
//! introducer belongs to the *n*th non-import item. Nothing is matched by name,
//! so two declarations sharing a name (an error the compiler reports separately)
//! cannot cross-wire the index.
//!
//! # What is not here yet
//!
//! Struct fields and function parameters are not symbols — only top-level items
//! and variant cases are. Fields are the obvious next layer and want the same
//! treatment (a name token at a known position inside the declaration); they are
//! left out rather than guessed at. Nothing here resolves types, so "go to the
//! definition of this type" works only because a type is written as a name that
//! is also a declaration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aipl_syntax::ast::{Item, Program, Type};
use aipl_syntax::{type_name, Error, Span};

/// What a declaration is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Variant,
    /// One case of a `variant` — `Circle` in `variant Shape = Circle(i64)`.
    /// Its own kind rather than a field of [`SymbolKind::Variant`] because a
    /// case is what source *mentions*: a constructor call and a match pattern
    /// both name the case, not the variant.
    Case,
}

impl SymbolKind {
    /// The keyword that introduces this kind of declaration, or `None` for a
    /// variant case, which is introduced by `=` or `|` rather than a keyword.
    fn keyword(self) -> Option<&'static str> {
        match self {
            SymbolKind::Function => Some("fn"),
            SymbolKind::Struct => Some("struct"),
            SymbolKind::Variant => Some("variant"),
            SymbolKind::Case => None,
        }
    }
}

/// One top-level declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// The declaration rendered on one line — `fn add(a: i64, b: i64) -> i64`,
    /// `variant Shape`, `Circle(i64)`. What a hover shows above the docs, and
    /// what a docs page uses as a heading.
    pub detail: String,
    /// The declaration's documentation: its `# ..` lines, joined with newlines,
    /// the `# ..` lines above the declaration. `None` when it is
    /// undocumented.
    pub doc: Option<String>,
    /// Declared `pub`, and so importable by another file. Always false for a
    /// case, whose visibility is its variant's.
    pub is_pub: bool,
    /// The span of the declared *name* — where "go to definition" lands, and
    /// what an editor highlights when renaming.
    pub name_span: Span,
    /// For a [`SymbolKind::Case`], the variant it belongs to.
    pub parent: Option<String>,
}

/// One name brought into a file by an `import`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The name as the *defining* file spells it — what to look up over there.
    pub name: String,
    /// The name as *this* file spells it: the alias when there is one
    /// (`equal as ==` binds `==`), otherwise the same as `name`.
    pub local: String,
    /// The file it came from, or `None` for `from builtins`.
    pub from: Option<String>,
    pub span: Span,
}

/// One identifier occurrence. Every identifier token in the file, declarations
/// included — an editor asks "what is under the cursor" without knowing whether
/// the cursor is on a use or on the definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub span: Span,
}

/// Everything one file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIndex {
    pub path: PathBuf,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
    pub references: Vec<Reference>,
}

impl FileIndex {
    /// Index `src` as the contents of `path`.
    ///
    /// Trailing `--- section ---` blocks are stripped first, so a test-case file
    /// indexes as the AIPL it contains. Fails only when the source does not
    /// parse; nothing here needs the file to compile, link or run, so a file
    /// with type errors still indexes — which is most of what an editor is
    /// looking at.
    ///
    /// **The caller must have installed the parser hooks**
    /// (`aipl::install_parser_hooks`, idempotent) before the first call: the
    /// parser reaches its dogfooded AIPL helpers through them and has no native
    /// fallback. Installing a process-global is the host's decision, so this
    /// library does not make it.
    pub fn parse(path: impl Into<PathBuf>, src: &str) -> Result<FileIndex, Error> {
        let path = path.into();
        let stripped = aipl_parser::strip_test_sections(src);
        let program = aipl_parser::parse(stripped)?;
        let tokens = aipl_parser::lex_tokens(stripped)?;
        Ok(FileIndex {
            path,
            symbols: symbols(&program, &tokens, stripped),
            imports: imports(&program),
            references: references(&tokens, stripped),
        })
    }

    /// The declaration named `name`, if this file has one.
    pub fn define(&self, name: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|s| s.name == name)
    }

    /// The identifier at byte `offset`, if the offset is inside one. The
    /// question an editor actually asks: it has a cursor, not a name.
    pub fn reference_at(&self, offset: usize) -> Option<&Reference> {
        self.references
            .iter()
            .find(|r| r.span.start <= offset && offset < r.span.end)
    }

    /// The import that binds `name` in this file, if any.
    pub fn import_of(&self, name: &str) -> Option<&Import> {
        self.imports.iter().find(|i| i.local == name)
    }

    /// Every symbol, in source order — the document outline an editor shows.
    /// Cases follow their variant, which is the order they are declared in.
    pub fn outline(&self) -> &[Symbol] {
        &self.symbols
    }
}

/// A set of indexed files, so a lookup can cross file boundaries.
#[derive(Debug, Clone, Default)]
pub struct Index {
    files: BTreeMap<PathBuf, FileIndex>,
}

impl Index {
    pub fn new() -> Index {
        Index::default()
    }

    /// Index `src` under `path`, replacing any previous index for it — an
    /// editor re-indexes the file it is editing on every keystroke it can
    /// afford to.
    pub fn add(&mut self, path: impl Into<PathBuf>, src: &str) -> Result<(), Error> {
        let file = FileIndex::parse(path, src)?;
        self.files.insert(file.path.clone(), file);
        Ok(())
    }

    pub fn file(&self, path: impl AsRef<Path>) -> Option<&FileIndex> {
        self.files.get(path.as_ref())
    }

    pub fn files(&self) -> impl Iterator<Item = &FileIndex> {
        self.files.values()
    }

    /// Go to definition: the declaration of whatever identifier sits at byte
    /// `offset` of `path`, and the file that declares it.
    ///
    /// Resolution order is the one the loader uses: a name declared in this file
    /// wins, and otherwise an `import` says which file to look in. A name
    /// imported `from builtins`, or from a file this index has not been given,
    /// resolves to nothing — the caller is told "no definition", which is the
    /// honest answer rather than a guess.
    pub fn definition_at(&self, path: impl AsRef<Path>, offset: usize) -> Option<(&Path, &Symbol)> {
        let file = self.file(path)?;
        let name = &file.reference_at(offset)?.name;
        self.definition_of(file, name)
    }

    /// The declaration of `name` as `file` sees it.
    pub fn definition_of<'a>(
        &'a self,
        file: &'a FileIndex,
        name: &str,
    ) -> Option<(&'a Path, &'a Symbol)> {
        if let Some(sym) = file.define(name) {
            return Some((file.path.as_path(), sym));
        }
        let import = file.import_of(name)?;
        let from = import.from.as_ref()?;
        // An import path is relative to the importing file's directory, exactly
        // as the loader resolves it.
        let dir = file.path.parent().unwrap_or_else(|| Path::new(""));
        let target = self.file(dir.join(from))?;
        let sym = target.define(&import.name)?;
        Some((target.path.as_path(), sym))
    }
}

/// Every declaration in `program`, in source order, with its name's span taken
/// from `tokens`.
fn symbols(program: &Program, tokens: &[(aipl_parser::TokenKind, Span)], src: &str) -> Vec<Symbol> {
    let mut spans = NameSpans::new(tokens, src);
    let mut out = Vec::new();
    for item in &program.items {
        match item {
            Item::Fn(f) => {
                let Some(name_span) = spans.next_named(SymbolKind::Function, &f.name) else {
                    continue;
                };
                out.push(Symbol {
                    name: f.name.clone(),
                    kind: SymbolKind::Function,
                    detail: fn_detail(f),
                    doc: f.doc.clone(),
                    is_pub: f.is_pub,
                    name_span,
                    parent: None,
                });
            }
            Item::Struct(s) => {
                let Some(name_span) = spans.next_named(SymbolKind::Struct, &s.name) else {
                    continue;
                };
                out.push(Symbol {
                    name: s.name.clone(),
                    kind: SymbolKind::Struct,
                    detail: format!("struct {}{}", s.name, type_vars(&s.type_vars)),
                    doc: s.doc.clone(),
                    is_pub: true,
                    name_span,
                    parent: None,
                });
            }
            Item::Variant(v) => {
                let Some(name_span) = spans.next_named(SymbolKind::Variant, &v.name) else {
                    continue;
                };
                out.push(Symbol {
                    name: v.name.clone(),
                    kind: SymbolKind::Variant,
                    detail: format!("variant {}{}", v.name, type_vars(&v.type_vars)),
                    doc: v.doc.clone(),
                    is_pub: true,
                    name_span,
                    parent: None,
                });
                for case in &v.cases {
                    let Some(case_span) = spans.next_case(&case.name) else {
                        continue;
                    };
                    // A named slot shows its name, and a keyword slot (one
                    // with a default) shows that it has one — `Many(Rule<K>,
                    // min: u64 = ..)` says far more about how the case is
                    // constructed than three bare types do. The default's
                    // *value* is an expression the AST holds unrendered, so it
                    // is shown as `..` rather than guessed at.
                    let payload = case
                        .payload
                        .iter()
                        .map(|p| match (&p.name, &p.default) {
                            (Some(n), Some(_)) => format!("{n}: {} = ..", type_name(&p.ty)),
                            (Some(n), None) => format!("{n}: {}", type_name(&p.ty)),
                            (None, _) => type_name(&p.ty),
                        })
                        .collect::<Vec<_>>();
                    let detail = if payload.is_empty() {
                        case.name.clone()
                    } else {
                        format!("{}({})", case.name, payload.join(", "))
                    };
                    out.push(Symbol {
                        name: case.name.clone(),
                        kind: SymbolKind::Case,
                        detail,
                        doc: case.doc.clone(),
                        is_pub: true,
                        name_span: case_span,
                        parent: Some(v.name.clone()),
                    });
                }
            }
            Item::Import(_) => {}
        }
    }
    out
}

/// Walks the token stream handing out declared-name spans in source order.
///
/// It is a cursor rather than a lookup table because the AST and the token
/// stream are both in source order, so the *n*th introducer is the *n*th item —
/// matching by name would instead have to decide what to do about two
/// declarations sharing one.
struct NameSpans<'a> {
    tokens: &'a [(aipl_parser::TokenKind, Span)],
    src: &'a str,
    at: usize,
}

impl<'a> NameSpans<'a> {
    fn new(tokens: &'a [(aipl_parser::TokenKind, Span)], src: &'a str) -> NameSpans<'a> {
        NameSpans { tokens, src, at: 0 }
    }

    /// The span of the next name introduced by `kind`'s keyword. `expected` is
    /// the name the AST says is there; a mismatch means the two streams have
    /// desynchronized, and the symbol is dropped rather than given a wrong
    /// position.
    fn next_named(&mut self, kind: SymbolKind, expected: &str) -> Option<Span> {
        let keyword = kind.keyword()?;
        while self.at < self.tokens.len() {
            let (k, span) = &self.tokens[self.at];
            self.at += 1;
            if *k != aipl_parser::TokenKind::Keyword || self.text(span) != keyword {
                continue;
            }
            let (nk, nspan) = self.tokens.get(self.at)?;
            if *nk == aipl_parser::TokenKind::Identifier && self.text(nspan) == expected {
                self.at += 1;
                return Some(nspan.clone());
            }
            return None;
        }
        None
    }

    /// The span of the next variant case name: the identifier right after the
    /// `=` opening the case list or a `|` separating cases. Those are the only
    /// two tokens a case name can follow, and neither can occur inside a case's
    /// payload — a type holds no `|`, and `||` lexes as one token.
    fn next_case(&mut self, expected: &str) -> Option<Span> {
        while self.at < self.tokens.len() {
            let (k, span) = &self.tokens[self.at];
            self.at += 1;
            let text = self.text(span);
            if *k != aipl_parser::TokenKind::Operator || (text != "=" && text != "|") {
                continue;
            }
            let (nk, nspan) = self.tokens.get(self.at)?;
            if *nk == aipl_parser::TokenKind::Identifier && self.text(nspan) == expected {
                self.at += 1;
                return Some(nspan.clone());
            }
        }
        None
    }

    fn text(&self, span: &Span) -> &str {
        self.src.get(span.start..span.end).unwrap_or("")
    }
}

fn type_vars(vars: &[aipl_syntax::ast::TypeParam]) -> String {
    if vars.is_empty() {
        return String::new();
    }
    let inner = vars
        .iter()
        .map(|v| format!("{}: {}", v.name, v.bound.name()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("<{inner}>")
}

fn ty(t: &Option<Type>) -> String {
    match t {
        Some(t) => format!(" -> {}", type_name(t)),
        None => String::new(),
    }
}

fn fn_detail(f: &aipl_syntax::ast::Function) -> String {
    let params = f
        .sig
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, type_name(&p.ty)))
        .collect::<Vec<_>>()
        .join(", ");
    let effects = f
        .sig
        .effects
        .iter()
        .map(|e| format!(" !{e}"))
        .collect::<String>();
    format!(
        "{}fn {}{}({}){}{}",
        if f.is_pub { "pub " } else { "" },
        f.name,
        type_vars(&f.sig.type_vars),
        params,
        effects,
        ty(&f.sig.return_ty)
    )
}

fn imports(program: &Program) -> Vec<Import> {
    let mut out = Vec::new();
    for item in &program.items {
        let Item::Import(decl) = item else { continue };
        let from = match &decl.source {
            aipl_syntax::ast::ImportSource::Path { path, .. } => Some(path.clone()),
            aipl_syntax::ast::ImportSource::Builtins { .. } => None,
        };
        for n in &decl.names {
            out.push(Import {
                name: n.name.clone(),
                local: n.alias.clone().unwrap_or_else(|| n.name.clone()),
                from: from.clone(),
                span: n.span.clone(),
            });
        }
    }
    out
}

fn references(tokens: &[(aipl_parser::TokenKind, Span)], src: &str) -> Vec<Reference> {
    tokens
        .iter()
        .filter(|(k, _)| *k == aipl_parser::TokenKind::Identifier)
        .map(|(_, span)| Reference {
            name: src[span.start..span.end].to_string(),
            span: span.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"import { wrapping_add as +, print } from builtins;
import { Cst, show as render } from "./cst.aipl";

struct Point { x: i64, y: i64 }

variant Shape<T: any> = Circle(i64) | Rect(i64, i64) | Empty

# Adds two integers.
pub fn add(a: i64, b: i64) -> i64 { a + b }

fn helper(n: i64) !prints -> i64 {
    print("hi");
    add(n, 1)
}
"#;

    /// The tests are this library's host, so they install the parser hooks the
    /// way a real one would. Idempotent, so every test can just call it.
    ///
    /// Every test goes through this or [`hosted_index`] rather than calling
    /// `Index::new` itself — one that forgot passed under `cargo test`, where
    /// the whole suite shares a process and some *other* test had already
    /// installed the hooks, and failed only under `cargo nextest`, which gives
    /// each test its own. Routing construction through here is what stops that
    /// being a thing to remember.
    fn hosted() {
        aipl_codegen::install_parser_hooks();
    }

    /// An empty [`Index`], with the hooks installed.
    fn hosted_index() -> Index {
        hosted();
        Index::new()
    }

    fn index() -> FileIndex {
        hosted();
        FileIndex::parse("src/demo.aipl", SRC).expect("indexes")
    }

    /// The offset of the `n`th *whole-word* occurrence of `needle`.
    ///
    /// Whole-word matters: a plain substring search finds `add` inside
    /// `wrapping_add` and `print` inside `!prints`, and would have this helper
    /// disagree with the index about which occurrence is which.
    fn nth(needle: &str, n: usize) -> usize {
        let word = |c: char| c.is_alphanumeric() || c == '_';
        SRC.match_indices(needle)
            .filter(|(i, _)| {
                !SRC[..*i].chars().next_back().is_some_and(word)
                    && !SRC[i + needle.len()..].chars().next().is_some_and(word)
            })
            .nth(n)
            .expect("occurrence")
            .0
    }

    #[test]
    fn finds_every_declaration_in_source_order() {
        let names: Vec<_> = index()
            .outline()
            .iter()
            .map(|s| (s.kind, s.name.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                (SymbolKind::Struct, "Point".into()),
                (SymbolKind::Variant, "Shape".into()),
                (SymbolKind::Case, "Circle".into()),
                (SymbolKind::Case, "Rect".into()),
                (SymbolKind::Case, "Empty".into()),
                (SymbolKind::Function, "add".into()),
                (SymbolKind::Function, "helper".into()),
            ]
        );
    }

    /// The whole point of the token walk: a name's span is the name, not the
    /// declaration and not the keyword.
    #[test]
    fn a_name_span_covers_exactly_the_name() {
        let idx = index();
        for sym in idx.outline() {
            let text = &SRC[sym.name_span.start..sym.name_span.end];
            assert_eq!(text, sym.name, "span of {:?} covers {text:?}", sym.name);
        }
        // ... and it is the *declaring* occurrence, not a later mention. `add`
        // is declared on line 7 and called on line 11.
        let add = idx.define("add").expect("add");
        assert_eq!(add.name_span.start, nth("add", 0));
        assert!(add.name_span.start < nth("add", 1));
    }

    #[test]
    fn renders_a_signature_and_keeps_the_doc() {
        let idx = index();
        let add = idx.define("add").expect("add");
        assert_eq!(add.detail, "pub fn add(a: i64, b: i64) -> i64");
        assert_eq!(add.doc.as_deref(), Some("Adds two integers."));
        assert!(add.is_pub);

        // A private function, with an effect and no doc.
        let helper = idx.define("helper").expect("helper");
        assert_eq!(helper.detail, "fn helper(n: i64) !prints -> i64");
        assert_eq!(helper.doc, None);
        assert!(!helper.is_pub);

        // Generic parameters and their bounds show in the heading.
        assert_eq!(
            idx.define("Shape").expect("Shape").detail,
            "variant Shape<T: any>"
        );
        assert_eq!(idx.define("Point").expect("Point").detail, "struct Point");
    }

    #[test]
    fn a_case_carries_its_payload_and_its_variant() {
        let idx = index();
        hosted();
        let circle = idx.define("Circle").expect("Circle");
        assert_eq!(circle.detail, "Circle(i64)");
        assert_eq!(circle.parent.as_deref(), Some("Shape"));
        assert_eq!(idx.define("Rect").expect("Rect").detail, "Rect(i64, i64)");
        // A named slot shows its name; a keyword slot shows that it has a
        // default, which is how the case is actually constructed.
        let named = "variant R = Many(inner: i64, min: u64 = 0) | Plain(str)\n";
        let idx2 = FileIndex::parse("src/n.aipl", named).expect("indexes");
        assert_eq!(
            idx2.define("Many").expect("Many").detail,
            "Many(inner: i64, min: u64 = ..)"
        );
        assert_eq!(idx2.define("Plain").expect("Plain").detail, "Plain(str)");
        // A nullary case is just its name — and the one after a payload case,
        // which is where a naive `|` scan would drift.
        assert_eq!(idx.define("Empty").expect("Empty").detail, "Empty");
    }

    #[test]
    fn records_imports_with_their_local_spelling() {
        let idx = index();
        // An operator alias binds the operator spelling, not the builtin's name.
        let plus = idx.import_of("+").expect("+");
        assert_eq!(plus.name, "wrapping_add");
        assert_eq!(plus.from, None); // from builtins
                                     // An unaliased import binds its own name.
        assert_eq!(idx.import_of("print").expect("print").name, "print");
        // A path import remembers the file.
        let cst = idx.import_of("Cst").expect("Cst");
        assert_eq!(cst.from.as_deref(), Some("./cst.aipl"));
        // ... and an alias over one binds the local spelling.
        let render = idx.import_of("render").expect("render");
        assert_eq!(render.name, "show");
        assert_eq!(render.from.as_deref(), Some("./cst.aipl"));
    }

    #[test]
    fn turns_a_cursor_offset_into_a_name() {
        let idx = index();
        let call = nth("add", 1); // the call inside `helper`
        assert_eq!(idx.reference_at(call).expect("ref").name, "add");
        // Anywhere inside the identifier, not just its first byte.
        assert_eq!(idx.reference_at(call + 2).expect("ref").name, "add");
        // Just past it is not inside it.
        assert!(idx.reference_at(call + 3).is_none_or(|r| r.name != "add"));
        // A cursor in whitespace is on no identifier at all — here the newline
        // just before `struct`.
        assert!(idx.reference_at(nth("struct", 0) - 1).is_none());
        // Nor is a keyword one, even though it is spelled like a word.
        assert!(idx.reference_at(nth("struct", 0)).is_none());
    }

    #[test]
    fn go_to_definition_within_a_file() {
        let mut index = hosted_index();
        index.add("src/demo.aipl", SRC).expect("indexes");
        let call = nth("add", 1);
        let (path, sym) = index
            .definition_at("src/demo.aipl", call)
            .expect("definition");
        assert_eq!(path, Path::new("src/demo.aipl"));
        assert_eq!(sym.name, "add");
        assert_eq!(sym.name_span.start, nth("add", 0));
    }

    #[test]
    fn go_to_definition_follows_an_import_across_files() {
        let cst = "# Renders.\npub fn show(n: i64) -> str { \"x\" }\n";
        let mut index = hosted_index();
        index.add("src/demo.aipl", SRC).expect("demo");
        index.add("src/cst.aipl", cst).expect("cst");

        // `render` here is `show` over there — the alias is followed, and the
        // answer names the other file.
        let at = nth("render", 0);
        let (path, sym) = index
            .definition_at("src/demo.aipl", at)
            .expect("cross-file definition");
        assert_eq!(path, Path::new("src/cst.aipl"));
        assert_eq!(sym.name, "show");
        assert_eq!(sym.doc.as_deref(), Some("Renders."));
    }

    #[test]
    fn an_unresolvable_name_is_no_definition_rather_than_a_guess() {
        let mut index = hosted_index();
        index.add("src/demo.aipl", SRC).expect("demo");
        // A builtin: imported, but this index holds no file that declares it.
        assert!(index
            .definition_at("src/demo.aipl", nth("print", 1))
            .is_none());
        // An import whose file was never added.
        assert!(index
            .definition_at("src/demo.aipl", nth("Cst", 0))
            .is_none());
    }

    /// A file that does not compile still indexes: nothing here type-checks, so
    /// an editor keeps working on source that is mid-edit.
    #[test]
    fn indexes_source_that_would_not_compile() {
        hosted();
        let src = "fn f(n: NoSuchType) -> AlsoMissing { g() }\n";
        let idx = FileIndex::parse("src/broken.aipl", src).expect("indexes anyway");
        assert_eq!(
            idx.define("f").expect("f").detail,
            "fn f(n: NoSuchType) -> AlsoMissing"
        );
    }

    /// A trailing `--- section ---` block is harness data, not source.
    #[test]
    fn ignores_trailing_harness_sections() {
        hosted();
        let src = "fn f() -> i64 { 1 }\n--- stdout ---\nfn not_a_declaration() {}\n";
        let idx = FileIndex::parse("src/cased.aipl", src).expect("indexes");
        assert_eq!(idx.outline().len(), 1);
        assert_eq!(idx.outline()[0].name, "f");
    }

    /// Source that does not parse is an error, not a partial index — a caller
    /// that gets `Ok` can trust every span in it.
    #[test]
    fn refuses_source_that_does_not_parse() {
        hosted();
        assert!(FileIndex::parse("src/bad.aipl", "fn f( {").is_err());
    }
}

/// The index over the repository's own AIPL, which is the only corpus big
/// enough to disagree with a hand-written fixture.
///
/// Its own module because it reads files from disk, where the tests above are
/// self-contained strings.
#[cfg(test)]
mod corpus {
    use super::*;

    fn aipl_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                aipl_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "aipl") {
                out.push(p);
            }
        }
    }

    /// Every `.aipl` in the repository indexes, and every name span it reports
    /// really covers that name in the file.
    ///
    /// This is the assertion that would catch the token walk drifting out of
    /// step with the AST: a desynchronized cursor hands back some *other*
    /// identifier's span, which reads back as the wrong text.
    #[test]
    fn indexes_the_whole_repository_with_correct_spans() {
        aipl_codegen::install_parser_hooks();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root");
        let mut files = Vec::new();
        for sub in ["crates", "examples", "tests/cases"] {
            aipl_files(&root.join(sub), &mut files);
        }
        files.sort();
        assert!(files.len() > 400, "corpus went missing? {}", files.len());

        let mut indexed = 0usize;
        let mut symbols = 0usize;
        for path in &files {
            let src = std::fs::read_to_string(path).expect("read");
            // Error fixtures are deliberately unparseable; they say nothing
            // about whether the index is right.
            let Ok(idx) = FileIndex::parse(path.clone(), &src) else {
                continue;
            };
            indexed += 1;
            symbols += idx.symbols.len();
            let stripped = aipl_parser::strip_test_sections(&src);
            for sym in &idx.symbols {
                let text = stripped
                    .get(sym.name_span.start..sym.name_span.end)
                    .unwrap_or("<out of range>");
                assert_eq!(
                    text,
                    sym.name,
                    "{}: {:?} name span covers {text:?}",
                    path.display(),
                    sym.name
                );
            }
        }
        assert!(indexed > 400, "only indexed {indexed} files");
        assert!(symbols > 2000, "only found {symbols} symbols");
    }

    /// Go-to-definition across the parser library's real files: `parse.aipl`
    /// imports `Literal` from `grammar.aipl`, and the index finds the variant
    /// case it names.
    #[test]
    fn resolves_a_real_cross_file_import() {
        aipl_codegen::install_parser_hooks();
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates dir")
            .join("aipl-codegen")
            .join("src");
        let mut index = Index::new();
        for name in ["parse.aipl", "grammar.aipl", "cst.aipl"] {
            let path = src_dir.join(name);
            let src = std::fs::read_to_string(&path).expect("read");
            index.add(path, &src).expect("indexes");
        }

        let parse = index.file(src_dir.join("parse.aipl")).expect("parse.aipl");
        let (path, sym) = index
            .definition_of(parse, "Literal")
            .expect("Literal resolves");
        assert_eq!(path, src_dir.join("grammar.aipl"));
        assert_eq!(sym.kind, SymbolKind::Case);
        assert_eq!(sym.parent.as_deref(), Some("Rule"));
        assert_eq!(sym.detail, "Literal(str)");

        // A function, with the doc a hover would show.
        let (path, link) = index.definition_of(parse, "link").expect("link resolves");
        assert_eq!(path, src_dir.join("grammar.aipl"));
        assert_eq!(link.kind, SymbolKind::Function);
        assert!(link.is_pub);
        let doc = link.doc.as_deref().expect("link is documented");
        assert!(doc.contains("every rule reference resolved"), "{doc}");
    }
}
