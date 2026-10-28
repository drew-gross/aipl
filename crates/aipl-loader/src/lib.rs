//! Resolve `import { foo, bar } from "./util.aipl";` declarations and
//! flatten a transitively-imported source tree into a single [`Program`].
//!
//! Each file is parsed once (cycles are safe — the visited-set is keyed on
//! canonical path). Top-level items in non-root files are renamed to
//! `__m{N}__{name}` so multiple files can define the same symbol without
//! colliding. Each file then gets a *view*: a map from the names it can
//! use (its own items + names it explicitly imported) to those mangled
//! global names. Item bodies are then rewritten so every `Call`,
//! `Construct`, and `Type::Named` reference resolves through the view.
//!
//! The root file's items keep their original names — codegen still expects
//! to find an unmangled `main`.

mod bake;
mod kwargs;
pub use bake::set_source_loc_hook;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use aipl_parser::parse_with_allows;
use aipl_syntax::ast::Callee;
use aipl_syntax::ast::{
    Expr, ExprKind, FieldInit, Function, ImportDecl, ImportName, ImportSource, Item, LambdaParam,
    MatchArm, Param, Pattern, Program, Signature, StructDecl, Type, TypeParam,
};
use aipl_syntax::{DebugOptions, Error, Span};

/// The extension an AIPL source file carries, without the dot.
pub const SOURCE_EXTENSION: &str = "aipl";

/// Whether `path` names an AIPL source file, which is to say ends in `.aipl`.
/// Fails with the error to report when it does not, naming the path. Case matters:
/// `.AIPL` is not `.aipl`. Suitable for refusing a file before it is read.
pub fn require_source_extension(path: &Path) -> Result<(), Error> {
    if path.extension().is_some_and(|ext| ext == SOURCE_EXTENSION) {
        return Ok(());
    }
    Err(Error::msg(format!(
        "{}: not an AIPL source file, whose name must end in `.{SOURCE_EXTENSION}`",
        path.display()
    )))
}

/// Parse `root`, recursively resolve every `import`, and return a single
/// merged [`Program`] ready for codegen.
///
/// The project root — what a `trace(..)` names its file relative to — is the
/// current working directory. That is right for every command that compiles
/// where it was invoked; `aipl check` runs its tests from a scratch directory
/// and so passes its own, through [`load_program_rooted`].
pub fn load_program(root: &Path, dbg: DebugOptions) -> Result<Program, Vec<Error>> {
    let here = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    load_program_rooted(root, &here, dbg)
}

/// [`load_program`] with the project root given rather than taken from the
/// working directory.
///
/// `project_root` decides one thing: how a `trace(expr)` names the file it sits
/// in (`subdir/file.aipl:5 v + 2 = 7`). A caller that changes directory between
/// being invoked and compiling — `aipl check`, which stages each file's
/// `--- file: ---` companions into a scratch directory and runs the tests
/// there — must pass the directory the *user* named paths relative to, or every
/// trace would be labelled against the scratch directory instead.
pub fn load_program_rooted(
    root: &Path,
    project_root: &Path,
    dbg: DebugOptions,
) -> Result<Program, Vec<Error>> {
    require_source_extension(root)?;
    let root_canon = canonicalize(root)?;
    let mut loader = Loader {
        dbg,
        entry: Some(root_canon.clone()),
        // Canonicalized so it shares a spelling with the file paths it is
        // stripped from — those are canonical, and on macOS a working
        // directory under `/var` or `/tmp` is a symlink into `/private`, which
        // would otherwise never match.
        project_root: project_root.canonicalize().ok(),
        ..Loader::default()
    };
    loader.load(&root_canon)?;
    loader.load_builtins(&root_canon)?;
    dbg.trace(
        "loader",
        format_args!("flattening {} file(s)", loader.files.len()),
    );
    Ok(loader.flatten(&root_canon)?)
}

/// [`load_program`] with the root file's source supplied in memory rather than
/// read from disk, while its imports still resolve from disk relative to
/// `root`'s directory.
///
/// This is what an editor needs: the buffer being typed into has not been
/// saved, so the file on disk is the wrong source for it, but every file it
/// imports is on disk and current. Only the root is overlaid — an unsaved
/// *imported* file still loads as it was last written, which is the honest
/// limitation of a one-file overlay rather than a surprise, since the error
/// lands with its own file's name on it either way.
///
/// `root` must exist on disk: it is canonicalized, so that the same file
/// reached again through an import of its own is recognized as already loaded
/// rather than read a second time from under the overlay.
pub fn load_program_overlay(
    root: &Path,
    source: &str,
    dbg: DebugOptions,
) -> Result<Program, Vec<Error>> {
    require_source_extension(root)?;
    let root_canon = canonicalize(root)?;
    let mut loader = Loader {
        dbg,
        entry: Some(root_canon.clone()),
        ..Loader::default()
    };
    loader.load_source(&root_canon, source)?;
    loader.load_builtins(&root_canon)?;
    Ok(loader.flatten(&root_canon)?)
}

/// Like [`load_program`] but with the root file's source supplied in memory
/// (used by the embedding FFI). Any `from "..."` path imports resolve relative
/// to the current directory; `from builtins` works as usual.
pub fn load_program_str(source: &str, dbg: DebugOptions) -> Result<Program, Vec<Error>> {
    let mut loader = Loader {
        dbg,
        ..Loader::default()
    };
    // A synthetic root under the current dir: it need not exist on disk, but
    // gives relative path imports a base directory and a stable map key.
    let root = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("__aipl_ffi_root__.aipl");
    loader.entry = Some(root.clone());
    loader.load_source(&root, source)?;
    loader.load_builtins(&root)?;
    Ok(loader.flatten(&root)?)
}

/// Like [`load_program`] but over a set of in-memory virtual files, supplied as
/// `(name, source)` pairs (typically each `include_str!`'d into the host
/// binary, so nothing is read from disk at runtime). The **first** pair is the
/// root/entry — its functions keep their original names, ready to be called.
/// `from builtins` works as usual.
///
/// Every name must be written as a relative path — `"./util.aipl"`, not
/// `"util.aipl"` — and a `from "..."` path import resolves *by exact match*
/// against those names, so an import must spell its target the same way. Nothing
/// is normalized: requiring the one spelling keeps a name meaning exactly one
/// file, and keeps these names identical to the `from "./..."` clauses the same
/// sources use when they're loaded from disk instead.
pub fn load_program_sources(
    sources: &[(&str, &str)],
    dbg: DebugOptions,
) -> Result<Program, Vec<Error>> {
    let Some((root_name, _)) = sources.first() else {
        return Err(Error::msg("load_program_sources: no sources provided").into());
    };
    let root = PathBuf::from(root_name);
    let mut loader = Loader {
        dbg,
        entry: Some(root.clone()),
        ..Loader::default()
    };
    for (name, src) in sources {
        // Propagate a parse/lint error (don't `unwrap`) so callers can render it
        // and pin the offending source rather than panicking opaquely.
        loader.register_virtual(PathBuf::from(name), src)?;
    }
    loader.check_virtual_imports()?;
    loader.load_builtins(&root)?;
    Ok(loader.flatten(&root)?)
}

#[derive(Default)]
struct Loader {
    /// Loaded files keyed by canonical path. A `BTreeMap` (not `HashMap`) so
    /// iteration — in particular the Pass-3 merge that fixes the output
    /// program's item order — is deterministic across runs. (A `HashMap`'s
    /// per-process random order would make a multi-file program's emitted
    /// function order, and thus its `binary size`, non-deterministic.)
    files: BTreeMap<PathBuf, LoadedFile>,
    next_index: u32,
    dbg: DebugOptions,
    /// The root file, set before loading starts. Diagnostics raised against
    /// *any other* file are tagged with the source they index — see
    /// `tag_origin`.
    entry: Option<PathBuf>,
    /// The files loaded as the AIPL-implemented builtin they implement (see
    /// [`Loader::load_builtins`]) — the one place
    /// [`aipl_syntax::INTERNAL_BUILTINS`] may be imported, and where a `pub fn`
    /// is named by its builtin's canonical name.
    builtin_files: HashSet<PathBuf>,
    /// The directory a baked source location names its file relative to — see
    /// [`load_program_rooted`] and [`bake::location_label`]. `None` for the
    /// in-memory entry points, whose files are named by the caller already.
    project_root: Option<PathBuf>,
}

/// Whether `path` is an AIPL-implemented builtin's source —
/// `crates/aipl-mono/src/builtin_<name>.aipl`.
fn is_builtin_impl_path(path: &Path) -> bool {
    let named_like_one = path
        .file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| f.starts_with("builtin_") && f.ends_with(".aipl"));
    let in_mono_src = path
        .parent()
        .is_some_and(|d| d.ends_with(Path::new("aipl-mono").join("src")));
    named_like_one && in_mono_src
}

struct LoadedFile {
    index: u32,
    /// This file's text, kept so `flatten` can hand the merged program a source
    /// map (`Program::sources`). A post-load pass raising a diagnostic against
    /// an item from this file needs the source its spans index; by then only the
    /// mangled name says which file that is.
    source: String,
    /// Functions + structs from this file (imports stripped out).
    items: Vec<Item>,
    /// `import { ... } from "./other"` declarations. `from` resolved to
    /// a canonical path.
    imports: Vec<ResolvedImport>,
    /// `import { ... } from builtins;` — the builtin names this file
    /// brings into scope (each possibly aliased).
    builtin_imports: Vec<ImportName>,
    /// Whether the file writes a grid type, which makes grid operations of its
    /// indexes and literals (see [`aipl_syntax::aipl_builtins::companions`]).
    mentions_grid: bool,
    /// The AIPL-implemented builtins the file calls without importing them: the
    /// ones its syntax is written as, such as `v[row, column]`'s `grid_get`.
    syntax_builtins: Vec<Callee>,
}

struct ResolvedImport {
    names: Vec<ImportName>,
    from: PathBuf,
}

impl Loader {
    fn load(&mut self, path: &Path) -> Result<(), Vec<Error>> {
        if self.files.contains_key(path) {
            return Ok(());
        }
        self.dbg
            .trace("loader", format_args!("load `{}`", path.display()));
        let src = fs::read_to_string(path)
            .map_err(|e| Error::msg(format!("read {}: {e}", path.display())))?;
        self.load_source(path, &src)
    }

