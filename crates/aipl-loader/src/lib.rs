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

mod kwargs;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use aipl_parser::parse;
use aipl_syntax::ast::{
    Expr, ExprKind, FieldInit, Function, ImportDecl, ImportName, ImportSource, Item, LambdaParam,
    MatchArm, Param, Program, Signature, StructDecl, Type, TypeParam,
};
use aipl_syntax::{builtin_canonical, DebugOptions, Error, Span};

/// Parse `root`, recursively resolve every `import`, and return a single
/// merged [`Program`] ready for codegen.
pub fn load_program(root: &Path, dbg: DebugOptions) -> Result<Program, Error> {
    let root_canon = canonicalize(root)?;
    let mut loader = Loader {
        dbg,
        ..Loader::default()
    };
    loader.load(&root_canon)?;
    dbg.trace(
        "loader",
        format_args!("flattening {} file(s)", loader.files.len()),
    );
    loader.flatten(&root_canon)
}

/// Like [`load_program`] but with the root file's source supplied in memory
/// (used by the embedding FFI). Any `from "..."` path imports resolve relative
/// to the current directory; `from builtins` works as usual.
pub fn load_program_str(source: &str, dbg: DebugOptions) -> Result<Program, Error> {
    let mut loader = Loader {
        dbg,
        ..Loader::default()
    };
    // A synthetic root under the current dir: it need not exist on disk, but
    // gives relative path imports a base directory and a stable map key.
    let root = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("__aipl_ffi_root__.aipl");
    loader.load_source(&root, source)?;
    loader.flatten(&root)
}

/// Like [`load_program`] but over a set of in-memory virtual files, supplied as
/// `(name, source)` pairs (typically each `include_str!`'d into the host
/// binary, so nothing is read from disk at runtime). The **first** pair is the
/// root/entry — its functions keep their original names, ready to be called.
/// `from "..."` path imports resolve *by name* against the provided set (a
/// leading `./` is stripped, so `from "./util.aipl"` matches a `"util.aipl"`
/// entry); `from builtins` works as usual.
pub fn load_program_sources(sources: &[(&str, &str)], dbg: DebugOptions) -> Result<Program, Error> {
    let Some((root_name, _)) = sources.first() else {
        return Err(Error::msg("load_program_sources: no sources provided"));
    };
    let root = PathBuf::from(root_name);
    let mut loader = Loader {
        dbg,
        ..Loader::default()
    };
    for (name, src) in sources {
        loader.register_virtual(PathBuf::from(name), src).unwrap();
    }
    loader.check_virtual_imports()?;
    loader.flatten(&root)
}

/// Reject importing from the same place on more than one line: every import from
/// a given source (`builtins`, or one path) must be a single `import { .. }`
/// statement. Errors at the redundant import, so its names can be merged into the
/// first. Run per file, before resolving paths.
fn check_no_duplicate_import_sources(program: &Program) -> Result<(), Error> {
    let mut seen_builtins = false;
    let mut seen_paths: HashSet<String> = HashSet::new();
    for item in &program.items {
        let Item::Import(ImportDecl { source, .. }) = item else {
            continue;
        };
        match source {
            ImportSource::Builtins { span } => {
                if seen_builtins {
                    return Err(Error::at(
                        "duplicate import from \"builtins\"; merge the names into the first \
                         `import { .. } from builtins;`",
                        span.clone(),
                    ));
                }
                seen_builtins = true;
            }
            ImportSource::Path { path, span } => {
                if !seen_paths.insert(path.clone()) {
                    return Err(Error::at(
                        format!(
                            "duplicate import from {path:?}; merge the names into the first \
                             `import {{ .. }} from {path:?};`"
                        ),
                        span.clone(),
                    ));
                }
            }
        }
    }
    Ok(())
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
}

struct LoadedFile {
    index: u32,
    /// Functions + structs from this file (imports stripped out).
    items: Vec<Item>,
    /// `import { ... } from "./other"` declarations. `from` resolved to
    /// a canonical path.
    imports: Vec<ResolvedImport>,
    /// `import { ... } from builtins;` — the builtin names this file
    /// brings into scope (each possibly aliased).
    builtin_imports: Vec<ImportName>,
}

struct ResolvedImport {
    names: Vec<ImportName>,
    from: PathBuf,
}

