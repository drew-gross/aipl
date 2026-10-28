//! Go-to-definition and find-references, answered by the compiler's own
//! decisions.
//!
//! The type checker has to work out what every field read, call and struct
//! literal refers to in order to type a program ([`aipl_syntax::resolution`]).
//! A [`Navigation`] is that, turned into places in files: for every name the
//! checker resolved, where it is written and where its declaration is. Nothing
//! here looks at a name's spelling to decide what it means, so `p.name`, `name(p)`
//! and `T { name: v }` go where the compiler sends them and a field never
//! resolves to a function that happens to share its name.
//!
//! A navigation exists only for a program the checker accepted, and describes
//! exactly the text it was built from. A request made before that, or against a
//! document that does not compile, is answered with an error saying so rather
//! than with an empty result, so that "the server is still working" and "this
//! does not compile" are never mistaken for "nothing refers to this".

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aipl_codegen::Frontend;
use aipl_syntax::ast::Item;
use aipl_syntax::resolution::Target;
use aipl_syntax::{mangled_file_index, Span};

use crate::server::resolved;

/// Where a name is written or declared: a file, resolved, and the span of the
/// name in it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Site {
    pub file: PathBuf,
    pub span: Span,
}

/// What the compiler sent a written name to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Found {
    /// A declaration written in a file.
    Declared(Site),
    /// A builtin with no source to jump to: the compiler expands it itself.
    Native(String),
}

/// What a navigation holds about one program. See the module docs.
#[derive(Debug, Default)]
pub(crate) struct Navigation {
    /// Each written name the checker resolved, and what it resolved to.
    uses: Vec<(Site, Found)>,
    /// Each declaration name written in the program, which is also the thing it
    /// resolves to: the cursor on a field's own declaration asks for its uses.
    declarations: Vec<Site>,
    /// The text of every file the program was built from, as the checker saw it,
    /// because a span means nothing against any other version of its file.
    sources: HashMap<PathBuf, String>,
}

/// The answer to "what is under the cursor".
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Under {
    /// A name the compiler resolved to a declaration written in a file.
    Declared(Site),
    /// A name the compiler resolved to a builtin that has no source.
    Native(String),
    /// Nothing the compiler resolved — neither a use nor a declaration it knows.
    Nothing,
}