    /// Parse `src` as the file at `path`, register it, and recurse into its
    /// path imports. `load` reads `src` from disk; the FFI supplies it directly.
    fn load_source(&mut self, path: &Path, src: &str) -> Result<(), Vec<Error>> {
        let is_entry = self.entry.as_deref() == Some(path);
        let root = self.project_root.clone();
        self.load_source_inner(path, src)
            .map_err(|errs| tag_origin(is_entry, path, root.as_deref(), src, errs))
    }

    fn load_source_inner(&mut self, path: &Path, src: &str) -> Result<(), Vec<Error>> {
        if self.files.contains_key(path) {
            return Ok(());
        }
        // Keep the original Error (including span) so `render()` can show
        // the caret pointing into the imported file's source. The path
        // tied to that source is implicit from the source line itself.
        let program = self.parse_and_lint(src)?;
        let (items, imports, builtin_imports) =
            self.prepare(program, path, src, |from, span| {
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                let candidate = parent.join(from);
                // Before the file is looked for, so a wrong extension is
                // reported as that and not as a file that does not exist.
                require_source_extension(&candidate).map_err(|_| {
                    Error::at(
                        format!(
                        "import {from:?}: an imported file's name must end in `.{SOURCE_EXTENSION}`"
                    ),
                        span.clone(),
                    )
                })?;
                canonicalize(&candidate)
                    .map_err(|e| Error::at(format!("import {from:?}: {}", e.message), span))
            })?;

        // Reserve the slot *before* recursing so cycles short-circuit at
        // `files.contains_key` above.
        let import_paths: Vec<PathBuf> = imports.iter().map(|i| i.from.clone()).collect();
        self.insert_file(path, src, items, imports, builtin_imports);
        for from in import_paths {
            self.load(&from)?;
        }
        Ok(())
    }

    /// Parse `src`, and run the per-file lints while the source (for line-scoped
    /// `#[allow]` squelching) is in hand. Before the `trace` rewrite in
    /// [`Loader::prepare`], so a lint sees what the author wrote rather than the
    /// block it desugars to.
    fn parse_and_lint(&self, src: &str) -> Result<Program, Vec<Error>> {
        let parsed = parse_with_allows(src)?;
        aipl_syntax::lint::check(&parsed.program, src, &parsed.allows, parsed.lints)?;
        Ok(parsed.program)
    }

    /// Check `program`, bake its reserved calls, and split its items into the
    /// declarations, the path imports (each resolved to a file by `resolve`, given
    /// the import's text and span) and the names imported from builtins.
    fn prepare(
        &self,
        mut program: Program,
        path: &Path,
        src: &str,
        mut resolve: impl FnMut(&str, Span) -> Result<PathBuf, Error>,
    ) -> Result<(Vec<Item>, Vec<ResolvedImport>, Vec<ImportName>), Vec<Error>> {
        bake::reserved_names_not_bound(&program)?;
        bake::bake(
            &mut program.items,
            src,
            &bake::location_label(path, self.project_root.as_deref()),
        )?;

        let mut items = Vec::new();
        let mut imports = Vec::new();
        let mut builtin_imports = Vec::new();
        for item in program.items {
            match item {
                Item::Import(ImportDecl {
                    names,
                    source,
                    source_span,
                }) => match source {
                    ImportSource::Path(from) => {
                        let from = resolve(&from, source_span)?;
                        imports.push(ResolvedImport { names, from });
                    }
                    ImportSource::Builtins => builtin_imports.extend(names),
                },
                other => items.push(other),
            }
        }
        Ok((items, imports, builtin_imports))
    }

    /// Give the file at `path` the next index and record it, answering whether
    /// `path` already held one (which it replaces).
    fn insert_file(
        &mut self,
        path: &Path,
        src: &str,
        items: Vec<Item>,
        imports: Vec<ResolvedImport>,
        builtin_imports: Vec<ImportName>,
    ) -> bool {
        let index = self.next_index;
        self.next_index += 1;
        self.files
            .insert(
                path.to_path_buf(),
                LoadedFile {
                    index,
                    source: src.to_string(),
                    mentions_grid: aipl_syntax::aipl_builtins::mentions_grid(&items),
                    syntax_builtins: syntax_builtins(&items),
                    items,
                    imports,
                    builtin_imports,
                },
            )
            .is_some()
    }

    /// Load, as files of this program, the AIPL-implemented builtins it
    /// reaches: every one a loaded file imports — the builtins' own files
    /// included, so the set closes over what they import in turn — and their
    /// [`companions`](aipl_syntax::aipl_builtins::companions), which passes
    /// after the checker introduce without any file naming them.
    ///
    /// A builtin file is the program's own only when it is the root, checked
    /// by `aipl check`: then its `.test` and `.performance` blocks run. Loaded
    /// for another program, those are dropped, as they are not that program's
    /// to run.
    fn load_builtins(&mut self, root: &Path) -> Result<(), Vec<Error>> {
        if is_builtin_impl_path(root) {
            self.builtin_files.insert(root.to_path_buf());
        }
        loop {
            let mut present: HashSet<Callee> = self
                .files
                .values()
                .flat_map(|f| &f.builtin_imports)
                .filter_map(|n| {
                    Callee::importable(&n.name).or_else(|| {
                        aipl_syntax::builtin::Builtin::exporting_name(&n.name)
                            .map(|b| b.callee.clone())
                    })
                })
                .chain(
                    self.files
                        .values()
                        .flat_map(|f| f.syntax_builtins.iter().cloned()),
                )
                // What a builtin file implements is present too, and may have
                // companions: the root of `aipl check` defines its builtin
                // rather than importing it.
                .chain(self.builtin_files.iter().filter_map(|p| {
                    aipl_syntax::builtin::Builtin::implemented_at(p).map(|b| b.callee.clone())
                }))
                .collect();
            let grid = self.files.values().any(|f| f.mentions_grid);
            present.extend(aipl_syntax::aipl_builtins::companions(&present, grid));
            let mut pending = Vec::new();
            for callee in &present {
                let Some(path) =
                    aipl_syntax::builtin::Builtin::of(callee).and_then(|b| b.source_path())
                else {
                    continue;
                };
                let path = canonicalize(&path)?;
                if self.builtin_files.insert(path.clone()) {
                    pending.push(path);
                }
            }
            if pending.is_empty() {
                return Ok(());
            }
            // A file's index is its place in the load order, and the index is in
            // the name of everything it defines (`__m<N>__f`). `present` is a
            // set with no order of its own, so the order is made here: left to
            // it, the same program would get different names, and a different
            // IR, from one run to the next.
            pending.sort();
            for path in pending {
                self.load(&path)?;
                if path != root {
                    let file = self.files.get_mut(&path).expect("just loaded");
                    file.items = std::mem::take(&mut file.items)
                        .into_iter()
                        .filter_map(without_tests)
                        .collect();
                }
            }
        }
    }

    /// Register an in-memory virtual file `key` (no disk access, no recursion —
    /// the FFI provides the whole set up front). Path imports resolve to other
    /// virtual files by normalized name; [`check_virtual_imports`] then verifies
    /// each target was actually supplied.
    ///
    /// [`check_virtual_imports`]: Loader::check_virtual_imports
    fn register_virtual(&mut self, key: PathBuf, src: &str) -> Result<(), Vec<Error>> {
        let is_entry = self.entry.as_deref() == Some(key.as_path());
        let path = key.clone();
        let root = self.project_root.clone();
        self.register_virtual_inner(key, src)
            .map_err(|errs| tag_origin(is_entry, &path, root.as_deref(), src, errs))
    }

    fn register_virtual_inner(&mut self, key: PathBuf, src: &str) -> Result<(), Vec<Error>> {
        let program = self.parse_and_lint(src)?;
        // A caller-supplied name, so a wrong one is a caller error to report,
        // not an invariant to assert: `load_program_sources` returns a `Result`
        // precisely so a host learns what to fix.
        if !key.starts_with("./") {
            return Err(Error::msg(format!(
                concat!(
                    "source name {:?} must be a relative path starting with \"./\" ",
                    "(e.g. \"./{}\"); imports resolve against these names by exact match"
                ),
                key,
                file_label(&key)
            ))
            .into());
        }
        let (items, imports, builtin_imports) = self.prepare(program, &key, src, |from, _| {
            Ok(virtual_import_path(&key, from))
        })?;
        if self.insert_file(&key, src, items, imports, builtin_imports) {
            return Err(Error::msg(format!(
                "duplicate source {:?} in compile_sources",
                file_label(&key)
            ))
            .into());
        }
        Ok(())
    }

    /// Verify every virtual file's path imports name a supplied source.
    fn check_virtual_imports(&self) -> Result<(), Error> {
        for (path, file) in &self.files {
            for imp in &file.imports {
                if !self.files.contains_key(&imp.from) {
                    return Err(Error::msg(format!(
                        "{}: imported module {:?} was not provided to compile_sources. Sources: {:?}",
                        file_label(path),
                        file_label(&imp.from),
                        self.files.keys()
                    )));
                }
            }
        }
        Ok(())
    }

    /// What a diagnostic about `path` calls it: the root-relative path, as the
    /// caret block's ` --> ` line does.
    fn label(&self, path: &Path) -> String {
        diagnostic_label(path, self.project_root.as_deref())
    }

    /// Tag `err` with the source of `path` so its caret lands in that file. The
    /// root file is left untagged — its caller renders against it already —
    /// as in `tag_origin`.
    fn attribute(&self, path: &Path, root: &Path, err: Error) -> Error {
        match self.files.get(path) {
            Some(file) if path != root => err.in_file(self.label(path), &file.source),
            _ => err,
        }
    }