impl Loader {
    fn load(&mut self, path: &Path) -> Result<(), Error> {
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
    fn load_source(&mut self, path: &Path, src: &str) -> Result<(), Error> {
        if self.files.contains_key(path) {
            return Ok(());
        }
        // Keep the original Error (including span) so `render()` can show
        // the caret pointing into the imported file's source. The path
        // tied to that source is implicit from the source line itself.
        let program = parse(src)?;
        check_no_duplicate_import_sources(&program)?;

        let mut items = Vec::new();
        let mut imports = Vec::new();
        let mut builtin_imports = Vec::new();
        for item in program.items {
            match item {
                Item::Import(ImportDecl { names, source }) => match source {
                    ImportSource::Path { path: from, span } => {
                        let parent = path.parent().unwrap_or_else(|| Path::new("."));
                        let candidate = parent.join(&from);
                        let canon = canonicalize(&candidate).map_err(|e| {
                            Error::at(format!("import {from:?}: {}", e.message), span)
                        })?;
                        imports.push(ResolvedImport { names, from: canon });
                    }
                    ImportSource::Builtins { .. } => {
                        builtin_imports.extend(names);
                    }
                },
                other => items.push(other),
            }
        }

        // Reserve the slot *before* recursing so cycles short-circuit at
        // `files.contains_key` above.
        let index = self.next_index;
        self.next_index += 1;
        let import_paths: Vec<PathBuf> = imports.iter().map(|i| i.from.clone()).collect();
        self.files.insert(
            path.to_path_buf(),
            LoadedFile {
                index,
                items,
                imports,
                builtin_imports,
            },
        );
        for from in import_paths {
            self.load(&from)?;
        }
        Ok(())
    }

    /// Register an in-memory virtual file `key` (no disk access, no recursion —
    /// the FFI provides the whole set up front). Path imports resolve to other
    /// virtual files by normalized name; [`check_virtual_imports`] then verifies
    /// each target was actually supplied.
    ///
    /// [`check_virtual_imports`]: Loader::check_virtual_imports
    fn register_virtual(&mut self, key: PathBuf, src: &str) -> Result<(), Error> {
        let program = parse(src)?;
        if !key.starts_with("./") {
            panic!("non-relative path: {key:?}");
        }
        check_no_duplicate_import_sources(&program)?;
        let mut items = Vec::new();
        let mut imports = Vec::new();
        let mut builtin_imports = Vec::new();
        for item in program.items {
            match item {
                Item::Import(ImportDecl { names, source }) => match source {
                    ImportSource::Path { path: from, .. } => imports.push(ResolvedImport {
                        names,
                        from: PathBuf::from(&from),
                    }),
                    ImportSource::Builtins { .. } => builtin_imports.extend(names),
                },
                other => items.push(other),
            }
        }
        let index = self.next_index;
        self.next_index += 1;
        if self
            .files
            .insert(
                key.clone(),
                LoadedFile {
                    index,
                    items,
                    imports,
                    builtin_imports,
                },
            )
            .is_some()
        {
            return Err(Error::msg(format!(
                "duplicate source {:?} in compile_sources",
                file_label(&key)
            )));
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

    fn flatten(&self, root: &Path) -> Result<Program, Error> {
        // Pass 1: local view — names defined in the file map to their
        // (mangled or, for root, original) global names. Also record which
        // names another file may *import*: `pub` functions and all types
        // (structs/variants). A non-`pub` function is file-private.
        let mut local_views: HashMap<&PathBuf, HashMap<String, String>> = HashMap::new();
        let mut importables: HashMap<&PathBuf, HashSet<String>> = HashMap::new();
        for (path, file) in &self.files {
            let is_root = path == root;
            let mut view = HashMap::new();
            let mut exports = HashSet::new();
            for item in &file.items {
                let (name, importable) = match item {
                    Item::Fn(f) => (f.name.clone(), f.is_pub),
                    Item::Struct(s) => (s.name.clone(), true),
                    Item::Variant(v) => (v.name.clone(), true),
                    Item::Import(_) => unreachable!("imports stripped during load"),
                };
                let mangled = mangle(is_root, file.index, &name);
                if view.insert(name.clone(), mangled).is_some() {
                    return Err(Error::msg(format!(
                        "{}: duplicate top-level item \"{name}\"",
                        file_label(path),
                    )));
                }
                if importable {
                    exports.insert(name);
                }
            }
            local_views.insert(path, view);
            importables.insert(path, exports);
        }

        // Pass 2: full view — extend each file's view with names it
        // explicitly imported. Validate that each imported name actually
        // exists, and that imports don't collide with locals or other
        // imports.
        let mut full_views: HashMap<&PathBuf, HashMap<String, String>> = local_views.clone();
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
                .collect::<Result<_, _>>()?;

            let view = full_views.get_mut(path).expect("local view present");
            // Spans of names already brought in by an import (path or builtin),
            // so a later conflicting import can point back at the first one.
            // Local items aren't tracked here (they carry no name span), so a
            // collision with one falls back to the single-span form.
            let mut import_spans: HashMap<String, Span> = HashMap::new();
            for (local, span, mangled) in resolved {
                if view.insert(local.clone(), mangled).is_some() {
                    return Err(import_conflict(
                        file_label(path),
                        "import",
                        &local,
                        span,
                        import_spans.get(&local).cloned(),
                    ));
                }
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
                // resolved above). A builtin operator maps its spelling to
                // itself: there's no callable canonical, just a marker that
                // `rewrite_expr` keeps as a primitive `Binop`. An operator
                // aliased to a user function maps to that function instead and
                // is dispatched to a call.
                let canonical = if let Some((_op, canonical_impl)) =
                    aipl_syntax::operator_builtin(&n.name)
                {
                    // A named operator builtin can be aliased to any operator
                    // name, though aliasing to a different operator than the one
                    // it provides is confusing and generally a bad idea. The view
                    // maps the operator spelling to the reserved `__builtin_*`
                    // impl, so a use of the operator resolves (in `rewrite_expr`)
                    // to a call to that impl — codegen intrinsifies it. Different
                    // builtins on the same operator (`wrapping_add`/`saturating_add`
                    // → `+`) thus dispatch to different impls, spelling-agnostically.
                    if n.alias.is_none() {
                        return Err(Error::at(
                            format!(
                                "\"{}\" is an operator builtin; it must be aliased to an operator (e.g., `{} as +`)",
                                n.name, n.name
                            ),
                            n.span.clone(),
                        ));
                    }
                    canonical_impl.to_string()
                } else if aipl_syntax::is_operator_name(&n.name) {
                    // Operators with pluggable semantics (`+`, `-`) have no bare
                    // form — you must pick a flavor and alias it. Everything else
                    // (`==`, `*`, `+++`, …) is a bare builtin operator.
                    if let Some((verb, wrapping, saturating)) = match n.name.as_str() {
                        "+" => Some(("add", "wrapping_add", "saturating_add")),
                        "-" => Some(("subtract", "wrapping_sub", "saturating_sub")),
                        _ => None,
                    } {
                        return Err(Error::at(
                            format!(
                                "the \"{}\" operator has no bare form; pick a semantics and \
                                 import it aliased, e.g. `{wrapping} as {}` or `{saturating} as {}` \
                                 from builtins ({verb})",
                                n.name, n.name, n.name
                            ),
                            n.span.clone(),
                        ));
                    }
                    n.name.clone()
                } else if let Some(canonical) = builtin_canonical(&n.name) {
                    canonical
                } else if let Some(canonical) = aipl_syntax::builtin_type_canonical(&n.name) {
                    canonical
                } else {
                    return Err(Error::at(
                        format!("\"{}\" is not a builtin", n.name),
                        n.span.clone(),
                    ));
                };
                if view.insert(local.to_string(), canonical).is_some() {
                    return Err(import_conflict(
                        file_label(path),
                        "builtin import",
                        local,
                        n.span.clone(),
                        import_spans.get(local).cloned(),
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
                    check_operators(&f.body, view)?;
                    if let Some(tb) = &f.test_body {
                        check_operators(tb, view)?;
                    }
                    // Keyword-parameter defaults are expressions too — an
                    // operator used in one needs this file's import.
                    for p in &f.sig.params {
                        if let Some(d) = &p.default {
                            check_operators(d, view)?;
                        }
                    }
                }
                merged.push(rewrite_item(item, view, is_root)?);
            }
        }
        // Resolve keyword arguments (and fill omitted keyword parameters from
        // their defaults) now that every reference is a final global name —
        // after this, calls are fully positional and no `ExprKind::KwArg`
        // survives into the merged program.
        kwargs::expand_keyword_args(&Program { items: merged })
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
            // The `+` operator has no bare spelling — it's the `wrapping_add`
            // builtin aliased to `+`.
            let hint = if spelling == "+" {
                "wrapping_add as +".to_string()
            } else {
                spelling.to_string()
            };
            Err(Error::at(
                format!("operator \"{spelling}\" must be imported: add `import {{ {hint} }} from builtins;`"),
                span,
            ))
        }
    };
    match &e.kind {
        ExprKind::Binop(a, op, b) => {
            require(aipl_syntax::binop_spelling(*op), e.span.clone())?;
            check_operators(a, view)?;
            check_operators(b, view)?;
        }
        ExprKind::Neg(x) => {
            require("-", e.span.clone())?;
            check_operators(x, view)?;
        }
        ExprKind::Not(x) => {
            require("!", e.span.clone())?;
            check_operators(x, view)?;
        }
        ExprKind::Field(x, _) | ExprKind::Try(x) | ExprKind::Return(x) | ExprKind::KwArg(_, x) => {
            check_operators(x, view)?
        }
        // An `Assign` LHS is a place (idents/fields only), so it can't
        // contain an operator — only the value and body need walking.
        ExprKind::Seq(a, b)
        | ExprKind::Index(a, b)
        | ExprKind::Let(_, a, b)
        | ExprKind::LetMut(_, a, b)
        | ExprKind::Assign(_, a, b)
        | ExprKind::For(_, a, b)
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
        ExprKind::Call(_, args, _)
        | ExprKind::ArrayLit(args)
        | ExprKind::SetLit(args)
        | ExprKind::TupleLit(args) => {
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
fn rewrite_item(item: &Item, view: &HashMap<String, String>, is_root: bool) -> Result<Item, Error> {
    Ok(match item {
        Item::Fn(f) => Item::Fn(Function {
            name: view.get(&f.name).cloned().unwrap_or_else(|| f.name.clone()),
            is_pub: f.is_pub,
            sig: Signature {
                // Generic type-var names are local type variables, not global
                // items — leave them untouched when rewriting signature types.
                type_vars: f.sig.type_vars.clone(),
                params: f
                    .sig
                    .params
                    .iter()
                    .map(|p| Param {
                        name: p.name.clone(),
                        ty: rewrite_type(&p.ty, view, &f.sig.type_vars),
                        mutable: p.mutable,
                        variadic: p.variadic,
                        // A keyword parameter's default is spliced into *call
                        // sites* (possibly in other files), so its global
                        // references must resolve through the declaring file's
                        // view now. Defaults can't reference the function's
                        // parameters (they're checked in an empty environment),
                        // so no locals shadow anything here.
                        default: p
                            .default
                            .as_ref()
                            .map(|d| rewrite_expr(d, view, &HashSet::new())),
                    })
                    .collect(),
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
                &f.sig.params.iter().map(|p| p.name.clone()).collect(),
            ),
            // Rewrite global references inside the `.test({ .. })` body too (it
            // can call the function under test and other globals). Parameters
            // aren't in scope in a test body — only globals — so no locals.
            // Dropped entirely for a non-root item — see this function's doc.
            test_body: if is_root {
                f.test_body
                    .as_ref()
                    .map(|tb| rewrite_expr(tb, view, &std::collections::HashSet::new()))
            } else {
                None
            },
            // Documentation is plain text — no global references to rewrite.
            doc: f.doc.clone(),
        }),
        Item::Struct(s) => Item::Struct(StructDecl {
            name: view.get(&s.name).cloned().unwrap_or_else(|| s.name.clone()),
            fields: s
                .fields
                .iter()
                .map(|fd| aipl_syntax::ast::FieldDecl {
                    name: fd.name.clone(),
                    ty: rewrite_type(&fd.ty, view, &[]),
                    default: fd.default.clone(),
                })
                .collect(),
        }),
        Item::Variant(v) => Item::Variant(aipl_syntax::ast::VariantDecl {
            name: view.get(&v.name).cloned().unwrap_or_else(|| v.name.clone()),
            cases: v
                .cases
                .iter()
                .map(|c| aipl_syntax::ast::VariantCase {
                    name: c.name.clone(),
                    payload: c
                        .payload
                        .iter()
                        .map(|t| rewrite_type(t, view, &[]))
                        .collect(),
                })
                .collect(),
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
        Type::ConcatStr => Type::ConcatStr,
        Type::Named(s) => {
            if is_builtin_type(s) || type_vars.iter().any(|v| v.name == *s) {
                // Builtin type (`Error`) or a local generic type variable →
                // keep as-is.
                Type::Named(s.clone())
            } else {
                // User-defined struct/variant name → look up the mangled name
                // through the file's view.
                Type::Named(view.get(s).cloned().unwrap_or_else(|| s.clone()))
            }
        }
        Type::Optional(inner) => Type::Optional(Box::new(rewrite_type(inner, view, type_vars))),
        Type::Array(inner) => Type::Array(Box::new(rewrite_type(inner, view, type_vars))),
        Type::Set(inner) => Type::Set(Box::new(rewrite_type(inner, view, type_vars))),
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
fn rewrite_expr(e: &Expr, view: &HashMap<String, String>, locals: &HashSet<String>) -> Expr {
    // Extend `locals` with a freshly-bound name for a nested scope.
    let with = |name: &str| -> HashSet<String> {
        let mut s = locals.clone();
        s.insert(name.to_string());
        s
    };
    let kind = match &e.kind {
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Unit
        | ExprKind::None => e.kind.clone(),
        // A bare ident: resolve it to a global only if it isn't a local. This is
        // how a function or imported builtin used as a value (`map(xs, f)`)
        // reaches its mangled/canonical name; a shadowing local is left alone.
        ExprKind::Ident(name) => match view.get(name) {
            Some(mangled) if !locals.contains(name) => ExprKind::Ident(mangled.clone()),
            _ => e.kind.clone(),
        },
        ExprKind::Call(name, args, method_style) => ExprKind::Call(
            view.get(name).cloned().unwrap_or_else(|| name.clone()),
            args.iter().map(|a| rewrite_expr(a, view, locals)).collect(),
            *method_style,
        ),
        ExprKind::Construct(name, fields) => ExprKind::Construct(
            view.get(name).cloned().unwrap_or_else(|| name.clone()),
            fields
                .iter()
                .map(|fi| FieldInit {
                    name: fi.name.clone(),
                    value: rewrite_expr(&fi.value, view, locals),
                })
                .collect(),
        ),
        ExprKind::Field(obj, field) => {
            ExprKind::Field(Box::new(rewrite_expr(obj, view, locals)), field.clone())
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
                        pattern: arm.pattern.clone(),
                        body: rewrite_expr(&arm.body, view, &arm_locals),
                        span: arm.span.clone(),
                    }
                })
                .collect();
            ExprKind::Match(Box::new(rewrite_expr(scrutinee, view, locals)), new_arms)
        }
        ExprKind::Neg(inner) => ExprKind::Neg(Box::new(rewrite_expr(inner, view, locals))),
        ExprKind::Not(inner) => ExprKind::Not(Box::new(rewrite_expr(inner, view, locals))),
        ExprKind::Binop(l, op, r) => {
            let lhs = rewrite_expr(l, view, locals);
            let rhs = rewrite_expr(r, view, locals);
            // A binary operator imported as one of the file's own functions
            // (`import { my_add as + } from "./x"`) desugars to a call to that
            // function — the view maps the operator spelling to its
            // already-mangled name. A builtin operator (bare ops,
            // `wrapping_add as +`) maps the spelling to itself, so it stays a
            // primitive Binop.
            let spelling = aipl_syntax::binop_spelling(*op);
            match view.get(spelling) {
                Some(target) if target != spelling => {
                    ExprKind::Call(target.clone(), vec![lhs, rhs], false)
                }
                // The increment operator `++` (`'P'`) is gated on its own
                // spelling but is just a `wrapping_add`; collapse it to `+` now
                // (gating already ran) so codegen/mono never see the
                // increment-only char.
                _ => {
                    let op = if *op == 'P' { '+' } else { *op };
                    ExprKind::Binop(Box::new(lhs), op, Box::new(rhs))
                }
            }
        }
        ExprKind::If(cond, then_b, else_b) => ExprKind::If(
            Box::new(rewrite_expr(cond, view, locals)),
            Box::new(rewrite_expr(then_b, view, locals)),
            Box::new(rewrite_expr(else_b, view, locals)),
        ),
        // `name` binds a local for `body` (but not for `value`).
        ExprKind::Let(name, value, body) => ExprKind::Let(
            name.clone(),
            Box::new(rewrite_expr(value, view, locals)),
            Box::new(rewrite_expr(body, view, &with(name))),
        ),
        ExprKind::LetMut(name, value, body) => ExprKind::LetMut(
            name.clone(),
            Box::new(rewrite_expr(value, view, locals)),
            Box::new(rewrite_expr(body, view, &with(name))),
        ),
        // `set lhs = value; body` — the LHS is rooted at a binding from an
        // enclosing `let mut` (always a local, never an imported alias), so it
        // needs no rewriting; the name stays a local in both `value` and `body`.
        ExprKind::Assign(lhs, value, body) => ExprKind::Assign(
            lhs.clone(),
            Box::new(rewrite_expr(value, view, locals)),
            Box::new(rewrite_expr(body, view, locals)),
        ),
        ExprKind::For(var, iterable, body) => ExprKind::For(
            var.clone(),
            Box::new(rewrite_expr(iterable, view, locals)),
            Box::new(rewrite_expr(body, view, &with(var))),
        ),
        // `while` binds nothing; both condition and body see the same scope.
        ExprKind::While(cond, body) => ExprKind::While(
            Box::new(rewrite_expr(cond, view, locals)),
            Box::new(rewrite_expr(body, view, locals)),
        ),
        ExprKind::ArrayLit(elems) => ExprKind::ArrayLit(
            elems
                .iter()
                .map(|e| rewrite_expr(e, view, locals))
                .collect(),
        ),
        ExprKind::SetLit(elems) => ExprKind::SetLit(
            elems
                .iter()
                .map(|e| rewrite_expr(e, view, locals))
                .collect(),
        ),
        ExprKind::TupleLit(elems) => ExprKind::TupleLit(
            elems
                .iter()
                .map(|e| rewrite_expr(e, view, locals))
                .collect(),
        ),
        ExprKind::DictLit(pairs) => ExprKind::DictLit(
            pairs
                .iter()
                .map(|(k, v)| (rewrite_expr(k, view, locals), rewrite_expr(v, view, locals)))
                .collect(),
        ),
        ExprKind::Index(obj, index) => ExprKind::Index(
            Box::new(rewrite_expr(obj, view, locals)),
            Box::new(rewrite_expr(index, view, locals)),
        ),
        ExprKind::Slice(obj, start, end) => ExprKind::Slice(
            Box::new(rewrite_expr(obj, view, locals)),
            Box::new(rewrite_expr(start, view, locals)),
            end.as_ref()
                .map(|e| Box::new(rewrite_expr(e, view, locals))),
        ),
        ExprKind::Try(e) => ExprKind::Try(Box::new(rewrite_expr(e, view, locals))),
        // The keyword name refers to the callee's parameter, not a global —
        // only the value is rewritten.
        ExprKind::KwArg(name, value) => {
            ExprKind::KwArg(name.clone(), Box::new(rewrite_expr(value, view, locals)))
        }
        ExprKind::Seq(first, rest) => ExprKind::Seq(
            Box::new(rewrite_expr(first, view, locals)),
            Box::new(rewrite_expr(rest, view, locals)),
        ),
        ExprKind::Return(value) => ExprKind::Return(Box::new(rewrite_expr(value, view, locals))),
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
                Box::new(rewrite_expr(body, view, &inner)),
            )
        }
    };
    Expr {
        kind,
        span: e.span.clone(),
    }
}

// Used to make errors better by eliminating asbolute paths. TODO: Instead, always
// use paths relative to the project root i.e. ./file_name.aipl
fn file_label(path: &Path) -> String {
    path.file_name()
        .expect("File should have file name")
        .to_str()
        .expect("Filename was invalid unicode")
        .to_string()
}

/// Build the "import conflicts" error. When the colliding name was a previous
/// *import* (`prior` carries its span), point at both — the new import as the
/// primary caret and the earlier one as a `note:`. A collision with a local
/// item (which has no recorded span) gets the single-span form. `kind` is the
/// import flavor for the message (`"import"` or `"builtin import"`).
fn import_conflict(
    label: String,
    kind: &str,
    local: &str,
    span: Span,
    prior: Option<Span>,
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
        None => Error::at(
            format!("{label}: {kind} \"{local}\" conflicts with a local item"),
            span,
        ),
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
