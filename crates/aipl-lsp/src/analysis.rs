//! What the server has worked out about files, kept from one request to the
//! next.
//!
//! Answering "where is this defined" takes two compiler passes over the file:
//! the lexer, to find which name the cursor is on, and the parser, to index its
//! declarations and imports. Both are dogfooded AIPL and cost time linear in the
//! file — measured at about a second together for `grammar_aipl.aipl` (5,600
//! lines) — and an editor asks for hover, definition and the outline on every
//! cursor move and keystroke, always of text that has not changed since the
//! last answer. So each is done once per distinct text and remembered.
//!
//! A file is keyed by its path and remembered by the text it was last asked
//! about, not by a version counter: the text is the one fact that decides the
//! answer, and an open buffer, an unsaved edit undone and a file read from disk
//! all reduce to it.
//!
//! # Mid-edit
//!
//! A buffer that is being typed in usually does not parse. Rather than answer
//! nothing for as long as that lasts, the index keeps the **last text that did
//! parse** and answers from it. What that costs is exactness: a position it
//! reports for a declaration in the same file is where the declaration was at
//! the last good parse, so it can be a few lines off while the edit is in
//! flight. Every answer carries the source it was computed from
//! ([`Resolved::file`]) so that the position is turned into a range against
//! that text, and so is at least a valid position in it. The name under the
//! cursor does not come from the stale text: it is read from the current text
//! by the lexer, which still succeeds for most broken code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aipl_index::{FileIndex, Index, Symbol};
use aipl_syntax::Span;

use crate::server::resolved;

/// The lexer scope every identifier token carries — the rule in
/// `lex_aipl.aipl` that matches a name. Turning a cursor position into a name
/// is a lexical question, so it is asked of the lexer rather than re-derived
/// from character classes here. `identifier_scope_is_current` pins it.
const IDENTIFIER_SCOPE: &str = "variable.other.aipl";

/// What a name resolved to — see [`Analyses::resolve`].
pub(crate) struct Resolved {
    pub symbol: Symbol,
    /// The file that declares it, and the source its declaration was indexed
    /// from (which the caller needs to turn the declaration's span into a
    /// range). `None` for a builtin that is described but not written anywhere.
    pub file: Option<(PathBuf, String)>,
    /// Whether it is a builtin rather than something a program declares.
    pub builtin: bool,
}

/// What a name stands for, whichever way a file spells it: the same value for
/// `add` where it is declared, for `add` where it is imported, and for `plus`
/// where it is imported `as plus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Declaration {
    /// A declaration in a file: the file, resolved, and the span of its name.
    Written { file: PathBuf, name_span: Span },
    /// A builtin, by the name an import list spells it.
    Builtin(String),
}

impl Resolved {
    /// What this name stands for, for comparing it with another resolution.
    pub fn declaration(&self) -> Option<Declaration> {
        if self.builtin {
            return Some(Declaration::Builtin(self.symbol.name.clone()));
        }
        let (file, _) = self.file.as_ref()?;
        Some(Declaration::Written {
            file: resolved(file),
            name_span: self.symbol.name_span.clone(),
        })
    }
}

/// The remembered work on one file.
#[derive(Default)]
struct Entry {
    /// The text the index was last brought up to, whether or not it parsed.
    /// Asking again about this text is free.
    asked: Option<String>,
    /// The last text that parsed, and so the one the index holds this file from.
    indexed: Option<String>,
    /// The text `identifiers` was found in, and the identifiers' spans — `None`
    /// when that text did not lex.
    lexed: Option<(String, Option<Vec<Span>>)>,
}

/// Every file the server has been asked about, indexed once per distinct text.
#[derive(Default)]
pub(crate) struct Analyses {
    index: Index,
    entries: HashMap<PathBuf, Entry>,
    /// How many times a file has been parsed, for the tests that hold the cache
    /// to doing each parse once.
    #[cfg(test)]
    parses: usize,
}

impl Analyses {
    /// Bring the index of `path` up to `text`, parsing only if `text` is not the
    /// one last asked about. A parse that fails leaves the previous good one.
    fn refresh(&mut self, path: &Path, text: &str) {
        let entry = self.entries.entry(path.to_path_buf()).or_default();
        if entry.asked.as_deref() == Some(text) {
            return;
        }
        entry.asked = Some(text.to_string());
        #[cfg(test)]
        {
            self.parses += 1;
        }
        if let Ok(file) = FileIndex::parse(path, text) {
            self.index.insert(file);
            entry.indexed = Some(text.to_string());
        }
    }