    fn flatten(&self, root: &Path) -> Result<Program, Error> {
        // Pass 1: local view — names defined in the file map to their
        // (mangled or, for root, original) global names. Also record which
        // names another file may *import*: `pub` functions and all types
        // (structs/variants). A non-`pub` function is file-private.
        let mut local_views: HashMap<&PathBuf, HashMap<String, String>> = HashMap::new();
        let mut importables: HashMap<&PathBuf, HashSet<String>> = HashMap::new();
        // Where each file's local items are named, so a later import that
        // collides with one can point back at the definition.
        let mut local_spans: HashMap<&PathBuf, HashMap<String, Span>> = HashMap::new();
        // A variant's global name → its case names, for resolving `V.A` paths.
        let mut ctor_cases: HashMap<String, Vec<String>> = HashMap::new();
        for (path, file) in &self.files {
            let is_root = path == root;
            let mut view = HashMap::new();
            let mut exports = HashSet::new();
            let mut spans: HashMap<String, Span> = HashMap::new();
            for item in &file.items {
                let (name, name_span, importable) = match item {
                    Item::Fn(f) => (f.name.text.clone(), f.name.span.clone(), f.is_pub),
                    Item::Struct(s) => (s.name.text.clone(), s.name.span.clone(), true),
                    Item::Variant(v) => (v.name.text.clone(), v.name.span.clone(), true),
                    Item::Refinement(r) => (r.name.text.clone(), r.name.span.clone(), true),
                    Item::Const(c) => (c.name.text.clone(), c.name.span.clone(), c.is_pub),
                    Item::Import(_) => unreachable!("imports stripped during load"),
                };
                // Loaded as the builtin it implements, the file's `pub fn` *is*
                // `__builtin_<name>`: every call to the builtin carries that
                // name, and a reference from inside the file — a recursive call,
                // or the file's own tests — resolves to it through this view.
                let canonical = match item {
                    Item::Fn(f) if self.builtin_files.contains(path) && f.is_pub => {
                        Callee::importable(&name).map(|c| c.name().to_string())
                    }
                    _ => None,
                };
                let mangled = canonical.unwrap_or_else(|| mangle(is_root, file.index, &name));
                if view.insert(name.clone(), mangled).is_some() {
                    let first = spans.get(&name).cloned();
                    let mut err = Error::at(
                        format!("{}: duplicate top-level item \"{name}\"", self.label(path)),
                        name_span,
                    );
                    if let Some(first) = first {
                        err = err.with_note(format!("\"{name}\" first defined here"), first);
                    }
                    return Err(self.attribute(path, root, err));
                }
                spans.insert(name.clone(), name_span);
                if importable {
                    exports.insert(name);
                }
            }
            // A variant's cases become importable constructors bound to the
            // variant-qualified global `Case@VariantGlobal`. The defining file
            // gets each case in its own view (so `A` works there directly);
            // other files reach it via `import { A }` or `V.A`. A case name that
            // isn't unique in the file (two variants share it, or it collides
            // with a fn/type) is left out of the bare view — reachable only as
            // `V.A` — so a bare use can never silently pick one.
            let mut case_target: HashMap<String, String> = HashMap::new();
            let mut ambiguous: HashSet<String> = HashSet::new();
            for item in &file.items {
                if let Item::Variant(v) = item {
                    // Both concrete and generic variants scope their constructors
                    // by import; for a generic variant the qualified `Case@Template`
                    // is later re-qualified to the chosen instance by the monomorphizer.
                    let vglobal = mangle(is_root, file.index, &v.name);
                    let cases: Vec<String> = v.cases.iter().map(|c| c.name.text.clone()).collect();
                    for c in &cases {
                        if view.contains_key(c) || case_target.contains_key(c) {
                            ambiguous.insert(c.clone());
                        } else {
                            case_target.insert(c.clone(), format!("{c}@{vglobal}"));
                        }
                    }
                    for c in &v.cases {
                        spans
                            .entry(c.name.text.clone())
                            .or_insert_with(|| c.name.span.clone());
                    }
                    ctor_cases.insert(vglobal, cases);
                }
            }
            for (case, qualified) in case_target {
                if ambiguous.contains(&case) {
                    continue;
                }
                view.insert(case.clone(), qualified);
                exports.insert(case);
            }
            local_views.insert(path, view);
            importables.insert(path, exports);
            local_spans.insert(path, spans);
        }

        // Pass 2: full view — extend each file's view with names it
        // explicitly imported. Validate that each imported name actually
        // exists, and that imports don't collide with locals or other
        // imports.
        let mut full_views: HashMap<&PathBuf, HashMap<String, String>> = local_views.clone();
        let mut import_sites: HashMap<PathBuf, Vec<aipl_syntax::ast::ImportSite>> = HashMap::new();
        for (path, file) in &self.files {
            // Need a mutable ref to this file's view; do the lookups first
            // to satisfy the borrow checker.
            // For each imported name, resolve the *exported* name in the source
            // file's view, but bind it locally under its alias (or the name
            // itself). `(local, span, mangled)`.
            let resolved: Vec<(String, Span, String)> = file
                .imports
                .iter()
                .flat_map(|imp| {
                    let from_view = local_views.get(&imp.from);
                    let from_exports = importables.get(&imp.from);
                    imp.names.iter().map(move |n| {
                        let Some(mangled) = from_view.and_then(|v| v.get(&n.name).cloned()) else {
                            return Err(Error::at(
                                format!(
                                    "\"{}\" is not exported by {}",
                                    n.name,
                                    file_label(&imp.from)
                                ),
                                n.span.clone(),
                            ));
                        };
                        // The name exists in the source file, but only `pub`
                        // functions (and types) may be imported.
                        if !from_exports.is_some_and(|s| s.contains(&n.name)) {
                            return Err(Error::at(
                                format!(
                                    "\"{}\" is private; mark it `pub` in {} to import it",
                                    n.name,
                                    file_label(&imp.from),
                                ),
                                n.span.clone(),
                            ));
                        }
                        Ok((n.local().to_string(), n.span.clone(), mangled))
                    })
                })
                .collect::<Result<_, _>>()
                .map_err(|e| self.attribute(path, root, e))?;

            let view = full_views.get_mut(path).expect("local view present");
            // Spans of names already brought in by an import (path or builtin),
            // so a later conflicting import can point back at the first one.
            // A collision with a local item points at `local_spans` instead.
            let mut import_spans: HashMap<String, Span> = HashMap::new();
            for (local, span, mangled) in resolved {
                let mangled_for_site = mangled.clone();
                if view.insert(local.clone(), mangled).is_some() {
                    return Err(self.attribute(
                        path,
                        root,
                        import_conflict(
                            self.label(path),
                            "import",
                            &local,
                            span,
                            import_spans.get(&local).cloned(),
                            local_spans[path].get(&local).cloned(),
                        ),
                    ));
                }
                import_sites
                    .entry(path.clone())
                    .or_default()
                    .push(aipl_syntax::ast::ImportSite {
                        span: span.clone(),
                        target: mangled_for_site.clone(),
                    });
                import_spans.insert(local, span);
            }

            // Builtin imports map the name to its reserved canonical form
            // (e.g. `len` -> `__builtin_len`), which codegen recognizes.
            // Validate each name is actually a builtin and doesn't clash
            // with a local item or another import.
            for n in &file.builtin_imports {
                let local = n.local();
                // Operator imports resolve into the view like any other name,
                // so the duplicate-import conflict check below covers them too
                // (including an operator imported both from builtins and as a
                // function — its view slot is already taken by the path import
                // resolved above). An operator aliased to a user function maps
                // to that function instead and is dispatched to a call.
                //
                // A *bare* operator spelling (`import { + }`) never reaches
                // here: the grammar refuses it during the parse, with the
                // aliases to write instead (`bare_operator_import` in
                // `grammar_aipl.aipl`), so `n.name` is always a name.
                let canonical = if let Some(builtin) =
                    aipl_syntax::builtin::Builtin::exporting_name(&n.name)
                {
                    // A type or case the builtin's source defines: the name it has
                    // in that file, which is loaded because this file imports it.
                    let source = builtin.source_path().and_then(|p| p.canonicalize().ok());
                    let found = source
                        .as_ref()
                        .and_then(|p| local_views.get(p))
                        .and_then(|v| v.get(&n.name));
                    match found {
                        Some(mangled) => mangled.clone(),
                        None => {
                            return Err(self.attribute(
                                path,
                                root,
                                Error::at(
                                    format!("\"{}\" is not exported by its builtin", n.name),
                                    n.span.clone(),
                                ),
                            ))
                        }
                    }
                } else if let Some((_op, canonical_impl)) = aipl_syntax::operator_builtin(&n.name) {
                    // A named operator builtin can be aliased to any operator
                    // name, though aliasing to a different operator than the one
                    // it provides is confusing and generally a bad idea. The view
                    // maps the operator spelling to the reserved `__builtin_*`
                    // impl, so a use of the operator resolves (in `rewrite_expr`)
                    // to a call to that impl — codegen intrinsifies it. Different
                    // builtins on the same operator (`wrapping_add`/`saturating_add`
                    // → `+`) thus dispatch to different impls, spelling-agnostically.
                    // Imported bare (`import { concat }`), the name binds to the
                    // same canonical as the aliased form, so it is callable as an
                    // ordinary function: `concat(a, b)`. That does *not* make the
                    // operator itself available — `a +++ b` still needs
                    // `concat as +++`, because gating looks up the operator's
                    // spelling in the view and a bare import adds only its name.
                    canonical_impl.to_string()
                } else if let Some(builtin) = Callee::importable(&n.name) {
                    // Reserved for the builtins' own implementations until
                    // it is decided whether programs get it.
                    if aipl_syntax::is_internal_builtin(&builtin)
                        && !self.builtin_files.contains(path)
                    {
                        return Err(self.attribute(
                            path,
                            root,
                            Error::at(
                                format!(
                                    "\"{}\" is internal to the builtins' implementation \
                                         and not available to programs",
                                    n.name
                                ),
                                n.span.clone(),
                            ),
                        ));
                    }
                    builtin.name().to_string()
                } else if let Some(canonical) = aipl_syntax::builtin_type_canonical(&n.name) {
                    canonical
                } else {
                    return Err(self.attribute(
                        path,
                        root,
                        Error::at(format!("\"{}\" is not a builtin", n.name), n.span.clone()),
                    ));
                };
                import_sites
                    .entry(path.clone())
                    .or_default()
                    .push(aipl_syntax::ast::ImportSite {
                        span: n.span.clone(),
                        target: canonical.clone(),
                    });
                if view.insert(local.to_string(), canonical).is_some() {
                    return Err(self.attribute(
                        path,
                        root,
                        import_conflict(
                            self.label(path),
                            "builtin import",
                            local,
                            n.span.clone(),
                            import_spans.get(local).cloned(),
                            local_spans[path].get(local).cloned(),
                        ),
                    ));
                }
                import_spans.insert(local.to_string(), n.span.clone());
            }
        }

        // Pass 3: gate operator usage against each file's view (every imported
        // operator — builtin or function-aliased — is a view key), then rewrite
        // item bodies using that view, collecting into one Program.
        let mut merged = Vec::new();
        for (path, file) in &self.files {
            let is_root = path == root;
            let view = &full_views[path];
            for item in &file.items {
                if let Item::Fn(f) = item {
                    check_operators(&f.body, view).map_err(|e| self.attribute(path, root, e))?;
                    if let Some(tb) = &f.test_body {
                        check_operators(tb, view).map_err(|e| self.attribute(path, root, e))?;
                    }
                    // Keyword-parameter defaults are expressions too — an
                    // operator used in one needs this file's import.
                    for p in &f.sig.params {
                        if let Some(d) = &p.default {
                            check_operators(d, view).map_err(|e| self.attribute(path, root, e))?;
                        }
                    }
                }
                let const_written = match item {
                    Item::Const(c) => {
                        check_const_name(c).map_err(|e| self.attribute(path, root, e))?;
                        Some(c.name.clone())
                    }
                    _ => None,
                };
                let rewritten = rewrite_item(item, view, &ctor_cases, is_root)
                    .map_err(|e| self.attribute(path, root, e))?;
                if let (Some(written), Item::Const(c)) = (&const_written, &rewritten) {
                    check_const_value(written, c).map_err(|e| self.attribute(path, root, e))?;
                }
                merged.push(rewritten);
            }
        }
        // Every reference is a final global name by now, so a constant's uses
        // are exactly the `Ident`s naming it — a local can never collide,
        // because a local keeps the name the author wrote and a constant has
        // been mangled. Substituting here rather than in any later pass is what
        // keeps constants out of the checker, mono and codegen entirely.
        let merged = substitute_constants(merged);
        // A named refinement is its target wherever it is written, so it is
        // expanded away here, for the same reason: nothing past the loader
        // should have to know a type can have a second name. Its diagnostics
        // name the declaration, which is attributed to the file that holds it.
        let mut expanded = Program {
            items: merged,
            sources: Vec::new(),
            doc: None,
        };
        let declared = aipl_syntax::expand_refinements(&mut expanded).and_then(|declared| {
            declared
                .iter()
                .try_for_each(|d| {
                    check_refinement_target(d, &ctor_cases).map_err(|e| (d.name.text.clone(), e))
                })
                .map(|()| declared)
        });
        if let Err((name, err)) = declared {
            let path = self
                .files
                .iter()
                .find(|(path, file)| {
                    let is_root = *path == root;
                    file.items.iter().any(|item| {
                        matches!(item, Item::Refinement(r)
                            if mangle(is_root, file.index, &r.name.text) == name)
                    })
                })
                .map(|(path, _)| path.clone());
            return Err(match path {
                Some(path) => self.attribute(&path, root, err),
                None => err,
            });
        }
        let merged = expanded.items;
        // Resolve keyword arguments (and fill omitted keyword parameters from
        // their defaults) now that every reference is a final global name —
        // after this, calls are fully positional and no `ExprKind::KwArg`
        // survives into the merged program.
        // The source map, positional by loader file index, so a downstream pass
        // can resolve a mangled `__m<N>__name` back to the file it came from.
        // Sized to the highest index actually assigned; a gap (there are none
        // today) reads as an empty entry, which attributes nothing rather than
        // attributing wrongly.
        let mut sources = vec![aipl_syntax::ast::FileSource::default(); self.next_index as usize];
        for (path, file) in &self.files {
            if let Some(slot) = sources.get_mut(file.index as usize) {
                *slot = aipl_syntax::ast::FileSource {
                    label: diagnostic_label(path, self.project_root.as_deref()),
                    source: file.source.clone(),
                    path: path.to_string_lossy().into_owned(),
                    imports: import_sites.get(path).cloned().unwrap_or_default(),
                };
            }
        }
        kwargs::expand_keyword_args(&Program {
            items: merged,
            sources,
            // A merged program is several files; no one file's `# ..` header
            // documents it, so it carries none.
            doc: None,
        })
    }
}

