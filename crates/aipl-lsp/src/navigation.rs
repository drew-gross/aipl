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
use aipl_syntax::ast::{Item, TypeParam};
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
    pub fn new(frontends: &[&Frontend], root: &std::path::Path, root_text: &str) -> Navigation {
        let program = &frontends[0].program;
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
        let mut parameters: HashMap<(&str, &str), Site> = HashMap::new();
        let mut structs: HashMap<&str, Site> = HashMap::new();
        let mut variants: HashMap<&str, Site> = HashMap::new();
        let mut fields: HashMap<(&str, &str), Site> = HashMap::new();
        let mut cases: HashMap<(&str, &str), Site> = HashMap::new();
        let mut declarations: Vec<Site> = Vec::new();
        // The items that declare type parameters, with where each starts in its
        // file: an item runs from its name to the next item's, so a type
        // parameter written in an annotation belongs to the item it sits in.
        let mut item_starts: HashMap<PathBuf, Vec<(usize, Vec<&TypeParam>)>> = HashMap::new();
        for item in frontends.iter().flat_map(|f| &f.program.items) {
            match item {
                Item::Fn(f) if f.name.starts_with("__test") => {}
                Item::Fn(f) => {
                    if let Some(file) = file_of(&f.name).filter(|_| written(&f.name.span)) {
                        item_starts
                            .entry(file)
                            .or_default()
                            .push((f.name.span.start, f.sig.type_vars.iter().collect()));
                    }
                    if let Some(file) = file_of(&f.name).filter(|_| written(&f.name.span)) {
                        let site = Site {
                            file,
                            span: f.name.span.clone(),
                        };
                        functions.insert(&f.name, site.clone());
                        declarations.push(site);
                    }
                    // A parameter is a place to jump to whether or not the body
                    // reads it: a keyword parameter may only be supplied by a call.
                    if let Some(file) = file_of(&f.name) {
                        for param in f.sig.params.iter().filter(|p| written(&p.name_span.0)) {
                            let site = Site {
                                file: file.clone(),
                                span: param.name_span.0.clone(),
                            };
                            parameters.insert((&f.name, &param.name), site.clone());
                            declarations.push(site);
                        }
                    }
                }
                Item::Struct(s) => {
                    let Some(file) = file_of(&s.name) else {
                        continue;
                    };
                    if written(&s.name.span) {
                        item_starts
                            .entry(file.clone())
                            .or_default()
                            .push((s.name.span.start, s.type_vars.iter().collect()));
                    }
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
                Item::Variant(v) => {
                    if let Some(file) = file_of(&v.name).filter(|_| written(&v.name.span)) {
                        item_starts
                            .entry(file)
                            .or_default()
                            .push((v.name.span.start, v.type_vars.iter().collect()));
                    }
                    if let Some(file) = file_of(&v.name).filter(|_| written(&v.name.span)) {
                        let site = Site {
                            file,
                            span: v.name.span.clone(),
                        };
                        variants.insert(&v.name, site.clone());
                        declarations.push(site);
                    }
                    if let Some(file) = file_of(&v.name) {
                        for case in v.cases.iter().filter(|c| written(&c.name.span)) {
                            let site = Site {
                                file: file.clone(),
                                span: case.name.span.clone(),
                            };
                            cases.insert((&v.name, &case.name), site.clone());
                            declarations.push(site);
                        }
                    }
                }
                _ => {}
            }
        }

        for owners in item_starts.values_mut() {
            owners.sort_by_key(|(start, _)| *start);
            owners.dedup_by_key(|(start, _)| *start);
        }
        for (file, owners) in &item_starts {
            for (_, type_vars) in owners {
                for type_var in type_vars.iter().filter(|t| written(&t.name_span.0)) {
                    declarations.push(Site {
                        file: file.clone(),
                        span: type_var.name_span.0.clone(),
                    });
                }
            }
        }

        let mut uses: Vec<(Site, Found)> = Vec::new();
        // The names in each file's import lists, which the loader bound.
        for source in program.sources.iter().filter(|s| !s.path.is_empty()) {
            let file = resolved(Path::new(&source.path));
            // Names written in type annotations: a type, never a function.
            for written in &source.type_uses {
                let declared = structs
                    .get(written.target.as_str())
                    .or_else(|| variants.get(written.target.as_str()))
                    .cloned();
                if let Some(site) = declared {
                    uses.push((
                        Site {
                            file: file.clone(),
                            span: written.span.clone(),
                        },
                        Found::Declared(site),
                    ));
                }
            }
            // A type parameter written in an annotation is the one the item
            // holding it declares: the item that starts last before it.
            let owners = item_starts.get(&file).map(Vec::as_slice).unwrap_or(&[]);
            for at in &source.type_var_uses {
                let Some(name) = source.source.get(at.clone()) else {
                    continue;
                };
                let Some((_, type_vars)) =
                    owners.iter().rev().find(|(start, _)| *start <= at.start)
                else {
                    continue;
                };
                if let Some(declared) = type_vars.iter().find(|t| t.name == name) {
                    if written(&declared.name_span.0) {
                        uses.push((
                            Site {
                                file: file.clone(),
                                span: at.clone(),
                            },
                            Found::Declared(Site {
                                file: file.clone(),
                                span: declared.name_span.0.clone(),
                            }),
                        ));
                    }
                }
            }
            // A keyword argument's name is the parameter it was passed for.
            for keyword in &source.keyword_uses {
                if let Some(site) =
                    parameters.get(&(keyword.callee.as_str(), keyword.param.as_str()))
                {
                    uses.push((
                        Site {
                            file: file.clone(),
                            span: keyword.span.clone(),
                        },
                        Found::Declared(site.clone()),
                    ));
                }
            }
            for import in &source.imports {
                let found = match import.target.strip_prefix("__builtin_") {
                    Some(builtin) => builtin_found(builtin, &mut sources),
                    None => functions
                        .get(import.target.as_str())
                        .or_else(|| structs.get(import.target.as_str()))
                        .or_else(|| variants.get(import.target.as_str()))
                        .cloned()
                        .map(Found::Declared),
                };
                if let Some(found) = found {
                    uses.push((
                        Site {
                            file: file.clone(),
                            span: import.span.clone(),
                        },
                        found,
                    ));
                }
            }
        }
        for resolution in frontends.iter().flat_map(|f| &f.resolutions) {
            let Some(file) = file_of(&resolution.in_fn) else {
                continue;
            };
            let found = match &resolution.target {
                Target::Field { owner, field } => fields
                    .get(&(owner.as_str(), field.as_str()))
                    .cloned()
                    .map(Found::Declared),
                Target::Struct(name) => structs.get(name.as_str()).cloned().map(Found::Declared),
                Target::Case { variant, case } => cases
                    .get(&(variant.as_str(), case.as_str()))
                    .cloned()
                    .map(Found::Declared),
                Target::Function(name) => match name.strip_prefix("__builtin_") {
                    Some(builtin) => builtin_found(builtin, &mut sources),
                    None => functions.get(name.as_str()).cloned().map(Found::Declared),
                },
                Target::Local(span) => {
                    let site = Site {
                        file: file.clone(),
                        span: span.clone(),
                    };
                    declarations.push(site.clone());
                    Some(Found::Declared(site))
                }
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
        // The program and its test program share every non-test function, so
        // the same finding can arrive twice.
        uses.sort_by(|a, b| {
            (&a.0.file, a.0.span.start, a.0.span.end).cmp(&(
                &b.0.file,
                b.0.span.start,
                b.0.span.end,
            ))
        });
        uses.dedup();
        declarations.sort_by(|a, b| {
            (&a.file, a.span.start, a.span.end).cmp(&(&b.file, b.span.start, b.span.end))
        });
        declarations.dedup();
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