    /// The index of `path` as of `text` — or of the last text of it that
    /// parsed, with that text — or `None` if none ever has.
    pub fn file(&mut self, path: &Path, text: &str) -> Option<(&FileIndex, &str)> {
        self.refresh(path, text);
        let source = self.entries.get(path)?.indexed.as_deref()?;
        Some((self.index.file(path)?, source))
    }

    /// The identifier at `offset` in `text`, with its span.
    ///
    /// A cursor sitting immediately after a name counts as being in it — that is
    /// where it lands when you double-click a word and then invoke a command, and
    /// treating it as "between two tokens" would answer nothing for the commonest
    /// gesture there is.
    pub fn identifier_at(
        &mut self,
        path: &Path,
        text: &str,
        offset: usize,
    ) -> Option<(String, Span)> {
        let spans = self.identifiers(path, text)?;
        let found = spans
            .iter()
            .find(|span| span.contains(&offset))
            .or_else(|| spans.iter().find(|span| span.end == offset))?;
        Some((text[found.clone()].to_string(), found.clone()))
    }

    /// The span of every identifier in `text`, or `None` if it does not lex.
    /// Lexed once per distinct text.
    fn identifiers(&mut self, path: &Path, text: &str) -> Option<&[Span]> {
        let entry = self.entries.entry(path.to_path_buf()).or_default();
        if entry.lexed.as_ref().map(|(seen, _)| seen.as_str()) != Some(text) {
            let spans = aipl_parser::token_scopes(text).ok().map(|tokens| {
                tokens
                    .into_iter()
                    .filter(|token| token.scope == IDENTIFIER_SCOPE)
                    .map(|token| token.span)
                    .collect()
            });
            entry.lexed = Some((text.to_string(), spans));
        }
        entry.lexed.as_ref()?.1.as_deref()
    }

    /// The spans in `text` (the file at `path`) of every identifier that stands
    /// for `target`, which `name` is the declared name of.
    ///
    /// A file spells `target` by `name`, or by the alias an import gives it, so
    /// only identifiers spelled one of those are resolved — and each is resolved
    /// as [`Analyses::resolve`] resolves a cursor on it, because a spelling that
    /// reaches a different declaration (another file's `add`) is not a mention.
    pub fn mentions(
        &mut self,
        path: &Path,
        text: &str,
        target: &Declaration,
        name: &str,
        read: &dyn Fn(&Path) -> Option<String>,
    ) -> Vec<Span> {
        let Some((file, _)) = self.file(path, text) else {
            return Vec::new();
        };
        let mut spellings = vec![name.to_string()];
        spellings.extend(
            file.imports
                .iter()
                .filter(|import| import.name == name)
                .map(|import| import.local.clone()),
        );
        let Some(spans) = self.identifiers(path, text).map(<[Span]>::to_vec) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for span in spans {
            let spelled = &text[span.clone()];
            if !spellings.iter().any(|spelling| spelling == spelled) {
                continue;
            }
            let resolved = self.resolve(path, text, spelled, span.start, read);
            if resolved.and_then(|r| r.declaration()).as_ref() == Some(target) {
                found.push(span);
            }
        }
        found
    }