/// The builtins `items` call by their callee rather than by an imported name —
/// the ones the syntax itself introduces (`xs[r, c]` calls `grid_get`, `xs$f`
/// calls `map`). An AIPL-implemented one is loaded with the program; a native
/// one has nothing to load, but its presence is what the companion rules read,
/// so a `map` the program never imported still brings `map_join` when the
/// program joins its result.
fn syntax_builtins(items: &[Item]) -> Vec<Callee> {
    let mut out = Vec::new();
    aipl_syntax::each_item_expr(items, &mut |e| {
        if let ExprKind::Call(callee, ..) = &e.kind {
            if aipl_syntax::builtin::Builtin::of(callee).is_some() && !out.contains(callee) {
                out.push(callee.clone());
            }
        }
    });
    out
}

/// `item` with its `.test` block and the functions declared in it dropped, and
/// `None` for the function a `.performance` block stands as — what a file
/// keeps when it is loaded for a program that is not its own.
fn without_tests(item: Item) -> Option<Item> {
    match item {
        Item::Fn(f) if aipl_syntax::performance_source_name(&f.name).is_some() => None,
        Item::Fn(mut f) => {
            f.test_body = None;
            f.test_fns = Vec::new();
            Some(Item::Fn(f))
        }
        other => Some(other),
    }
}

/// The rules a `variant W = V without C` declaration is held to, on its target
/// once expanded: what it refines must be a variant, and each case it leaves out
/// one that variant has. The checker holds a *written* `V without C` to the same
/// rules, but it only sees the types that are used, and a declaration nothing
/// mentions would otherwise go unchecked.
fn check_refinement_target(
    decl: &aipl_syntax::ExpandedRefinement,
    ctor_cases: &HashMap<String, Vec<String>>,
) -> Result<(), Error> {
    let mut base = &decl.target;
    let mut excluded = Vec::new();
    while let Type::Without(inner, x) = base {
        excluded.push(x);
        base = inner;
    }
    let cases = match base {
        Type::Named(v) | Type::Generic(v, _) => ctor_cases.get(v),
        _ => None,
    };
    let Some(cases) = cases else {
        return Err(Error::at(
            format!(
                "{:?} refines {}, which is not a variant",
                decl.name.text,
                aipl_syntax::type_name(base)
            ),
            decl.name.span.clone(),
        ));
    };
    for x in excluded {
        match x {
            aipl_syntax::ast::Exclusion::NamedCase(case) if !cases.contains(case) => {
                return Err(Error::at(
                    format!(
                        "{:?} leaves out {case:?}, but {} has no such case",
                        decl.name.text,
                        aipl_syntax::type_name(base)
                    ),
                    decl.name.span.clone(),
                ));
            }
            aipl_syntax::ast::Exclusion::NamedCase(_) => {}
            aipl_syntax::ast::Exclusion::EmptyArray => {
                return Err(Error::at(
                    format!(
                        "{:?} refines {}, a variant, and `without []` excludes the empty array",
                        decl.name.text,
                        aipl_syntax::type_name(base)
                    ),
                    decl.name.span.clone(),
                ));
            }
        }
    }
    Ok(())
}

/// A top-level constant must be ALL_CAPS.
///
/// The rule keeps a use readable at the site that reads it: ALL_CAPS says "this
/// name is not a binding from around here".
fn check_const_name(c: &aipl_syntax::ast::ConstDecl) -> Result<(), Error> {
    if !is_screaming_snake_case(&c.name) {
        return Err(Error::at(
            format!(
                "a top-level constant is named in ALL_CAPS: write {:?} as {:?}",
                c.name,
                to_screaming_snake_case(&c.name)
            ),
            c.name.span.clone(),
        ));
    }
    Ok(())
}

/// The value half of [`check_const_name`].
///
/// Run on the *rewritten* declaration, so a callee in the value carries its
/// canonical name — which is what tells the `to_set` builtin apart from a user
/// function of the same spelling. `written` is the name the author gave the
/// constant, since the rewritten one is mangled.
///
/// A value that is not a literal is computed once, on first use, and the type
/// of what it computes is the one thing the loader cannot infer for it.
fn check_const_value(written: &str, c: &aipl_syntax::ast::ConstDecl) -> Result<(), Error> {
    if !is_const_literal(&c.value) && c.ty.is_none() {
        return Err(Error::at(
            format!(
                "a top-level constant that is given something to evaluate needs a type — \
                 write `let {written}: <type> = ...;`, since a constant computed on first use \
                 has no declaration to say what it returns"
            ),
            c.value.span.clone(),
        ));
    }
    Ok(())
}

/// At least one letter, and no lowercase: `MAX`, `S_KEYWORD`, `HTTP_2`.
fn is_screaming_snake_case(name: &str) -> bool {
    name.chars().any(|ch| ch.is_ascii_uppercase())
        && !name.chars().any(|ch| ch.is_ascii_lowercase())
}

/// `name` as it would be written in ALL_CAPS, for the diagnostic above.
fn to_screaming_snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 && !out.ends_with('_') {
            out.push('_');
        }
        out.push(ch.to_ascii_uppercase());
    }
    out
}

/// Whether `e` is a literal a constant may stand for.
/// Whether `callee` is the `to_set` builtin. The name has been rewritten to its
/// canonical by now, which a user function cannot hold — so this is the check
/// that a `to_set` in a constant is *the* one and not a same-named local.
fn is_to_set(callee: &Callee) -> bool {
    match callee {
        Callee::ToSet => true,
        Callee::User(n) => Callee::from_canonical(n) == Some(Callee::ToSet),
        _ => false,
    }
}