impl Navigation {
    /// What the checker decided about `frontend`'s program, read as places.
    ///
    /// `root` is the file the program was loaded from. Its items keep their
    /// source names while every other file's are mangled with the file's index,
    /// which is how a name is traced back to a file (see
    /// [`aipl_syntax::mangled_file_index`]).
    pub fn new(frontend: &Frontend, root: &std::path::Path, root_text: &str) -> Navigation {
        let program = &frontend.program;
        let root = resolved(root);
        let mut sources: HashMap<PathBuf, String> = program
            .sources
            .iter()
            .filter(|source| !source.path.is_empty())
            .map(|source| (resolved(Path::new(&source.path)), source.source.clone()))
            .collect();
        // The root was read from the editor's buffer, which the loader's own
        // copy of it already is — but say so rather than rely on it.
        sources.insert(root.clone(), root_text.to_string());
        let files: Vec<Option<PathBuf>> = program
            .sources
            .iter()
            .map(|source| {
                (!source.path.is_empty()).then(|| resolved(PathBuf::from(&source.path).as_path()))
            })
            .collect();
        // The file an item's name was written in. A builtin's own sources are
        // not part of what a program's author wrote, so nothing in one is a
        // place to jump from or to.
        let file_of = |name: &str| -> Option<PathBuf> {
            if name.starts_with("__builtin_") {
                return None;
            }
            match mangled_file_index(name) {
                Some(index) => files.get(index as usize)?.clone(),
                None => Some(root.clone()),
            }
        };

        let mut functions: HashMap<&str, Site> = HashMap::new();
        let mut structs: HashMap<&str, Site> = HashMap::new();
        let mut fields: HashMap<(&str, &str), Site> = HashMap::new();
        let mut declarations: Vec<Site> = Vec::new();
        for item in &program.items {
            match item {
                Item::Fn(f) => {
                    if let Some(file) = file_of(&f.name).filter(|_| written(&f.name.span)) {
                        let site = Site {
                            file,
                            span: f.name.span.clone(),
                        };
                        functions.insert(&f.name, site.clone());
                        declarations.push(site);
                    }
                }
                Item::Struct(s) => {
                    let Some(file) = file_of(&s.name) else {
                        continue;
                    };
                    if written(&s.name.span) {
                        let site = Site {
                            file: file.clone(),
                            span: s.name.span.clone(),
                        };
                        structs.insert(&s.name, site.clone());
                        declarations.push(site);
                    }
                    for field in s.fields.iter().filter(|f| written(&f.name.span)) {
                        let site = Site {
                            file: file.clone(),
                            span: field.name.span.clone(),
                        };
                        fields.insert((&s.name, &field.name), site.clone());
                        declarations.push(site);
                    }
                }
                _ => {}
            }
        }

        let mut uses: Vec<(Site, Found)> = Vec::new();
        for resolution in &frontend.resolutions {
            let Some(file) = file_of(&resolution.in_fn) else {
                continue;
            };
            let found = match &resolution.target {
                Target::Field { owner, field } => fields
                    .get(&(owner.as_str(), field.as_str()))
                    .cloned()
                    .map(Found::Declared),
                Target::Struct(name) => structs.get(name.as_str()).cloned().map(Found::Declared),
                Target::Function(name) => match name.strip_prefix("__builtin_") {
                    Some(builtin) => builtin_found(builtin, &mut sources),
                    None => functions.get(name.as_str()).cloned().map(Found::Declared),
                },
                Target::Local(span) => Some(Found::Declared(Site {
                    file: file.clone(),
                    span: span.clone(),
                })),
            };
            if let Some(found) = found {
                uses.push((
                    Site {
                        file,
                        span: resolution.at.clone(),
                    },
                    found,
                ));
            }
        }
        Navigation {
            uses,
            declarations,
            sources,
        }
    }

    /// What the compiler resolved the name at `offset` of `file` to.
    pub fn under(&self, file: &std::path::Path, offset: usize) -> Under {
        let file = resolved(file);
        let covers =
            |site: &Site| site.file == file && site.span.start <= offset && offset <= site.span.end;
        let used = self
            .uses
            .iter()
            .filter(|(site, _)| covers(site))
            .min_by_key(|(site, _)| site.span.len());
        if let Some((_, found)) = used {
            return match found {
                Found::Declared(site) => Under::Declared(site.clone()),
                Found::Native(name) => Under::Native(name.clone()),
            };
        }
        match self.declarations.iter().find(|site| covers(site)) {
            Some(site) => Under::Declared(site.clone()),
            None => Under::Nothing,
        }
    }

    /// The source `file` had when the program was checked.
    pub fn source(&self, file: &Path) -> Option<&str> {
        self.sources.get(file).map(String::as_str)
    }

    /// Every written use of `declaration` this program has, in source order.
    pub fn uses_of(&self, declaration: &Site) -> Vec<Site> {
        let mut found: Vec<Site> = self
            .uses
            .iter()
            .filter(|(_, found)| matches!(found, Found::Declared(site) if site == declaration))
            .map(|(site, _)| site.clone())
            .collect();
        found.sort_by(|a, b| (&a.file, a.span.start).cmp(&(&b.file, b.span.start)));
        found
    }
}

/// A span that points at text: the empty one is the placeholder a synthesized
/// declaration carries.
fn written(span: &Span) -> bool {
    span.start < span.end
}

/// A builtin written in AIPL is a place to jump to; a native one is not.
fn builtin_found(name: &str, sources: &mut HashMap<PathBuf, String>) -> Option<Found> {
    let builtin = aipl_index::builtin(name)?;
    Some(match builtin.file {
        Some((file, source)) => {
            let file = resolved(&file);
            sources.entry(file.clone()).or_insert(source);
            Found::Declared(Site {
                file,
                span: builtin.symbol.name_span,
            })
        }
        None => Found::Native(name.to_string()),
    })
}
