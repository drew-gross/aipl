//! The AIPL auto-formatter: canonical source layout (gofmt-style — the
//! formatter, not the author, decides line breaks) driven by a width limit.
//!
//! Works from the *token stream* ([`aipl_parser::lex_tokens_and_comments`]),
//! not the AST: the parser desugars as it builds (template literals, `set
//! n++`, destructuring, operator-value arguments), so the AST can't be printed
//! back faithfully — while tokens carry every literal, comment, and paren
//! verbatim by span. A small recursive-descent walk over the tokens mirrors
//! the grammar just enough to build a [`doc`] layout tree; [`doc::print`]
//! picks line breaks.
//!
//! Style (see the repo discussion): 4-space indent; width-limited groups that
//! either fit on one line or block-indent one element per line with a
//! trailing comma; imports hoisted to the top (builtins first, then paths
//! sorted; names within a list sorted, operators first); exactly one blank
//! line between top-level items; call-site keyword arguments spelled tight
//! (`f(1, k=1)`) but declaration defaults spaced (`k: i64 = 1`). String,
//! char, number, and template literals — and everything inside a template's
//! interpolations — are emitted verbatim from the source.
//!
//! Trailing `--- section ---` blocks (test expectations) are split off before
//! formatting and re-attached byte-for-byte: their bodies are assertions
//! where even trailing whitespace can be significant.

mod doc;

use std::collections::VecDeque;

use aipl_parser::{lex_tokens_and_comments, FmtTokenKind, TokenKind};
use aipl_syntax::{Error, Span};
use doc::{concat, group, indent, text, Doc};

/// Options for [`format_source`]. More knobs may grow here; construct with
/// `FmtOptions::default()` and override fields.
#[derive(Debug, Clone)]
pub struct FmtOptions {
    /// Maximum line width the layout aims for (long verbatim atoms — string
    /// literals, templates — may still exceed it).
    pub max_width: usize,
}

impl Default for FmtOptions {
    fn default() -> Self {
        FmtOptions { max_width: 100 }
    }
}

/// Format AIPL source to the canonical style. The input's trailing
/// `--- section ---` blocks (if any) are preserved byte-for-byte; trailing
/// whitespace in the source portion is removed (the language rejects it, so
/// fixing it can't change an accepted program's meaning).
///
/// Requires the parser hooks (`aipl::install_parser_hooks`) — lexing a
/// `"""` raw string runs the dogfooded de-denter.
pub fn format_source(src: &str, opts: &FmtOptions) -> Result<String, Error> {
    // Split off the trailing test sections: they are re-attached verbatim
    // from the *original* text (an expected-output body may contain
    // whitespace the cleanup below must not touch).
    let prefix_len = aipl_parser::strip_test_sections(src).len();
    let sections = &src[prefix_len..];

    // Remove trailing whitespace per line before lexing, so every span the
    // walker copies verbatim refers to the cleaned text.
    let cleaned = clean_trailing_whitespace(&src[..prefix_len]);

    // Validate with the real parser first: its errors are the good ones, and
    // anything it accepts the walker below must handle.
    aipl_parser::parse(&cleaned)?;

    let (toks, comments) = lex_tokens_and_comments(&cleaned)?;
    let mut w = Walker {
        src: &cleaned,
        toks,
        pos: 0,
        comments: comments.into(),
        last_end: 0,
        opts,
    };
    let d = w.program()?;
    if let Some(c) = w.comments.front() {
        return Err(Error::at(
            "formatter: comment was not carried into the output (formatter bug)",
            c.clone(),
        ));
    }

    let mut out = doc::print(&d, opts.max_width);
    while out.ends_with('\n') {
        out.pop();
    }
    if !out.is_empty() {
        out.push('\n');
    }

    // Safety net: the output must contain exactly the input's tokens and
    // comments (imports may be reordered, so compare as multisets). Any
    // mismatch is a formatter bug — refuse to emit rather than corrupt code.
    verify_same_tokens(&cleaned, &out)?;

    if !sections.is_empty() {
        if out.is_empty() {
            out.push('\n');
        }
        out.push_str(sections);
    }
    Ok(out)
}

/// Strip trailing spaces/tabs from every line (line endings preserved).
fn clean_trailing_whitespace(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut line_start = true;
    let mut pending_ws = String::new();
    for c in src.chars() {
        match c {
            ' ' | '\t' => pending_ws.push(c),
            '\n' => {
                pending_ws.clear();
                out.push('\n');
                line_start = true;
            }
            _ => {
                if !line_start || !pending_ws.is_empty() {
                    out.push_str(&pending_ws);
                }
                pending_ws.clear();
                out.push(c);
                line_start = false;
            }
        }
    }
    out
}

/// Lex both texts and compare token/comment content as multisets (imports may
/// legitimately reorder). Errors with a formatter-bug message on mismatch.
fn verify_same_tokens(input: &str, output: &str) -> Result<(), Error> {
    let (in_toks, in_comments) = lex_tokens_and_comments(input)?;
    let (out_toks, out_comments) = lex_tokens_and_comments(output)
        .map_err(|e| Error::msg(format!("formatter produced unlexable output: {e}")))?;
    // Trailing commas are normalized by design (dropped when a list renders
    // flat, added when it breaks), so commas don't participate in the check.
    let texts = |src: &str, toks: &[(FmtTokenKind, Span)]| -> Vec<String> {
        let mut v: Vec<String> = toks
            .iter()
            .filter(|(_, sp)| &src[(*sp).clone()] != ",")
            .map(|(k, sp)| format!("{k:?} {}", canonical_raw(&src[sp.clone()])))
            .collect();
        v.sort();
        v
    };
    let ctexts = |src: &str, cs: &[Span]| -> Vec<String> {
        let mut v: Vec<String> = cs.iter().map(|sp| src[sp.clone()].to_string()).collect();
        v.sort();
        v
    };
    let before = texts(input, &in_toks);
    let after = texts(output, &out_toks);
    if before != after {
        // Name a few tokens from the symmetric difference, to make the bug
        // report (and debugging) concrete.
        let missing: Vec<&String> = before.iter().filter(|t| !after.contains(t)).collect();
        let added: Vec<&String> = after.iter().filter(|t| !before.contains(t)).collect();
        return Err(Error::msg(format!(
            "formatter would not preserve the token stream (formatter bug); no output written. \
             lost: {:?}; gained: {:?}",
            &missing[..missing.len().min(5)],
            &added[..added.len().min(5)],
        )));
    }
    if ctexts(input, &in_comments) != ctexts(output, &out_comments) {
        return Err(Error::msg(
            "formatter would not preserve comments (formatter bug); no output written",
        ));
    }
    Ok(())
}