fn is_const_literal(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Unit => true,
        // `-1` is a literal with a sign on it.
        ExprKind::Neg(inner) => matches!(inner.kind, ExprKind::Num(_)),
        // `"…".to_set()` — a character class. Written as a call, but it names a
        // value rather than computing one: the receiver is a literal, there is
        // no second operand to order against it, and codegen folds it to the
        // four words of the `#{char}` bitfield, so each substituted use is four
        // immediate stores. That is what a literal has to mean here — something
        // that can be spliced with no evaluation order to argue about.
        //
        // Only a *literal* receiver qualifies. `xs.to_set()` over a binding is
        // a computation like any other, and is computed once on first use.
        ExprKind::Call(callee, args, _) if is_to_set(callee) => {
            matches!(args.as_slice(), [arg] if matches!(arg.kind, ExprKind::Str(_)))
        }
        _ => false,
    }
}

/// Replace every use of a top-level constant with what it stands for, and drop
/// the declarations.
///
/// A literal is spliced into each use. Anything else becomes a nullary function
/// (`constant_initializer_name`) and each use a call to it: codegen answers
/// that call from a cache, so the value is built once and every later use
/// shares it.
fn substitute_constants(items: Vec<Item>) -> Vec<Item> {
    let mut values: HashMap<String, Expr> = HashMap::new();
    let mut initializers: Vec<Item> = Vec::new();
    for item in &items {
        let Item::Const(c) = item else { continue };
        if is_const_literal(&c.value) {
            values.insert(c.name.text.clone(), c.value.clone());
            continue;
        }
        let name = aipl_syntax::constant_initializer_name(&c.name);
        values.insert(
            c.name.text.clone(),
            Expr::new(
                ExprKind::Call(Callee::User(name.clone()), Vec::new(), false),
                c.value.span.clone(),
            ),
        );
        initializers.push(Item::Fn(aipl_syntax::ast::Function {
            name: c.name.renamed(name),
            is_pub: false,
            sig: aipl_syntax::ast::Signature {
                type_vars: Vec::new(),
                params: Vec::new(),
                effects: Vec::new(),
                return_ty: c.ty.clone(),
            },
            body: c.value.clone(),
            test_body: None,
            test_fns: Vec::new(),
            doc: c.doc.clone(),
        }));
    }
    let mut items: Vec<Item> = items
        .into_iter()
        .filter(|i| !matches!(i, Item::Const(_)))
        .chain(initializers)
        .collect();
    if values.is_empty() {
        return items;
    }
    for item in &mut items {
        for e in item_exprs_mut(item) {
            splice_constants(e, &values);
        }
    }
    items
}

/// Every expression an item holds, mutably: a function's body, its test body
/// and its keyword-parameter defaults, and the field / payload defaults of a
/// struct or variant. A default is spliced into call sites, so a constant used
/// in one has to be substituted here like any other use.
fn item_exprs_mut(item: &mut Item) -> Vec<&mut Expr> {
    let mut out = Vec::new();
    match item {
        Item::Fn(f) => {
            out.extend(f.sig.params.iter_mut().filter_map(|p| p.default.as_mut()));
            out.push(&mut f.body);
            out.extend(f.test_body.as_mut());
        }
        Item::Struct(s) => out.extend(s.fields.iter_mut().filter_map(|f| f.default.as_mut())),
        Item::Variant(v) => out.extend(
            v.cases
                .iter_mut()
                .flat_map(|c| c.payload.iter_mut())
                .filter_map(|slot| slot.default.as_mut()),
        ),
        // Dropped just above, or expanded away; an import declares no
        // expression.
        Item::Const(_) | Item::Refinement(_) | Item::Import(_) => {}
    }
    out
}

/// `e` with each `Ident` naming a constant replaced by that constant's literal.
/// The literal keeps the *use site's* span, so a diagnostic about the value
/// points at where it was written rather than at the declaration.
fn splice_constants(e: &mut Expr, values: &HashMap<String, Expr>) {
    if let ExprKind::Ident(name) = &e.kind {
        if let Some(lit) = values.get(name.as_str()) {
            e.kind = lit.kind.clone();
            return;
        }
    }
    for kid in aipl_syntax::each_subexpr_mut(e) {
        splice_constants(kid, values);
    }
}

/// Reject any operator used in `e` whose spelling isn't imported by the file —
/// operators must be imported to be used. Every imported operator (a builtin
/// like `==` / `wrapping_add as +`, or a function aliased to an operator) is a
/// key in the file's `view`, so membership there is exactly "imported".
fn check_operators(e: &Expr, view: &HashMap<String, String>) -> Result<(), Error> {
    let require = |spelling: &str, span| -> Result<(), Error> {
        if view.contains_key(spelling) {
            Ok(())
        } else {
            // *No* operator has a bare spelling — each is a named builtin
            // aliased to the operator — so the hint is always `name as op`,
            // generated from `OPERATOR_BUILTINS` rather than restated here. It
            // used to name the flavored operators by hand and fall through to the
            // bare spelling for the rest, which advised `import { == }` — itself
            // a hard error. The first named form is the one suggested, which for
            // the pluggable-semantics operators is the wrapping flavor: what a
            // file that has not thought about overflow wants.
            let hint = match aipl_syntax::operator_named_forms(spelling).first() {
                Some(name) => format!("{name} as {spelling}"),
                None => spelling.to_string(),
            };
            Err(Error::at(
                format!("operator \"{spelling}\" must be imported: add `import {{ {hint} }} from builtins;`"),
                span,
            ))
        }
    };
    match &e.kind {
        // A shim's bindings are bare names; operators can only be in its body.
        ExprKind::Shim(_, _, body) => check_operators(body, view)?,
        // Unary `-` is the one operator with no named builtin behind it, so it
        // is still a node of its own and still gated here by hand. Every other
        // operator is a `Call` named for its spelling and gated in that arm.
        ExprKind::Neg(x) => {
            require("-", e.span.clone())?;
            check_operators(x, view)?;
        }
        ExprKind::Field(x, _)
        | ExprKind::Try(x)
        | ExprKind::Return(x)
        | ExprKind::KwArg(_, x, _)
        | ExprKind::TypeArgs(x, _)
        | ExprKind::Spread(x) => check_operators(x, view)?,
        // An `Assign` LHS is a place (idents/fields only), so it can't
        // contain an operator — only the value and body need walking.
        ExprKind::Seq(a, b)
        | ExprKind::Index(a, b)
        | ExprKind::Let(_, _, a, b)
        | ExprKind::LetMut(_, _, a, b)
        | ExprKind::Assign(_, a, b)
        | ExprKind::For(_, _, a, b)
        | ExprKind::While(a, b) => {
            check_operators(a, view)?;
            check_operators(b, view)?;
        }
        ExprKind::If(a, b, c) => {
            check_operators(a, view)?;
            check_operators(b, view)?;
            check_operators(c, view)?;
        }
        ExprKind::Slice(a, b, c) => {
            check_operators(a, view)?;
            check_operators(b, view)?;
            if let Some(c) = c {
                check_operators(c, view)?;
            }
        }
        ExprKind::Call(callee, args, _) => {
            // Before resolution every source-spelled callee is a `User`; the
            // compiler's own arms (`some`, the template intrinsics) name no
            // operator and take no import.
            let name = callee.user().unwrap_or_default();
            // An operator use is a call named for the spelling written, so the
            // gate is one membership test on the callee. This replaces a `Binop`
            // arm and a `Not` arm that asked the same question of their own node
            // kinds — the operator having no node of its own is exactly what
            // collapses the three into one.
            if aipl_syntax::is_operator_name(name) {
                require(name, e.span.clone())?;
            }
            // A bare-imported operator builtin is called like any function, but
            // its arity is the operator's — and no signature covers it (these are
            // intrinsified rather than emitted), so a miscount has to be rejected
            // here rather than surfacing as a call to a function that does not
            // exist.
            if let Some(arity) = view
                .get(name)
                .and_then(|t| Callee::from_canonical(t))
                .and_then(|c| aipl_syntax::operator_arity(&c))
            {
                if args.len() != arity {
                    return Err(Error::at(
                        format!(
                            "{name:?} takes {arity} argument{}, got {}",
                            if arity == 1 { "" } else { "s" },
                            args.len()
                        ),
                        e.span.clone(),
                    ));
                }
            }
            for a in args {
                check_operators(a, view)?;
            }
        }
        ExprKind::ArrayLit(args) | ExprKind::SetLit(args, _) | ExprKind::TupleLit(args) => {
            for a in args {
                check_operators(a, view)?;
            }
        }
        ExprKind::DictLit(pairs) => {
            for (k, v) in pairs {
                check_operators(k, view)?;
                check_operators(v, view)?;
            }
        }
        ExprKind::Construct(_, inits) => {
            for i in inits {
                check_operators(&i.value, view)?;
            }
        }
        ExprKind::Match(s, arms) => {
            check_operators(s, view)?;
            for a in arms {
                check_operators(&a.body, view)?;
            }
        }
        ExprKind::IfLet(arm, s, else_b) => {
            check_operators(s, view)?;
            check_operators(&arm.body, view)?;
            check_operators(else_b, view)?;
        }
        ExprKind::Lambda(_, body) => check_operators(body, view)?,
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit
        | ExprKind::Ident(_) => {}
    }
    Ok(())
}

fn mangle(is_root: bool, file_index: u32, name: &str) -> String {
    if is_root {
        name.to_string()
    } else {
        format!("__m{file_index}__{name}")
    }
}

/// The inverse of [`mangle`]'s non-root case: strips a `__m<digits>__` prefix
/// from a compiled item's name, recovering the plain name it was declared with
/// in its own file — the identity function for a root item (already unmangled)
/// or any other string that doesn't match the pattern. Lets a caller holding a
/// flattened [`Program`]'s item names (functions and structs are named exactly
/// like this — see the module doc) look an item up by the name its source
/// used, regardless of which file — root or not — declared it.
pub fn unmangled_name(name: &str) -> &str {
    let Some(rest) = name.strip_prefix("__m") else {
        return name;
    };
    match rest.find("__") {
        Some(i) if i > 0 && rest.as_bytes()[..i].iter().all(u8::is_ascii_digit) => &rest[i + 2..],
        _ => name,
    }
}

