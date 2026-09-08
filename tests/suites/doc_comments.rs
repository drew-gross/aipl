//! `# text` doc comments: the documentation form that attaches to any
//! declaration, not just to a function.
//!
//! The user-visible rules live in `tests/cases/docs/`, where a reviewer sees
//! them as source and diagnostic. What is left here is what a case cannot
//! express: the doc *text* a declaration ends up carrying, which nothing a
//! program can print exposes — it reaches the world through `aipl doc`, the
//! docs site, and the source index, each of which reads the AST field.

use aipl::ast::Item;

fn parse(src: &str) -> aipl::ast::Program {
    aipl::install_parser_hooks();
    aipl::parse(src).unwrap_or_else(|e| panic!("{}", e.render(src, "test.aipl")))
}

fn err(src: &str) -> String {
    aipl::install_parser_hooks();
    aipl::parse(src)
        .err()
        .map(|e| e.message)
        .unwrap_or_else(|| {
            panic!("expected a failure, but this parsed:\n{src}");
        })
}

/// The doc of the one item in `src`, whatever kind it is.
fn doc_of(src: &str) -> Option<String> {
    match parse(src).items.into_iter().next().expect("an item") {
        Item::Fn(f) => f.doc,
        Item::Struct(s) => s.doc,
        Item::Variant(v) => v.doc,
        Item::Import(_) => None,
    }
}

#[test]
fn attaches_to_every_kind_of_declaration() {
    assert_eq!(doc_of("# Adds.\nfn add() {}").as_deref(), Some("Adds."));
    assert_eq!(
        doc_of("# A point.\nstruct P { x: i64 }").as_deref(),
        Some("A point.")
    );
    assert_eq!(
        doc_of("# A shape.\nvariant S = A | B").as_deref(),
        Some("A shape.")
    );
    // Undocumented stays undocumented.
    assert_eq!(doc_of("fn f() {}"), None);
}

/// Lines join with newlines and nothing else, so a doc block keeps its
/// paragraphs — a bare `#` is a blank line, not a dropped one.
#[test]
fn joins_lines_and_keeps_blank_ones() {
    assert_eq!(
        doc_of("# One.\n#\n# Two.\nfn f() {}").as_deref(),
        Some("One.\n\nTwo.")
    );
    // One optional space of separation is removed; further indentation is the
    // author's and survives, which is what a code block in a doc needs.
    assert_eq!(
        doc_of("# Text.\n#     indented\nfn f() {}").as_deref(),
        Some("Text.\n    indented")
    );
    // `#text` without the space means the same as `# text`.
    assert_eq!(doc_of("#Tight.\nfn f() {}").as_deref(), Some("Tight."));
}

/// The placement rule, which is the whole reason these are tokens rather than
/// trivia: a `#` line is documentation *only* in front of a declaration.
#[test]
fn is_refused_anywhere_but_in_front_of_a_declaration() {
    for src in [
        "fn f() {\n    # not here\n    g();\n}",
        "fn f(\n    # nor here\n    a: i64,\n) {}",
        "struct P {\n    # nor here\n    x: i64,\n}",
        "fn f() -> i64 {\n    1\n}\n# trailing, documenting nothing\n",
    ] {
        let message = err(src);
        assert!(
            message.contains('#') || message.to_lowercase().contains("doc"),
            "expected a complaint about the `#` line, got {message:?} for:\n{src}"
        );
    }
}

/// `//` comments are untouched — including the `////////` section banners this
/// change was careful not to break.
#[test]
fn leaves_slash_comments_alone() {
    let src = "fn f() -> i64 {\n    ////////////\n    // part 1 //\n    ////////////\n    1\n}";
    assert_eq!(parse(src).items.len(), 1);
    assert_eq!(doc_of(src), None);
}

/// `#` also leads a set/dict literal and the `#[allow]` marker; neither is a
/// doc comment.
#[test]
fn does_not_swallow_set_literals_or_lint_markers() {
    let src = "fn f() -> #{i64} { #{1, 2} }";
    assert_eq!(parse(src).items.len(), 1);
    let allow = "import { len, print } from builtins;\n\
                 fn f(s: str) !prints { print(s[3..s.len()]); #[allow]\n}";
    assert_eq!(parse(allow).items.len(), 2);
}

/// What the index — and so the docs site — now sees for a type.
#[test]
fn reaches_the_source_index() {
    aipl::install_parser_hooks();
    let src = "# A point in the plane.\nstruct P { x: i64 }\n";
    let idx = aipl::index::FileIndex::parse("p.aipl", src).expect("indexes");
    let sym = idx.define("P").expect("P");
    assert_eq!(sym.doc.as_deref(), Some("A point in the plane."));
}
