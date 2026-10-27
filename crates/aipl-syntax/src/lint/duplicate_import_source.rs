use crate::ast::{ImportDecl, ImportSource, Item, Program};
use crate::Error;
use std::collections::HashSet;

/// `import { a } from builtins;` and then `import { b } from builtins;` — the
/// same source imported on more than one line. Every import from a given source
/// (`builtins`, or one path) is a single `import { .. }` statement, so a reader
/// finds everything a file takes from it in one place. The advice is to merge the
/// names into the first.
///
/// Points at the source of each redundant import (`builtins`, or the path),
/// which sits on the import's one line however its names are laid out.
pub(super) fn duplicate_import_source(program: &Program, hits: &mut Vec<Error>) {
    let mut seen_builtins = false;
    let mut seen_paths: HashSet<&str> = HashSet::new();
    for item in &program.items {
        let Item::Import(ImportDecl { source, .. }) = item else {
            continue;
        };
        match source {
            ImportSource::Builtins { span } => {
                if seen_builtins {
                    hits.push(Error::at(
                        "duplicate import from \"builtins\"; merge the names into the first \
                         `import { .. } from builtins;` (or append #[allow] to this line to keep it)",
                        span.clone(),
                    ));
                }
                seen_builtins = true;
            }
            ImportSource::Path { path, span } => {
                if !seen_paths.insert(path) {
                    hits.push(Error::at(
                        format!(
                            "duplicate import from {path:?}; merge the names into the first \
                             `import {{ .. }} from {path:?};` (or append #[allow] to this line to keep it)"
                        ),
                        span.clone(),
                    ));
                }
            }
        }
    }
}