/// Rewrite one item's global references through `view`. `is_root` says whether
/// `item` comes from the file being loaded directly, as opposed to a
/// transitively imported one: a non-root function's `.test({ .. })` body is
/// dropped rather than carried into the merged [`Program`], so `aipl check`'s
/// test driver (`build_test_program`) only ever runs the tests that belong to
/// the file it was pointed at, not every test reachable through its imports.
/// Resolve a match pattern's constructor name to its variant-qualified global —
/// the pattern analog of the construction rewriting in [`rewrite_expr`]. A bare
/// `A` resolves through the file's `view` (an imported or same-file case); a
/// `V.A` path resolves `V` to its variant and qualifies its case. Builtin
/// constructors (`some`/`ok`/`err`/`none`) and non-constructor patterns are left
/// as-is; an out-of-scope constructor is left bare for the checker to reject.
fn resolve_pattern(pat: &Pattern, view: &HashMap<String, String>, sc: &Scope) -> Pattern {
    let Pattern::Ctor {
        name,
        bindings,
        ignore_payload,
    } = pat
    else {
        return pat.clone();
    };
    if matches!(name.as_str(), "some" | "ok" | "err" | "none") {
        return pat.clone();
    }
    let resolved = if let Some((v, a)) = name.split_once('.') {
        view.get(v)
            .filter(|g| {
                sc.ctor_cases
                    .get(*g)
                    .is_some_and(|cs| cs.iter().any(|c| c == a))
            })
            .map(|g| format!("{a}@{g}"))
    } else {
        // A view entry that is a constructor is `Case@Variant` (only ctors carry
        // `@`); a same-named function/type resolves to a plain mangled name and
        // is not a valid pattern here, so leave it bare for the checker.
        view.get(name).filter(|t| t.contains('@')).cloned()
    };
    Pattern::Ctor {
        name: resolved.unwrap_or_else(|| name.clone()),
        bindings: bindings.clone(),
        ignore_payload: *ignore_payload,
    }
}

fn rewrite_item(
    item: &Item,
    view: &HashMap<String, String>,
    ctor_cases: &HashMap<String, Vec<String>>,
    is_root: bool,
) -> Result<Item, Error> {
    let sc = &Scope { ctor_cases };
    Ok(match item {
        Item::Fn(f) => Item::Fn(Function {
            name: f.name.renamed(
                view.get(&*f.name)
                    .cloned()
                    .unwrap_or_else(|| f.name.text.clone()),
            ),
            is_pub: f.is_pub,
            sig: Signature {
                // Generic type-var names are local type variables, not global
                // items — leave them untouched when rewriting signature types.
                type_vars: f.sig.type_vars.clone(),
                params: {
                    // A keyword parameter's default is spliced into *call
                    // sites* (possibly in other files), so its global
                    // references must resolve through the declaring file's
                    // view now. The parameters declared *before* it are its
                    // locals: a default may name one (`fn f(a: T, b: T = a)`),
                    // and such a name refers to the parameter even when a
                    // global of the same name is in view — the call site binds
                    // it to that argument. Parameters from this one on are
                    // deliberately *not* locals, so a forward or self
                    // reference stays an ordinary name for
                    // `kwargs::FnKwInfo::from_sig` to reject.
                    let mut earlier: HashSet<String> = HashSet::new();
                    let mut params = Vec::with_capacity(f.sig.params.len());
                    for p in &f.sig.params {
                        params.push(Param {
                            name_span: p.name_span.clone(),
                            name: p.name.clone(),
                            ty: rewrite_type(&p.ty, view, &f.sig.type_vars),
                            mutable: p.mutable,
                            arity: p.arity,
                            default: p
                                .default
                                .as_ref()
                                .map(|d| rewrite_expr(d, view, sc, &earlier)),
                            implicit_some: p.implicit_some,
                        });
                        earlier.insert(p.name.clone());
                    }
                    params
                },
                effects: f.sig.effects.clone(),
                return_ty: f
                    .sig
                    .return_ty
                    .as_ref()
                    .map(|t| rewrite_type(t, view, &f.sig.type_vars)),
            },
            // Parameters are locals — they shadow any global of the same name,
            // so an ident referring to a parameter is never rewritten.
            body: rewrite_expr(
                &f.body,
                view,
                sc,
                &f.sig.params.iter().map(|p| p.name.clone()).collect(),
            ),
            // Rewrite global references inside the `.test({ .. })` body too (it
            // can call the function under test and other globals). Parameters
            // aren't in scope in a test body — only globals — so no locals.
            // Dropped entirely for a non-root item — see this function's doc.
            test_body: if is_root {
                f.test_body
                    .as_ref()
                    .map(|tb| rewrite_expr(tb, view, sc, &std::collections::HashSet::new()))
            } else {
                None
            },
            test_fns: Vec::new(),
            // Documentation is plain text — no global references to rewrite.
            doc: f.doc.clone(),
        }),
        Item::Struct(s) => Item::Struct(StructDecl {
            name: s.name.renamed(
                view.get(&*s.name)
                    .cloned()
                    .unwrap_or_else(|| s.name.text.clone()),
            ),
            // Documentation is plain text — no global references to rewrite.
            doc: s.doc.clone(),
            type_vars: s.type_vars.clone(),
            // Field types may reference the struct's own type variables (`T`);
            // pass them so they're left un-rewritten.
            fields: s
                .fields
                .iter()
                .map(|fd| aipl_syntax::ast::FieldDecl {
                    name: fd.name.clone(),
                    ty: rewrite_type(&fd.ty, view, &s.type_vars),
                    default: fd.default.clone(),
                })
                .collect(),
        }),
        Item::Variant(v) => Item::Variant(aipl_syntax::ast::VariantDecl {
            name: v.name.renamed(
                view.get(&*v.name)
                    .cloned()
                    .unwrap_or_else(|| v.name.text.clone()),
            ),
            doc: v.doc.clone(),
            type_vars: v.type_vars.clone(),
            cases: v
                .cases
                .iter()
                .map(|c| aipl_syntax::ast::VariantCase {
                    name: c.name.clone(),
                    doc: c.doc.clone(),
                    payload: {
                        // A payload default is rewritten exactly as a keyword
                        // parameter's is: the *named* slots declared before it
                        // are its locals (a default may read one, and the
                        // construction site binds it to the argument passed
                        // there), so a global of the same name doesn't capture
                        // it. Slots from this one on are deliberately left out,
                        // so a forward or self reference stays an ordinary name
                        // for `kwargs::FnKwInfo::from_params` to reject.
                        let mut earlier: HashSet<String> = HashSet::new();
                        let mut slots = Vec::with_capacity(c.payload.len());
                        for slot in &c.payload {
                            slots.push(aipl_syntax::ast::CaseParam {
                                name: slot.name.clone(),
                                ty: rewrite_type(&slot.ty, view, &v.type_vars),
                                default: slot
                                    .default
                                    .as_ref()
                                    .map(|d| rewrite_expr(d, view, sc, &earlier)),
                                implicit_some: slot.implicit_some,
                                doc: slot.doc.clone(),
                            });
                            earlier.extend(slot.name.clone());
                        }
                        slots
                    },
                })
                .collect(),
        }),
        // A refinement's own name becomes the global it stands for and its
        // target may name imported types, exactly as a variant's payloads do.
        // The case it excludes is a bare case name, which is never mangled.
        Item::Refinement(r) => Item::Refinement(aipl_syntax::ast::RefinementDecl {
            name: r.name.renamed(
                view.get(&*r.name)
                    .cloned()
                    .unwrap_or_else(|| r.name.text.clone()),
            ),
            doc: r.doc.clone(),
            type_vars: r.type_vars.clone(),
            target: rewrite_type(&r.target, view, &r.type_vars),
        }),
        // A constant's own name becomes the global it stands for, and its
        // annotation may name an imported type. Its *value* is rewritten too:
        // `"…".to_set()` is admitted as a constant, so a value can name a
        // builtin, and the name has to resolve here — substitution happens
        // after this pass, so anything left unresolved would reach the use site
        // as an unknown function.
        Item::Const(c) => Item::Const(aipl_syntax::ast::ConstDecl {
            name: c.name.renamed(
                view.get(&*c.name)
                    .cloned()
                    .unwrap_or_else(|| c.name.text.clone()),
            ),
            ty: c.ty.as_ref().map(|t| rewrite_type(t, view, &[])),
            value: rewrite_expr(&c.value, view, sc, &std::collections::HashSet::new()),
            ..c.clone()
        }),
        Item::Import(_) => unreachable!("imports stripped during load"),
    })
}

fn rewrite_type(t: &Type, view: &HashMap<String, String>, type_vars: &[TypeParam]) -> Type {
    match t {
        // Primitives, Unit, and the compiler pseudo-types carry no name to
        // resolve — pass through unchanged.
        Type::Unit => Type::Unit,
        Type::Primitive(p) => Type::Primitive(*p),
        Type::Any => Type::Any,
        Type::NoneInner => Type::NoneInner,
        Type::EmptyArrayArg => Type::EmptyArrayArg,
        Type::NoneLiteralArg => Type::NoneLiteralArg,
        Type::Unknown => Type::Unknown,
        Type::ConcatStr => Type::ConcatStr,
        // `Case<V>` names `V`, which resolves through the view like any other
        // type name — importing the variant is what puts its cases in scope.
        Type::Case(v) => Type::Case(Box::new(rewrite_type(v, view, type_vars))),
        // Already promoted by `promote_type_vars` at the end of parsing: a type
        // parameter is bound by its own declaration, so there is no name to
        // resolve through the file's view.
        Type::TypeVar(v) => Type::TypeVar(v.clone()),
        Type::Named(s) => {
            if is_builtin_type(s) {
                // Builtin type (`Error`) → keep as-is.
                Type::Named(s.clone())
            } else {
                // User-defined struct/variant name → look up the mangled name
                // through the file's view.
                Type::Named(view.get(s).cloned().unwrap_or_else(|| s.clone()))
            }
        }
        Type::Optional(inner) => Type::Optional(Box::new(rewrite_type(inner, view, type_vars))),
        Type::Array(inner) => Type::Array(Box::new(rewrite_type(inner, view, type_vars))),
        Type::Without(base, x) => {
            Type::Without(Box::new(rewrite_type(base, view, type_vars)), x.clone())
        }
        Type::Set(inner, o) => Type::Set(Box::new(rewrite_type(inner, view, type_vars)), *o),
        Type::Dict(k, v) => Type::Dict(
            Box::new(rewrite_type(k, view, type_vars)),
            Box::new(rewrite_type(v, view, type_vars)),
        ),
        Type::Result(ok, err) => Type::Result(
            Box::new(rewrite_type(ok, view, type_vars)),
            Box::new(rewrite_type(err, view, type_vars)),
        ),
        Type::Fn(params, ret) => Type::Fn(
            params
                .iter()
                .map(|p| rewrite_type(p, view, type_vars))
                .collect(),
            Box::new(rewrite_type(ret, view, type_vars)),
        ),
        Type::Tuple(elems) => Type::Tuple(
            elems
                .iter()
                .map(|e| rewrite_type(e, view, type_vars))
                .collect(),
        ),
        // `Foo<A, B>` — rewrite the template's base name through the view (it's
        // a user struct/variant), and each type argument recursively.
        Type::Generic(name, args) => {
            let base = if type_vars.iter().any(|v| v.name == *name) {
                name.clone()
            } else {
                view.get(name).cloned().unwrap_or_else(|| name.clone())
            };
            Type::Generic(
                base,
                args.iter()
                    .map(|a| rewrite_type(a, view, type_vars))
                    .collect(),
            )
        }
    }
}