/// Canonicalize a token's raw text for the preservation check: a multi-line
/// raw-string / template token whose last line is a bare closing delimiter has
/// that line's leading whitespace collapsed, so the formatter's re-indentation
/// of the closing `"""` / ``` ``` ``` (its only edit to a raw block) reads as
/// no change. Every other token — and every content line — is left as-is, so a
/// real corruption still trips the check.
fn canonical_raw(text: &str) -> String {
    if let Some(nl) = text.rfind('\n') {
        let (head, last) = text.split_at(nl + 1);
        let trimmed = last.trim_start();
        if trimmed == "\"\"\"" || trimmed == "```" {
            return format!("{head}{trimmed}");
        }
    }
    text.to_string()
}

/// Binary operator spellings as they appear in token text.
fn is_binop(t: &str) -> bool {
    matches!(
        t,
        "+" | "-" | "*" | "/" | "%" | "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||"
    )
}

struct Walker<'s> {
    src: &'s str,
    toks: Vec<(FmtTokenKind, Span)>,
    pos: usize,
    comments: VecDeque<Span>,
    /// End offset of the last consumed token or comment — for blank-line and
    /// same-line (trailing comment) decisions.
    last_end: usize,
    opts: &'s FmtOptions,
}

impl<'s> Walker<'s> {
    // ---------- token primitives ----------

    fn peek(&self) -> Option<&(FmtTokenKind, Span)> {
        self.toks.get(self.pos)
    }