    /// Where `name` is declared, as the file at `path` (whose text is `text`)
    /// sees it: the declaration, and — when it is written somewhere — the
    /// defining file and its source.
    ///
    /// `read` gives the current text of an imported file: the open buffer if
    /// there is one, otherwise what is on disk. Only the import that binds
    /// `name` is read and indexed, so a request costs one imported file at
    /// most, whatever the file imports.
    ///
    /// Resolution is [`Index::definition_of`]'s, so a name declared in the file
    /// wins and an `import` otherwise says which file to look in. A name
    /// imported `from builtins` resolves to the builtin. So does the builtin's
    /// own name in the import list when the cursor (`offset`) is in that import:
    /// `equal` in `equal as ==` is not a name the file binds, but it is what the
    /// author is pointing at.
    pub fn resolve(
        &mut self,
        path: &Path,
        text: &str,
        name: &str,
        offset: usize,
        read: &dyn Fn(&Path) -> Option<String>,
    ) -> Option<Resolved> {
        let (file, _) = self.file(path, text)?;
        // `add` in `add as plus` binds nothing here — `plus` is the local name —
        // but in the import list it is what the author is pointing at, so it
        // stands for the import that spells it.
        let spelled_in_import = file
            .imports
            .iter()
            .find(|i| i.local != name && i.name == name && i.span.contains(&offset))
            .cloned();
        // An import path is relative to the importing file's directory, exactly
        // as `definition_of` resolves it, and the imported file is keyed by it.
        let imported = spelled_in_import
            .as_ref()
            .or_else(|| file.import_of(name))
            .and_then(|import| import.from.as_ref())
            .map(|from| path.parent().unwrap_or(Path::new("")).join(from));
        if let Some(target) = &imported {
            if let Some(source) = read(target) {
                self.refresh(target, &source);
            }
        }
        let file = self.index.file(path)?;
        let declaration = match (&spelled_in_import, &imported) {
            (Some(import), Some(target)) => self
                .index
                .file(target)
                .and_then(|file| Some((file.path.as_path(), file.define(&import.name)?))),
            _ => self.index.definition_of(file, name),
        };
        if let Some((target, symbol)) = declaration {
            let source = self.entries.get(target)?.indexed.clone()?;
            return Some(Resolved {
                symbol: symbol.clone(),
                file: Some((target.to_path_buf(), source)),
                builtin: false,
            });
        }
        let builtin_name = match file.import_of(name) {
            Some(import) if import.from.is_none() => &import.name,
            Some(_) => return None,
            None => {
                &file
                    .imports
                    .iter()
                    .find(|i| i.from.is_none() && i.name == name && i.span.contains(&offset))?
                    .name
            }
        };
        let found = aipl_index::builtin(builtin_name)?;
        Some(Resolved {
            symbol: found.symbol,
            file: found.file,
            builtin: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: &str = "import { add } from \"./util.aipl\";\nfn main() { add(1, 2); }\n";
    const UTIL: &str = "# Adds.\npub fn add(a: i64, b: i64) -> i64 { a }\n";

    fn analyses() -> Analyses {
        aipl_codegen::install_parser_hooks();
        Analyses::default()
    }

    fn at(text: &str, snippet: &str) -> usize {
        text.find(snippet).expect("snippet in text")
    }

    fn resolve(analyses: &mut Analyses, text: &str) -> Option<Resolved> {
        let offset = at(text, "add(1");
        analyses.resolve(Path::new("/w/main.aipl"), text, "add", offset, &|path| {
            (path == Path::new("/w/./util.aipl")).then(|| UTIL.to_string())
        })
    }

    #[test]
    fn a_text_is_parsed_once_however_often_it_is_asked_about() {
        let mut analyses = analyses();
        for _ in 0..5 {
            assert!(resolve(&mut analyses, MAIN).is_some());
        }
        // The file, and the one file it imports.
        assert_eq!(analyses.parses, 2);
    }

    #[test]
    fn only_the_import_that_binds_the_name_is_read() {
        let mut analyses = analyses();
        let text = "import { add } from \"./util.aipl\";\nimport { x } from \"./other.aipl\";\nfn main() { add(1, 2); }\n";
        assert!(resolve(&mut analyses, text).is_some());
        assert_eq!(
            analyses.parses, 2,
            "main.aipl and util.aipl, not other.aipl"
        );
    }

    #[test]
    fn a_new_text_is_parsed_again() {
        let mut analyses = analyses();
        let path = Path::new("/w/a.aipl");
        assert_eq!(
            analyses
                .file(path, "fn a() {}\n")
                .map(|(f, _)| f.symbols.len()),
            Some(1)
        );
        assert_eq!(
            analyses
                .file(path, "fn a() {}\nfn b() {}\n")
                .map(|(f, _)| f.symbols.len()),
            Some(2)
        );
        assert_eq!(analyses.parses, 2);
    }

    #[test]
    fn a_text_that_does_not_parse_keeps_the_last_good_index_and_is_tried_once() {
        let mut analyses = analyses();
        let path = Path::new("/w/a.aipl");
        analyses.file(path, "fn a() {}\n");
        for _ in 0..3 {
            let (file, source) = analyses.file(path, "fn a() {}\nfn b(").expect("last good");
            assert_eq!(file.symbols.len(), 1);
            assert_eq!(source, "fn a() {}\n");
        }
        assert_eq!(analyses.parses, 2, "the broken text is attempted once");
    }

    #[test]
    fn nothing_that_never_parsed_is_answered() {
        let mut analyses = analyses();
        assert!(analyses.file(Path::new("/w/a.aipl"), "fn (").is_none());
    }

    #[test]
    fn an_identifier_is_found_from_one_lex_per_text() {
        let mut analyses = analyses();
        let path = Path::new("/w/a.aipl");
        let text = "fn alpha() {}\n";
        let at = |analyses: &mut Analyses, offset| analyses.identifier_at(path, text, offset);
        assert_eq!(at(&mut analyses, 4), Some(("alpha".to_string(), 3..8)));
        assert_eq!(
            at(&mut analyses, 8),
            Some(("alpha".to_string(), 3..8)),
            "just after it"
        );
        assert_eq!(at(&mut analyses, 0), None, "a keyword is not a name");
    }
}