/// Whether a [`Type::Named`] spelling is an ambient builtin that must not be
/// rewritten through a file's import view. Primitives are now [`Type::Primitive`]
/// (never reach here), so the only such name is the builtin `Error` type.
fn is_builtin_type(s: &str) -> bool {
    s == aipl_syntax::ERROR
}

/// Rewrite an expression's global references through the file's `view`.
///
/// `locals` is the set of names bound in the enclosing scope (function
/// parameters, `let`/`for`/lambda/`match` bindings). A bare [`ExprKind::Ident`]
/// is rewritten to its global mangled name only when it is *not* shadowed by a
/// local — this is what lets a function (or imported builtin) be referenced by
/// name as a value (e.g. `map(xs, double)`), while a local variable of the same
/// name still resolves to the local. Names introduced by a binder are added to
/// `locals` for that binder's sub-scope only.
/// Constructor-resolution context for [`rewrite_expr`], alongside the file's
/// `view`: `ctor_cases` maps a variant's global name to its case names, for
/// resolving `V.A` paths.
struct Scope<'a> {
    ctor_cases: &'a HashMap<String, Vec<String>>,
}

fn rewrite_expr(
    e: &Expr,
    view: &HashMap<String, String>,
    sc: &Scope,
    locals: &HashSet<String>,
) -> Expr {
    // Extend `locals` with a freshly-bound name for a nested scope.
    let with = |name: &str| -> HashSet<String> {
        let mut s = locals.clone();
        s.insert(name.to_string());
        s
    };
    // A loop binds its element and, in the two-binder form, its index; both
    // scope over the body.
    let with_loop = |name: &str, index: &Option<String>| -> HashSet<String> {
        let mut s = with(name);
        if let Some(i) = index {
            s.insert(i.clone());
        }
        s
    };
    let kind = match &e.kind {
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Unit
        | ExprKind::None => e.kind.clone(),
        // A shim's bound functions are global references like any other, so
        // they resolve through the view (a local can't shadow them: a shim
        // binds a *function*, and locals never name one here).
        ExprKind::Shim(effect, bindings, body) => ExprKind::Shim(
            effect.clone(),
            bindings
                .iter()
                .map(|(op, f)| {
                    let resolved = view.get(f).cloned().unwrap_or_else(|| f.clone());
                    (op.clone(), resolved)
                })
                .collect(),
            Box::new(rewrite_expr(body, view, sc, locals)),
        ),
        // A bare ident: resolve it to a global only if it isn't a local. This is
        // how a function or imported builtin used as a value (`map(xs, f)`)
        // reaches its mangled/canonical name; a shadowing local is left alone.
        ExprKind::Ident(name) => match view.get(name) {
            Some(mangled) if !locals.contains(name) => ExprKind::Ident(mangled.clone()),
            _ => e.kind.clone(),
        },
        // A callee the compiler already knows (`some`, a template intrinsic)
        // has no name to resolve; only its arguments are rewritten.
        ExprKind::Call(callee, args, method_style) if callee.user().is_none() => ExprKind::Call(
            callee.clone(),
            args.iter()
                .map(|a| rewrite_expr(a, view, sc, locals))
                .collect(),
            *method_style,
        ),
        ExprKind::Call(callee, args, method_style) => {
            let Callee::User(name) = callee else {
                unreachable!("a known callee takes the arm above")
            };
            // `V.A(args)` — a variant-qualified constructor call. It parses as a
            // method call whose receiver is the variant *type* name `V`; detect a
            // non-local `V` the view resolves to a variant with a case `name`, and
            // rewrite to the qualified constructor `A@Vglobal` (dropping the type
            // receiver). Anything else is an ordinary call / method call.
            let qualified = if *method_style {
                args.first().and_then(|first| match &first.kind {
                    ExprKind::Ident(recv) if !locals.contains(recv) => view
                        .get(recv)
                        .filter(|g| sc.ctor_cases.get(*g).is_some_and(|cs| cs.contains(name)))
                        .cloned(),
                    _ => None,
                })
            } else {
                None
            };
            match qualified {
                Some(vglobal) => ExprKind::Call(
                    Callee::User(format!("{name}@{vglobal}")),
                    args[1..]
                        .iter()
                        .map(|a| rewrite_expr(a, view, sc, locals))
                        .collect(),
                    false,
                ),
                // A call through a local holding a function value. Every global
                // reaches here spelled differently from its source — mangled, or
                // an imported builtin's canonical name — except the root file's
                // own functions, which keep theirs. A local of that spelling is
                // the one such a call means (the checker has always held it to
                // being a function), so it is the local that is called, and it is
                // recorded as one: no global can then be taken for it, nor it for
                // a global.
                None if locals.contains(name) && view.get(name).is_none_or(|g| g == name) => {
                    ExprKind::Call(
                        Callee::Local(name.clone()),
                        args.iter()
                            .map(|a| rewrite_expr(a, view, sc, locals))
                            .collect(),
                        *method_style,
                    )
                }
                // A bare-imported operator builtin (`import { concat }` then
                // `concat(a, b)`) needs nothing special: its canonical is a real
                // `__builtin_*`, the same one the operator spelling resolves to,
                // so both spellings land on this one line and are the same
                // program. (They used to diverge — the canonical was a marker, so
                // the call had to be lowered back into the primitive node the
                // operator produced.)
                None => ExprKind::Call(
                    Callee::resolve(view.get(name).cloned().unwrap_or_else(|| name.clone())),
                    args.iter()
                        .map(|a| rewrite_expr(a, view, sc, locals))
                        .collect(),
                    *method_style,
                ),
            }
        }
        ExprKind::Construct(name, fields) => ExprKind::Construct(
            view.get(name).cloned().unwrap_or_else(|| name.clone()),
            fields
                .iter()
                .map(|fi| FieldInit {
                    name: fi.name.clone(),
                    value: rewrite_expr(&fi.value, view, sc, locals),
                })
                .collect(),
        ),
        ExprKind::Field(obj, field) => {
            // `V.A` — a nullary variant-qualified constructor used as a value
            // (`V.Empty`), the no-argument analog of the `V.A(args)` call above.
            let qualified = match &obj.kind {
                ExprKind::Ident(recv) if !locals.contains(recv) => view
                    .get(recv)
                    .filter(|g| sc.ctor_cases.get(*g).is_some_and(|cs| cs.contains(field)))
                    .cloned(),
                _ => None,
            };
            match qualified {
                Some(vglobal) => ExprKind::Ident(format!("{field}@{vglobal}")),
                None => {
                    ExprKind::Field(Box::new(rewrite_expr(obj, view, sc, locals)), field.clone())
                }
            }
        }
        ExprKind::Match(scrutinee, arms) => {
            // The arm bindings (e.g. `v` in `some(v)`, `w`/`h` in `Rect(w, h)`)
            // are locals in that arm. The constructor name is a pattern keyword,
            // not a global reference, so it isn't rewritten.
            let new_arms = arms
                .iter()
                .map(|arm| {
                    let mut arm_locals = locals.clone();
                    for b in arm.pattern.bindings() {
                        arm_locals.insert(b.clone());
                    }
                    MatchArm {
                        pattern: resolve_pattern(&arm.pattern, view, sc),
                        body: rewrite_expr(&arm.body, view, sc, &arm_locals),
                        span: arm.span.clone(),
                    }
                })
                .collect::<Vec<MatchArm>>();
            // `A | B => body` is one arm per alternative, each with a copy of the
            // body, and a copy gets binders of its own.
            let mut new_arms = new_arms;
            for i in 1..new_arms.len() {
                if new_arms[..i].iter().any(|a| a.body == new_arms[i].body) {
                    new_arms[i].body = aipl_syntax::distinct_binders(&new_arms[i].body);
                }
            }
            ExprKind::Match(
                Box::new(rewrite_expr(scrutinee, view, sc, locals)),
                new_arms,
            )
        }
        // The pattern's bindings are locals for `arm.body` (the `then` branch)
        // only — not `scrutinee`, and not `else_b` — exactly the scoping one
        // `Match` arm's bindings get.
        ExprKind::IfLet(arm, scrutinee, else_b) => {
            let mut then_locals = locals.clone();
            for b in arm.pattern.bindings() {
                then_locals.insert(b.clone());
            }
            let new_arm = MatchArm {
                pattern: resolve_pattern(&arm.pattern, view, sc),
                body: rewrite_expr(&arm.body, view, sc, &then_locals),
                span: arm.span.clone(),
            };
            ExprKind::IfLet(
                Box::new(new_arm),
                Box::new(rewrite_expr(scrutinee, view, sc, locals)),
                Box::new(rewrite_expr(else_b, view, sc, locals)),
            )
        }
        ExprKind::Neg(inner) => ExprKind::Neg(Box::new(rewrite_expr(inner, view, sc, locals))),
        ExprKind::If(cond, then_b, else_b) => ExprKind::If(
            Box::new(rewrite_expr(cond, view, sc, locals)),
            Box::new(rewrite_expr(then_b, view, sc, locals)),
            Box::new(rewrite_expr(else_b, view, sc, locals)),
        ),
        // `name` binds a local for `body` (but not for `value`).
        // A `let x: T = ..` annotation names a type in *this* file's view, so it
        // is rewritten like a lambda parameter's annotation (no type vars in
        // expression position).
        ExprKind::Let(name, ty, value, body) => ExprKind::Let(
            name.clone(),
            ty.as_ref().map(|t| rewrite_type(t, view, &[])),
            Box::new(rewrite_expr(value, view, sc, locals)),
            Box::new(rewrite_expr(body, view, sc, &with(name))),
        ),
        ExprKind::LetMut(name, ty, value, body) => ExprKind::LetMut(
            name.clone(),
            ty.as_ref().map(|t| rewrite_type(t, view, &[])),
            Box::new(rewrite_expr(value, view, sc, locals)),
            Box::new(rewrite_expr(body, view, sc, &with(name))),
        ),
        // `set lhs = value; body` — the LHS is rooted at a binding from an
        // enclosing `let mut` (always a local, never an imported alias), so it
        // needs no rewriting; the name stays a local in both `value` and `body`.
        ExprKind::Assign(lhs, value, body) => ExprKind::Assign(
            lhs.clone(),
            Box::new(rewrite_expr(value, view, sc, locals)),
            Box::new(rewrite_expr(body, view, sc, locals)),
        ),
        ExprKind::For(var, index, iterable, body) => ExprKind::For(
            var.clone(),
            index.clone(),
            Box::new(rewrite_expr(iterable, view, sc, locals)),
            Box::new(rewrite_expr(body, view, sc, &with_loop(var, index))),
        ),
        // `while` binds nothing; both condition and body see the same scope.
        ExprKind::While(cond, body) => ExprKind::While(
            Box::new(rewrite_expr(cond, view, sc, locals)),
            Box::new(rewrite_expr(body, view, sc, locals)),
        ),
        ExprKind::ArrayLit(elems) => ExprKind::ArrayLit(
            elems
                .iter()
                .map(|e| rewrite_expr(e, view, sc, locals))
                .collect(),
        ),
        ExprKind::SetLit(elems, o) => ExprKind::SetLit(
            elems
                .iter()
                .map(|e| rewrite_expr(e, view, sc, locals))
                .collect(),
            *o,
        ),
        ExprKind::TupleLit(elems) => ExprKind::TupleLit(
            elems
                .iter()
                .map(|e| rewrite_expr(e, view, sc, locals))
                .collect(),
        ),
        ExprKind::DictLit(pairs) => ExprKind::DictLit(
            pairs
                .iter()
                .map(|(k, v)| {
                    (
                        rewrite_expr(k, view, sc, locals),
                        rewrite_expr(v, view, sc, locals),
                    )
                })
                .collect(),
        ),
        ExprKind::Index(obj, index) => ExprKind::Index(
            Box::new(rewrite_expr(obj, view, sc, locals)),
            Box::new(rewrite_expr(index, view, sc, locals)),
        ),
        ExprKind::Slice(obj, start, end) => ExprKind::Slice(
            Box::new(rewrite_expr(obj, view, sc, locals)),
            Box::new(rewrite_expr(start, view, sc, locals)),
            end.as_ref()
                .map(|e| Box::new(rewrite_expr(e, view, sc, locals))),
        ),
        ExprKind::Try(e) => ExprKind::Try(Box::new(rewrite_expr(e, view, sc, locals))),
        ExprKind::TypeArgs(e, tys) => ExprKind::TypeArgs(
            Box::new(rewrite_expr(e, view, sc, locals)),
            tys.iter().map(|t| rewrite_type(t, view, &[])).collect(),
        ),
        ExprKind::Spread(e) => ExprKind::Spread(Box::new(rewrite_expr(e, view, sc, locals))),
        // The keyword name refers to the callee's parameter, not a global —
        // only the value is rewritten.
        ExprKind::KwArg(name, value, fwd) => ExprKind::KwArg(
            name.clone(),
            Box::new(rewrite_expr(value, view, sc, locals)),
            *fwd,
        ),
        ExprKind::Seq(first, rest) => ExprKind::Seq(
            Box::new(rewrite_expr(first, view, sc, locals)),
            Box::new(rewrite_expr(rest, view, sc, locals)),
        ),
        ExprKind::Return(value) => {
            ExprKind::Return(Box::new(rewrite_expr(value, view, sc, locals)))
        }
        // Lambda params are locals in the body; their optional type annotations
        // may reference structs, so rewrite those through the view.
        ExprKind::Lambda(params, body) => {
            let mut inner = locals.clone();
            for p in params {
                inner.insert(p.name.clone());
            }
            ExprKind::Lambda(
                params
                    .iter()
                    .map(|p| LambdaParam {
                        name: p.name.clone(),
                        ty: p.ty.as_ref().map(|t| rewrite_type(t, view, &[])),
                        span: p.span.clone(),
                    })
                    .collect(),
                Box::new(rewrite_expr(body, view, sc, &inner)),
            )
        }
    };
    Expr {
        kind,
        span: e.span.clone(),
        ty: None,
        // Carried over with `span`: the parser recorded it, and rewriting a
        // name or qualifying a path moves no text.
        value_span: e.value_span.clone(),
        name_span: e.name_span.clone(),
    }
}