    fn peek_text(&self) -> &'s str {
        self.toks
            .get(self.pos)
            .map(|(_, sp)| &self.src[sp.clone()])
            .unwrap_or("")
    }

    fn peek_kind(&self) -> Option<FmtTokenKind> {
        self.toks.get(self.pos).map(|(k, _)| *k)
    }

    fn peek_text_n(&self, n: usize) -> &'s str {
        self.toks
            .get(self.pos + n)
            .map(|(_, sp)| &self.src[sp.clone()])
            .unwrap_or("")
    }

    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    /// Consume the next token, returning its text. The caller has already
    /// dispatched on it; comments before it must have been drained (else the
    /// final not-carried check fails).
    fn bump(&mut self) -> &'s str {
        let (_, sp) = &self.toks[self.pos];
        self.pos += 1;
        self.last_end = sp.end;
        &self.src[sp.clone()]
    }

    fn expect(&mut self, want: &str) -> Result<&'s str, Error> {
        if self.peek_text() == want {
            Ok(self.bump())
        } else {
            Err(self.err_here(&format!(
                "formatter: expected {want:?}, found {:?}",
                self.peek_text()
            )))
        }
    }

    fn err_here(&self, msg: &str) -> Error {
        match self.peek() {
            Some((_, sp)) => Error::at(msg.to_string(), sp.clone()),
            None => Error::msg(format!("{msg} (at end of input)")),
        }
    }

    /// Offset where the next syntactic entity begins: the next comment or
    /// token, whichever comes first.
    fn next_start(&self) -> usize {
        let tok = self
            .peek()
            .map(|(_, sp)| sp.start)
            .unwrap_or(self.src.len());
        match self.comments.front() {
            Some(c) if c.start < tok => c.start,
            _ => tok,
        }
    }

    fn gap_has_blank(&self, from: usize, to: usize) -> bool {
        self.src
            .get(from..to)
            .is_some_and(|s| s.matches('\n').count() >= 2)
    }

    fn same_line(&self, from: usize, to: usize) -> bool {
        self.src.get(from..to).is_some_and(|s| !s.contains('\n'))
    }

    /// Drain comments that appear before the next token, rendering each on
    /// its own line (a mid-list line comment thereby forces the enclosing
    /// group to break — a comment can't be flattened onto one line). A blank
    /// line *after* a comment is preserved: it marks the comment as
    /// standalone rather than attached to what follows.
    fn lead(&mut self) -> Doc {
        let limit = self.peek().map(|(_, sp)| sp.start).unwrap_or(usize::MAX);
        let mut docs = Vec::new();
        while let Some(c) = self.comments.front() {
            if c.start >= limit {
                break;
            }
            let c = self.comments.pop_front().unwrap();
            self.last_end = c.end;
            let next = self.next_start().max(c.end);
            docs.push(text(&self.src[c.clone()]));
            docs.push(if self.gap_has_blank(c.end, next) {
                Doc::BlankLine
            } else {
                Doc::HardLine
            });
        }
        concat(docs)
    }

    /// A trailing comment sitting on the same line as whatever just ended, if
    /// any — rendered as ` // ...` glued to the current line, plus a
    /// break-parent so no group flattens anything after it onto the comment's
    /// line. The comment must also come *before* the next unconsumed token:
    /// a comment at the end of a source line whose earlier tokens are still
    /// being consumed (e.g. after the `}` closing an inline block) belongs to
    /// a later flush point, not this one.
    fn trailing_comment(&mut self) -> Doc {
        let next_tok = self.peek().map(|(_, sp)| sp.start).unwrap_or(usize::MAX);
        if let Some(c) = self.comments.front() {
            if c.start < next_tok && self.same_line(self.last_end, c.start) {
                let c = self.comments.pop_front().unwrap();
                self.last_end = c.end;
                return concat(vec![
                    text(" "),
                    text(&self.src[c.clone()]),
                    Doc::BreakParent,
                ]);
            }
        }
        concat(vec![])
    }

    // ---------- program / items ----------

    fn program(&mut self) -> Result<Doc, Error> {
        // File-header comments (before the first item) stay at the very top,
        // even though imports below may be reordered.
        let mut header = Vec::new();
        let first_tok_start = self.peek().map(|(_, sp)| sp.start).unwrap_or(usize::MAX);
        loop {
            let Some(c) = self.comments.front() else {
                break;
            };
            if c.start >= first_tok_start {
                break;
            }
            let c = self.comments.pop_front().unwrap();
            let after_blank = self.gap_has_blank(c.end, self.next_start().max(c.end));
            self.last_end = c.end;
            header.push(text(&self.src[c.clone()]));
            header.push(if after_blank {
                Doc::BlankLine
            } else {
                Doc::HardLine
            });
        }

        // Parse items; imports are collected separately so they can be
        // hoisted and sorted (builtins first, then by path).
        let mut imports: Vec<(ImportKey, Doc)> = Vec::new();
        let mut items: Vec<Doc> = Vec::new();
        while !self.at_end() {
            let lead = self.lead();
            if self.at_end() {
                // Comments after the last item.
                items.push(lead);
                break;
            }
            if self.peek_text() == "import" {
                let (key, d) = self.import_item()?;
                imports.push((key, concat(vec![lead, d, self.trailing_comment()])));
            } else {
                let d = self.item()?;
                items.push(concat(vec![lead, d, self.trailing_comment()]));
            }
        }
        imports.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = Vec::new();
        out.extend(header);
        for (i, (_, d)) in imports.iter().enumerate() {
            if i > 0 {
                out.push(Doc::HardLine);
            }
            out.push(d.clone());
        }
        for (i, d) in items.iter().enumerate() {
            if i > 0 || !imports.is_empty() {
                out.push(Doc::BlankLine);
            }
            out.push(d.clone());
        }
        Ok(concat(out))
    }

    fn item(&mut self) -> Result<Doc, Error> {
        match self.peek_text() {
            "pub" | "fn" => self.fn_item(),
            "struct" => self.struct_item(),
            "variant" => self.variant_item(),
            other => Err(self.err_here(&format!(
                "formatter: expected an item (fn/struct/variant/import), found {other:?}"
            ))),
        }
    }

    // ---------- imports ----------

    fn import_item(&mut self) -> Result<(ImportKey, Doc), Error> {
        self.expect("import")?;
        self.expect("{")?;
        let mut names: Vec<(u8, String, String)> = Vec::new();
        while self.peek_text() != "}" {
            let first = self.bump().to_string();
            let rendered;
            let local;
            if self.peek_text() == "as" {
                self.bump();
                let alias = self.bump().to_string();
                rendered = format!("{first} as {alias}");
                local = alias;
            } else {
                rendered = first.clone();
                local = first;
            }
            // Operators sort ahead of plain names, each bucket alphabetical
            // by the locally-bound name.
            let bucket = u8::from(!aipl_syntax::is_operator_name(&local));
            names.push((bucket, local, rendered));
            if self.peek_text() == "," {
                self.bump();
            }
        }
        self.expect("}")?;
        self.expect("from")?;
        let source = self.bump().to_string();
        self.expect(";")?;

        names.sort();
        let key = if source == "builtins" {
            ImportKey::Builtins
        } else {
            ImportKey::Path(source.clone())
        };
        let list = self.comma_list_docs(
            names.into_iter().map(|(_, _, r)| text(r)).collect(),
            ListStyle::SpacedBraces,
        );
        let d = concat(vec![
            text("import "),
            list,
            text(" from "),
            text(source),
            text(";"),
        ]);
        Ok((key, d))
    }

    // ---------- fn / struct / variant ----------

    fn fn_item(&mut self) -> Result<Doc, Error> {
        let mut sig = Vec::new();
        if self.peek_text() == "pub" {
            self.bump();
            sig.push(text("pub "));
        }
        self.expect("fn")?;
        sig.push(text("fn "));
        sig.push(text(self.bump())); // name

        if self.peek_text() == "<" {
            self.bump();
            let mut tps = Vec::new();
            while self.peek_text() != ">" {
                let name = self.bump().to_string();
                self.expect(":")?;
                let bound = self.bump();
                tps.push(text(format!("{name}: {bound}")));
                if self.peek_text() == "," {
                    self.bump();
                }
            }
            self.expect(">")?;
            sig.push(self.comma_list_docs(tps, ListStyle::Angles));
        }

        self.expect("(")?;
        let params = self.params_until(")")?;
        self.expect(")")?;
        sig.push(self.comma_list_docs(params, ListStyle::Parens));

        while self.peek_text() == "!" {
            self.bump();
            let eff = self.bump();
            sig.push(text(format!(" !{eff}")));
        }
        if self.peek_text() == "->" {
            self.bump();
            sig.push(text(" -> "));
            sig.push(self.ty()?);
        }

        sig.push(text(" "));
        let body = self.block()?;
        sig.push(body);

        // `.test({ .. })` / `.doc("..")` attributes, glued to the closing
        // brace like the existing corpus writes them.
        while self.peek_text() == "." {
            self.bump();
            let attr = self.bump().to_string();
            self.expect("(")?;
            let arg = if self.peek_text() == "{" {
                self.block()?
            } else {
                // The doc string, verbatim — a `"""` raw block aligns its
                // closing delimiter under the opening; a plain `"..."` is text.
                let t = self.bump();
                if t.starts_with("\"\"\"") {
                    doc::raw_block(t)
                } else {
                    text(t)
                }
            };
            self.expect(")")?;
            sig.push(text(format!(".{attr}(")));
            sig.push(arg);
            sig.push(text(")"));
        }
        Ok(concat(sig))
    }

    /// Parameters of a fn declaration, up to (not consuming) `end`.
    fn params_until(&mut self, end: &str) -> Result<Vec<Doc>, Error> {
        let mut params = Vec::new();
        while self.peek_text() != end {
            let lead = self.lead();
            let mut p = vec![lead];
            if self.peek_text() == "mut" {
                self.bump();
                p.push(text("mut "));
            }
            p.push(text(self.bump())); // name
            self.expect(":")?;
            p.push(text(": "));
            p.push(self.ty()?);
            if self.peek_text() == "*" {
                self.bump();
                p.push(text("*"));
            }
            if self.peek_text() == "=" {
                // A keyword parameter's declaration default keeps spaces
                // around `=` (unlike a call-site keyword argument).
                self.bump();
                p.push(text(" = "));
                p.push(self.expr()?);
            }
            params.push(concat(p));
            if self.peek_text() == "," {
                self.bump();
            }
        }
        Ok(params)
    }

    fn struct_item(&mut self) -> Result<Doc, Error> {
        self.expect("struct")?;
        let name = self.bump().to_string();
        self.expect("{")?;
        let mut fields = Vec::new();
        while self.peek_text() != "}" {
            let lead = self.lead();
            let fname = self.bump().to_string();
            self.expect(":")?;
            let fty = self.ty()?;
            let mut f = vec![lead, text(fname), text(": "), fty];
            if self.peek_text() == "=" {
                self.bump();
                f.push(text(" = "));
                f.push(self.expr()?);
            }
            fields.push(concat(f));
            if self.peek_text() == "," {
                self.bump();
            }
        }
        self.expect("}")?;
        Ok(concat(vec![
            text(format!("struct {name} ")),
            self.comma_list_docs(fields, ListStyle::SpacedBraces),
        ]))
    }

    fn variant_item(&mut self) -> Result<Doc, Error> {
        self.expect("variant")?;
        let name = self.bump().to_string();
        self.expect("=")?;
        let mut cases = Vec::new();
        loop {
            let cname = self.bump().to_string();
            let mut c = vec![text(cname)];
            if self.peek_text() == "(" {
                self.bump();
                let mut tys = Vec::new();
                while self.peek_text() != ")" {
                    tys.push(self.ty()?);
                    if self.peek_text() == "," {
                        self.bump();
                    }
                }
                self.expect(")")?;
                c.push(self.comma_list_docs(tys, ListStyle::TyParens));
            }
            cases.push(concat(c));
            if self.peek_text() == "|" {
                self.bump();
            } else {
                break;
            }
        }
        // Flat: `variant V = A | B(i64)`. Broken: one case per line, the
        // separator `|` leading each continuation.
        let mut body = vec![cases[0].clone()];
        for c in &cases[1..] {
            body.push(Doc::Line);
            body.push(text("| "));
            body.push(c.clone());
        }
        Ok(group(concat(vec![
            text(format!("variant {name} =")),
            indent(concat(vec![Doc::Line, concat(body)])),
        ])))
    }

    // ---------- types ----------

    fn ty(&mut self) -> Result<Doc, Error> {
        // `!E` — void-ok result.
        if self.peek_text() == "!" {
            self.bump();
            let inner = self.base_ty()?;
            return Ok(concat(vec![text("!"), inner]));
        }
        if self.peek_text() == "(" {
            // Function type `(A, B) -> R`, tuple `(A, B)`, or tuple array
            // `(A, B)[]`.
            self.bump();
            let mut args = Vec::new();
            while self.peek_text() != ")" {
                args.push(self.ty()?);
                if self.peek_text() == "," {
                    self.bump();
                }
            }
            self.expect(")")?;
            let mut d = vec![self.comma_list_docs(args, ListStyle::TyParens)];
            if self.peek_text() == "->" {
                self.bump();
                d.push(text(" -> "));
                d.push(self.ty()?);
            } else {
                while self.peek_text() == "[" && self.peek_text_n(1) == "]" {
                    self.bump();
                    self.bump();
                    d.push(text("[]"));
                }
            }
            return Ok(concat(d));
        }
        let base = self.base_ty()?;
        if self.peek_text() == "!" {
            self.bump();
            let err = self.base_ty()?;
            return Ok(concat(vec![base, text("!"), err]));
        }
        Ok(base)
    }

    fn base_ty(&mut self) -> Result<Doc, Error> {
        let mut d = if self.peek_text() == "#" {
            self.bump();
            self.expect("{")?;
            let k = self.ty()?;
            let mut parts = vec![text("#{"), k];
            if self.peek_text() == ":" {
                self.bump();
                parts.push(text(": "));
                parts.push(self.ty()?);
            }
            self.expect("}")?;
            parts.push(text("}"));
            vec![concat(parts)]
        } else {
            vec![text(self.bump())]
        };
        loop {
            match self.peek_text() {
                "?" => {
                    self.bump();
                    d.push(text("?"));
                }
                "[" if self.peek_text_n(1) == "]" => {
                    self.bump();
                    self.bump();
                    d.push(text("[]"));
                }
                _ => break,
            }
        }
        Ok(concat(d))
    }

    // ---------- blocks & statements ----------

    /// A `{ .. }` block as its own independently-breaking group. See
    /// [`block_layout`] for the shared-group form an `if`/`else` uses.
    ///
    /// [`block_layout`]: Walker::block_layout
    fn block(&mut self) -> Result<Doc, Error> {
        Ok(group(self.block_layout()?))
    }

    /// A `{ .. }` block: statements (`;`-terminated) plus an optional trailing
    /// expression. Renders `{}` when empty, `{ expr }` when a lone trailing
    /// expression fits, and one statement per line otherwise (any `;` forces
    /// the break via the hard lines between statements). The result is *not*
    /// wrapped in a group, so a caller can place several blocks under one
    /// shared group (an `if`/`else` breaks both branches together) — [`block`]
    /// is the grouped form for a standalone block.
    ///
    /// [`block`]: Walker::block
    fn block_layout(&mut self) -> Result<Doc, Error> {
        self.expect("{")?;
        let body_start = self.pos;
        let mut entries: Vec<Doc> = Vec::new();
        let mut prev_end = self.last_end;
        loop {
            // Comments before the next statement (or the closing brace).
            let lead = self.lead();
            let had_lead = !matches!(&lead, Doc::Concat(v) if v.is_empty());
            if had_lead {
                let blank = self.gap_has_blank(prev_end, prev_end.max(self.last_end));
                entries.push(sep_entry(blank, !entries.is_empty()));
                entries.push(lead);
                // `lead` already ends with a line break per comment; the next
                // entry continues under it.
                prev_end = self.last_end;
            }
            if self.peek_text() == "}" {
                if had_lead {
                    // Drop the line break `lead` left dangling before `}` —
                    // the closing edge supplies it.
                    if let Some(Doc::Concat(v)) = entries.last_mut() {
                        if matches!(v.last(), Some(Doc::HardLine | Doc::BlankLine)) {
                            v.pop();
                        }
                    }
                }
                break;
            }
            let start = self.next_start();
            let stmt = self.statement()?;
            if !had_lead {
                let blank = self.gap_has_blank(prev_end, start);
                entries.push(sep_entry(blank, !entries.is_empty()));
            }
            entries.push(stmt);
            entries.push(self.trailing_comment());
            prev_end = self.last_end;
        }
        // A block that contains any statement (a `;` anywhere inside it —
        // nested ones force their own block first, so the distinction doesn't
        // matter) always lays out one line per statement. Only a pure
        // single-expression body (`fn f() -> i64 { x + y }`) may stay flat.
        let has_stmt = self.toks[body_start..self.pos]
            .iter()
            .any(|(_, sp)| &self.src[sp.clone()] == ";");
        self.expect("}")?;
        if entries.is_empty() {
            return Ok(text("{}"));
        }
        let edge = if has_stmt { Doc::HardLine } else { Doc::Line };
        Ok(concat(vec![
            text("{"),
            indent(concat(
                std::iter::once(edge.clone()).chain(entries).collect(),
            )),
            edge,
            text("}"),
        ]))
    }

    /// One statement (consuming its `;`) or the block's trailing expression.
    fn statement(&mut self) -> Result<Doc, Error> {
        match self.peek_text() {
            "let" => {
                self.bump();
                if self.peek_text() == "(" {
                    // `let (a, b) = expr;`
                    self.bump();
                    let mut ids = Vec::new();
                    while self.peek_text() != ")" {
                        ids.push(self.bump().to_string());
                        if self.peek_text() == "," {
                            self.bump();
                        }
                    }
                    self.expect(")")?;
                    self.expect("=")?;
                    let value = self.expr()?;
                    self.expect(";")?;
                    return Ok(concat(vec![
                        text(format!("let ({}) = ", ids.join(", "))),
                        value,
                        text(";"),
                    ]));
                }
                let name = self.bump().to_string();
                if self.peek_text() == "{" {
                    // `let Name { a, b } = expr;`
                    self.bump();
                    let mut ids = Vec::new();
                    while self.peek_text() != "}" {
                        ids.push(self.bump().to_string());
                        if self.peek_text() == "," {
                            self.bump();
                        }
                    }
                    self.expect("}")?;
                    self.expect("=")?;
                    let value = self.expr()?;
                    self.expect(";")?;
                    return Ok(concat(vec![
                        text(format!("let {name} {{ {} }} = ", ids.join(", "))),
                        value,
                        text(";"),
                    ]));
                }
                self.expect("=")?;
                let value = self.expr()?;
                self.expect(";")?;
                Ok(concat(vec![
                    text(format!("let {name} = ")),
                    value,
                    text(";"),
                ]))
            }
            "mut" => {
                self.bump();
                let name = self.bump().to_string();
                self.expect("=")?;
                let value = self.expr()?;
                self.expect(";")?;
                Ok(concat(vec![
                    text(format!("mut {name} = ")),
                    value,
                    text(";"),
                ]))
            }
            "set" => {
                self.bump();
                let name = self.bump().to_string();
                if self.peek_text() == "++" {
                    self.bump();
                    self.expect(";")?;
                    return Ok(text(format!("set {name}++;")));
                }
                self.expect("=")?;
                let value = self.expr()?;
                self.expect(";")?;
                Ok(concat(vec![
                    text(format!("set {name} = ")),
                    value,
                    text(";"),
                ]))
            }
            "return" => {
                self.bump();
                let value = self.expr()?;
                self.expect(";")?;
                Ok(concat(vec![text("return "), value, text(";")]))
            }
            "for" => {
                self.bump();
                self.expect("(")?;
                self.expect("let")?;
                let binder = if self.peek_text() == "(" {
                    self.bump();
                    let mut ids = Vec::new();
                    while self.peek_text() != ")" {
                        ids.push(self.bump().to_string());
                        if self.peek_text() == "," {
                            self.bump();
                        }
                    }
                    self.expect(")")?;
                    format!("({})", ids.join(", "))
                } else {
                    self.bump().to_string()
                };
                self.expect(":")?;
                let iterable = self.expr()?;
                self.expect(")")?;
                let body = self.block()?;
                Ok(concat(vec![
                    text(format!("for (let {binder} : ")),
                    iterable,
                    text(") "),
                    body,
                ]))
            }
            "while" => {
                self.bump();
                self.expect("(")?;
                let cond = self.expr()?;
                self.expect(")")?;
                let body = self.block()?;
                Ok(concat(vec![text("while ("), cond, text(") "), body]))
            }
            _ => {
                let e = self.expr()?;
                if self.peek_text() == ";" {
                    self.bump();
                    Ok(concat(vec![e, text(";")]))
                } else {
                    // The block's trailing (value) expression.
                    Ok(e)
                }
            }
        }
    }

    // ---------- expressions ----------

    fn expr(&mut self) -> Result<Doc, Error> {
        let lead = self.lead();
        let first = self.unary()?;
        let mut tail = Vec::new();
        while is_binop(self.peek_text()) {
            let op = self.bump().to_string();
            let lead_rhs = self.lead();
            let rhs = self.unary()?;
            tail.push(concat(vec![Doc::Line, text(op), text(" "), lead_rhs, rhs]));
        }
        let d = if tail.is_empty() {
            first
        } else {
            // Flat: `a + b + c`; broken: continuation lines led by the
            // operator, one indent in.
            group(concat(vec![first, indent(concat(tail))]))
        };
        Ok(concat(vec![lead, d]))
    }

    fn unary(&mut self) -> Result<Doc, Error> {
        match self.peek_text() {
            "-" | "!" => {
                let op = self.bump().to_string();
                let rest = self.unary()?;
                Ok(concat(vec![text(op), rest]))
            }
            _ => self.postfix(),
        }
    }

    fn postfix(&mut self) -> Result<Doc, Error> {
        let head = self.atom()?;
        // Collect postfix segments, tagging `.method(...)` calls so a long
        // chain can break *before each call* rather than exploding an inner
        // argument list. Field access, indexing, and `?` glue to their left.
        let mut segs: Vec<(bool, Doc)> = Vec::new();
        loop {
            match self.peek_text() {
                "." => {
                    self.bump();
                    let member = self.bump().to_string();
                    if self.peek_text() == "(" {
                        self.bump();
                        let args = self.call_args_until(")")?;
                        self.expect(")")?;
                        let d = concat(vec![
                            text(format!(".{member}")),
                            self.comma_list_docs(args, ListStyle::Parens),
                        ]);
                        segs.push((true, d));
                    } else {
                        segs.push((false, text(format!(".{member}"))));
                    }
                }
                "[" => {
                    self.bump();
                    let mut idx = vec![text("[")];
                    if self.peek_text() == ".." {
                        self.bump();
                        idx.push(text(".."));
                        idx.push(self.expr()?);
                    } else {
                        idx.push(self.expr()?);
                        if self.peek_text() == ".." {
                            self.bump();
                            idx.push(text(".."));
                            if self.peek_text() != "]" {
                                idx.push(self.expr()?);
                            }
                        }
                    }
                    self.expect("]")?;
                    idx.push(text("]"));
                    segs.push((false, concat(idx)));
                }
                "?" => {
                    self.bump();
                    segs.push((false, text("?")));
                }
                _ => break,
            }
        }
        if segs.is_empty() {
            return Ok(head);
        }
        // A "member chain" (2+ method calls) becomes one group that, when it
        // can't fit on a line, breaks before every call segment — the
        // receiver ends up alone on the first line and each `.method(...)`
        // starts its own indented line. Fewer than two calls: no chain break
        // (a lone call's own argument list may still break).
        let call_count = segs.iter().filter(|(is_call, _)| *is_call).count();
        if call_count < 2 {
            let mut parts = vec![head];
            parts.extend(segs.into_iter().map(|(_, d)| d));
            return Ok(concat(parts));
        }
        let mut chain = Vec::new();
        for (is_call, d) in segs {
            if is_call {
                chain.push(Doc::SoftLine);
            }
            chain.push(d);
        }
        Ok(group(concat(vec![head, indent(concat(chain))])))
    }

    fn atom(&mut self) -> Result<Doc, Error> {
        match self.peek_kind() {
            Some(FmtTokenKind::TemplateHead) => return self.template_verbatim(),
            Some(FmtTokenKind::Plain(TokenKind::Str)) => {
                // A `"""..."""` raw string is a [`RawBlock`] so its closing
                // delimiter aligns under the opening one; a plain `"..."`
                // string is ordinary verbatim text.
                let t = self.bump();
                return Ok(if t.starts_with("\"\"\"") {
                    doc::raw_block(t)
                } else {
                    text(t)
                });
            }
            Some(FmtTokenKind::Plain(
                TokenKind::Number | TokenKind::Char | TokenKind::Constant,
            )) => return Ok(text(self.bump())),
            _ => {}
        }
        match self.peek_text() {
            "if" => self.if_expr(),
            "match" => self.match_expr(),
            "(" => {
                self.bump();
                let first = self.expr()?;
                if self.peek_text() == "," {
                    // Tuple literal.
                    let mut elems = vec![first];
                    while self.peek_text() == "," {
                        self.bump();
                        if self.peek_text() == ")" {
                            break;
                        }
                        elems.push(self.expr()?);
                    }
                    self.expect(")")?;
                    Ok(self.comma_list_docs(elems, ListStyle::Parens))
                } else {
                    self.expect(")")?;
                    Ok(concat(vec![text("("), first, text(")")]))
                }
            }
            "[" => {
                self.bump();
                let elems = self.call_args_until("]")?;
                self.expect("]")?;
                Ok(self.comma_list_docs(elems, ListStyle::Brackets))
            }
            "#" => {
                self.bump();
                self.expect("{")?;
                if self.peek_text() == ":" {
                    self.bump();
                    self.expect("}")?;
                    return Ok(text("#{:}"));
                }
                let mut entries = Vec::new();
                while self.peek_text() != "}" {
                    let lead = self.lead();
                    let k = self.expr()?;
                    let mut e = vec![lead, k];
                    if self.peek_text() == ":" {
                        self.bump();
                        e.push(text(": "));
                        e.push(self.expr()?);
                    }
                    entries.push(concat(e));
                    if self.peek_text() == "," {
                        self.bump();
                    }
                }
                self.expect("}")?;
                Ok(self.comma_list_docs(entries, ListStyle::HashBraces))
            }
            _ => {
                if self.peek_kind() != Some(FmtTokenKind::Plain(TokenKind::Identifier))
                    && self.peek_kind() != Some(FmtTokenKind::Plain(TokenKind::BuiltinType))
                    && self.peek_kind() != Some(FmtTokenKind::Plain(TokenKind::Keyword))
                {
                    return Err(self.err_here(&format!(
                        "formatter: expected an expression, found {:?}",
                        self.peek_text()
                    )));
                }
                let name = self.bump().to_string();
                if self.peek_text() == "(" {
                    self.bump();
                    let args = self.call_args_until(")")?;
                    self.expect(")")?;
                    Ok(concat(vec![
                        text(name),
                        self.comma_list_docs(args, ListStyle::Parens),
                    ]))
                } else if self.peek_text() == "{" {
                    // Struct construction `Name { field: value, .. }`.
                    self.bump();
                    let mut inits = Vec::new();
                    while self.peek_text() != "}" {
                        let lead = self.lead();
                        let fname = self.bump().to_string();
                        self.expect(":")?;
                        let value = self.expr()?;
                        inits.push(concat(vec![lead, text(format!("{fname}: ")), value]));
                        if self.peek_text() == "," {
                            self.bump();
                        }
                    }
                    self.expect("}")?;
                    if inits.is_empty() {
                        return Ok(text(format!("{name} {{}}")));
                    }
                    Ok(concat(vec![
                        text(format!("{name} ")),
                        self.comma_list_docs(inits, ListStyle::SpacedBraces),
                    ]))
                } else {
                    Ok(text(name))
                }
            }
        }
    }

    fn if_expr(&mut self) -> Result<Doc, Error> {
        self.expect("if")?;
        self.expect("(")?;
        let cond = self.expr()?;
        self.expect(")")?;
        // The branches share one group (via the ungrouped `block_layout`), so
        // they break *together*: an `if`/`else` is either `if (c) { a } else
        // { b }` on one line or both blocks broken — never one inline and the
        // other split. A trailing `else if` stays its own group so a fitting
        // tail can remain inline.
        let then_b = self.block_layout()?;
        let mut d = vec![text("if ("), cond, text(") "), then_b];
        if self.peek_text() == "else" {
            self.bump();
            d.push(text(" else "));
            if self.peek_text() == "if" {
                d.push(self.if_expr()?);
            } else {
                d.push(self.block_layout()?);
            }
        }
        Ok(group(concat(d)))
    }

    fn match_expr(&mut self) -> Result<Doc, Error> {
        self.expect("match")?;
        self.expect("(")?;
        let scrutinee = self.expr()?;
        self.expect(")")?;
        self.expect("{")?;
        let mut arms = Vec::new();
        while self.peek_text() != "}" {
            let lead = self.lead();
            let pat = self.pattern()?;
            self.expect("=>")?;
            let body = self.expr()?;
            arms.push(concat(vec![lead, pat, text(" => "), body]));
            if self.peek_text() == "," {
                self.bump();
            }
        }
        self.expect("}")?;
        Ok(concat(vec![
            text("match ("),
            scrutinee,
            text(") "),
            self.comma_list_docs(arms, ListStyle::SpacedBraces),
        ]))
    }

    fn pattern(&mut self) -> Result<Doc, Error> {
        if self.peek_kind() == Some(FmtTokenKind::Plain(TokenKind::Str)) {
            return Ok(text(self.bump()));
        }
        if self.peek_text() == "[" {
            self.bump();
            let elems = self.call_args_until("]")?;
            self.expect("]")?;
            return Ok(self.comma_list_docs(elems, ListStyle::Brackets));
        }
        let name = self.bump().to_string();
        if self.peek_text() == "(" {
            self.bump();
            let mut ids = Vec::new();
            while self.peek_text() != ")" {
                ids.push(self.bump().to_string());
                if self.peek_text() == "," {
                    self.bump();
                }
            }
            self.expect(")")?;
            return Ok(text(format!("{name}({})", ids.join(", "))));
        }
        Ok(text(name))
    }

    /// Call-argument (or array-element) list up to `end`: expressions, plus
    /// the arg-only forms — keyword arguments (`k=1`, spelled tight), lambdas,
    /// and bare operator values (`apply(2, 3, +)`).
    fn call_args_until(&mut self, end: &str) -> Result<Vec<Doc>, Error> {
        let mut args = Vec::new();
        while self.peek_text() != end {
            let lead = self.lead();
            let arg = self.call_arg()?;
            args.push(concat(vec![lead, arg]));
            if self.peek_text() == "," {
                self.bump();
            }
        }
        Ok(args)
    }

    fn call_arg(&mut self) -> Result<Doc, Error> {
        // Keyword argument `k = expr` → tight `k=expr` (`==` is one token, so
        // a lone `=` after an identifier is unambiguous).
        if self.peek_kind() == Some(FmtTokenKind::Plain(TokenKind::Identifier))
            && self.peek_text_n(1) == "="
        {
            let name = self.bump().to_string();
            self.bump(); // =
            let value = self.expr()?;
            return Ok(concat(vec![text(format!("{name}=")), value]));
        }
        // Lambda `|x| body` / `|| body`.
        if self.peek_text() == "|" || self.peek_text() == "||" {
            return self.lambda();
        }
        // A bare operator passed as a value: an operator token immediately
        // followed by `,` or a closing bracket.
        if self.peek_kind() == Some(FmtTokenKind::Plain(TokenKind::Operator))
            && matches!(self.peek_text_n(1), "," | ")" | "]")
        {
            return Ok(text(self.bump()));
        }
        self.expr()
    }

    fn lambda(&mut self) -> Result<Doc, Error> {
        let mut params = Vec::new();
        if self.peek_text() == "||" {
            self.bump();
        } else {
            self.expect("|")?;
            while self.peek_text() != "|" {
                let name = self.bump().to_string();
                if self.peek_text() == ":" {
                    self.bump();
                    let ty = self.ty()?;
                    params.push(concat(vec![text(format!("{name}: ")), ty]));
                } else {
                    params.push(text(name));
                }
                if self.peek_text() == "," {
                    self.bump();
                }
            }
            self.expect("|")?;
        }
        let body = if self.peek_text() == "{" {
            self.block()?
        } else {
            self.expr()?
        };
        let mut d = vec![text("|")];
        for (i, p) in params.iter().enumerate() {
            if i > 0 {
                d.push(text(", "));
            }
            d.push(p.clone());
        }
        d.push(text("| "));
        d.push(body);
        Ok(concat(d))
    }

    /// Copy a template literal verbatim, from its head through the matching
    /// tail (interpolation contents included — the formatter does not reach
    /// inside templates).
    fn template_verbatim(&mut self) -> Result<Doc, Error> {
        let start = self.toks[self.pos].1.start;
        let mut depth = 0usize;
        let end;
        loop {
            let Some((kind, sp)) = self.toks.get(self.pos) else {
                return Err(Error::at(
                    "formatter: unterminated template literal in token stream",
                    start..self.src.len(),
                ));
            };
            match kind {
                FmtTokenKind::TemplateHead => depth += 1,
                FmtTokenKind::TemplateTail => {
                    depth -= 1;
                    if depth == 0 {
                        end = sp.end;
                        self.pos += 1;
                        self.last_end = end;
                        break;
                    }
                }
                _ => {}
            }
            self.pos += 1;
            self.last_end = sp.end;
        }
        // Comments inside interpolations came through the comment stream;
        // they're inside the verbatim copy already, so drop them from the
        // queue rather than double-emitting.
        while self.comments.front().is_some_and(|c| c.start < end) {
            self.comments.pop_front();
        }
        // A triple-backtick template is a raw block (its closing ``` aligns
        // under the opening); a single-backtick template has no own-line
        // closing delimiter, so `reindent_closing` leaves it untouched.
        Ok(doc::raw_block(&self.src[start..end]))
    }

    // ---------- shared list machinery ----------

    /// Wrap pre-rendered list items in the agreed one-line-or-block shape.
    fn comma_list_docs(&mut self, items: Vec<Doc>, style: ListStyle) -> Doc {
        let _ = &self.opts; // width decisions happen at print time
        let (open, close, spaced, trailing) = match style {
            ListStyle::Parens => ("(", ")", false, true),
            ListStyle::Brackets => ("[", "]", false, true),
            ListStyle::HashBraces => ("#{", "}", false, true),
            ListStyle::SpacedBraces => ("{", "}", true, true),
            ListStyle::Angles => ("<", ">", false, false),
            ListStyle::TyParens => ("(", ")", false, false),
        };
        if items.is_empty() {
            return text(format!("{open}{close}"));
        }
        let edge = if spaced { Doc::Line } else { Doc::SoftLine };
        let mut inner = vec![edge.clone()];
        for (i, item) in items.into_iter().enumerate() {
            if i > 0 {
                inner.push(text(","));
                inner.push(Doc::Line);
            }
            inner.push(item);
        }
        let mut parts = vec![text(open), indent(concat(inner))];
        if trailing {
            parts.push(Doc::IfBroken(",".into()));
        }
        parts.push(edge);
        parts.push(text(close));
        group(concat(parts))
    }
}

/// Separator between block entries: nothing before the first, otherwise a
/// hard line — doubled when the source had a blank line there.
fn sep_entry(blank: bool, not_first: bool) -> Doc {
    if !not_first {
        concat(vec![])
    } else if blank {
        Doc::BlankLine
    } else {
        Doc::HardLine
    }
}

/// Sort order for hoisted imports: builtins first, then paths alphabetically.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
enum ImportKey {
    Builtins,
    Path(String),
}

/// The bracket pair (and flat-mode edge spacing) of a comma list. Braces that
/// read as words get inner spaces when flat (`{ a, b }`); the rest hug their
/// contents (`(a, b)`, `[a]`, `#{a}`, `<T: any>`). Type-position lists
/// (`TyParens`, `Angles`) take no trailing comma when broken — the grammar
/// doesn't allow one there.
#[derive(Clone, Copy)]
enum ListStyle {
    Parens,
    Brackets,
    HashBraces,
    SpacedBraces,
    Angles,
    TyParens,
}