/// Tag every error with the source its spans index, so the renderer points the
/// caret into the right file. Diagnostics raised while loading an *imported*
/// file index that file, while the caller renders against whatever source it
/// holds — normally the entry file's — which is how a caret ends up on an
/// unrelated line of the wrong file.
///
/// The entry file itself is deliberately left untagged: the caller already
/// renders those against its source, under the path the user actually typed,
/// and tagging would replace that with a bare basename. Errors arriving from
/// deeper imports are already tagged, and `Error::in_file` keeps the innermost
/// attribution.
fn tag_origin(
    is_entry: bool,
    path: &Path,
    project_root: Option<&Path>,
    src: &str,
    errs: Vec<Error>,
) -> Vec<Error> {
    if is_entry {
        return errs;
    }
    let label = diagnostic_label(path, project_root);
    errs.into_iter().map(|e| e.in_file(&label, src)).collect()
}

/// What a diagnostic that points into `path` calls the file: the path relative
/// to the project root as `./sub/file.aipl`, the way the entry file is named
/// under a root-relative invocation, so an error in an imported file says which
/// file of that name. A virtual source's key is already such a name; a file
/// outside the root keeps its full path, since nothing shorter identifies it.
fn diagnostic_label(path: &Path, project_root: Option<&Path>) -> String {
    if path.is_relative() {
        return path.display().to_string();
    }
    match project_root.and_then(|root| path.strip_prefix(root).ok()) {
        Some(rel) => format!("./{}", bake::slash_joined(rel)),
        None => path.display().to_string(),
    }
}

/// Just the file's name, for the places a message mentions a file in passing
/// (`"x" is not exported by lib.aipl`) rather than pointing a caret into it.
pub(crate) fn file_label(path: &Path) -> String {
    path.file_name()
        .expect("File should have file name")
        .to_str()
        .expect("Filename was invalid unicode")
        .to_string()
}

/// Build the "import conflicts" error. When the colliding name was a previous
/// *import* (`prior` carries its span), point at both — the new import as the
/// primary caret and the earlier one as a `note:`. A collision with a local
/// item points the primary caret at the import and a `note:` at the item's
/// name (`local_item`). `kind` is the import flavor for the message (`"import"`
/// or `"builtin import"`); `label` names the file both spans index.
fn import_conflict(
    label: String,
    kind: &str,
    local: &str,
    span: Span,
    prior: Option<Span>,
    local_item: Option<Span>,
) -> Error {
    match prior {
        // Point the primary caret at the *later* (duplicate) import and the
        // `note:` at the *earlier* one, regardless of which the loader happened
        // to resolve first — so the message reads in source order (rustc-style).
        Some(p) => {
            let (first, dup) = if p.start <= span.start {
                (p, span)
            } else {
                (span, p)
            };
            Error::at(
                format!("{label}: {kind} \"{local}\" conflicts with another import"),
                dup,
            )
            .with_note(format!("\"{local}\" first imported here"), first)
        }
        None => {
            let error = Error::at(
                format!("{label}: {kind} \"{local}\" conflicts with a local item"),
                span,
            );
            match local_item {
                Some(item) => error.with_note(format!("\"{local}\" defined here"), item),
                None => error,
            }
        }
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, Error> {
    path.canonicalize()
        .map_err(|e| Error::msg(format!("resolve {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::unmangled_name;

    #[test]
    fn strips_a_mangled_prefix() {
        assert_eq!(unmangled_name("__m3__assert_loc"), "assert_loc");
        assert_eq!(unmangled_name("__m0__x"), "x");
        assert_eq!(unmangled_name("__m12__count_while"), "count_while");
    }

    #[test]
    fn leaves_an_unmangled_name_alone() {
        assert_eq!(unmangled_name("assert_loc"), "assert_loc");
        assert_eq!(unmangled_name(""), "");
    }

    #[test]
    fn leaves_lookalikes_alone() {
        // No digit run between "__m" and the next "__".
        assert_eq!(unmangled_name("__m__x"), "__m__x");
        // No second "__" at all.
        assert_eq!(unmangled_name("__mfoo"), "__mfoo");
        // A non-digit inside what should be the index.
        assert_eq!(unmangled_name("__m3a__x"), "__m3a__x");
    }
}

/// The virtual file an import `from` written in `importer` names: `from`
/// resolved against the importer's directory, with `.` and `..` folded out, so
/// `../expr.aipl` in `./lint/lint.aipl` is `./expr.aipl`.
fn virtual_import_path(importer: &std::path::Path, from: &str) -> PathBuf {
    use std::path::Component;
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    let dir = importer.parent().unwrap_or(std::path::Path::new("."));
    for component in dir
        .components()
        .chain(std::path::Path::new(from).components())
    {
        match component {
            Component::Normal(part) => parts.push(part.to_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    let mut path = PathBuf::from(".");
    path.extend(parts);
    path
}
