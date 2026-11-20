//! Everything known about each importable builtin, in one place: which
//! callee it is, and where its signature and documentation come from.
//!
//! A builtin is one of three kinds, and a [`Definition`] says which:
//!
//! - **Written in AIPL.** Its source is an ordinary AIPL file (see
//!   [`crate::aipl_builtins`]), which holds the `pub fn` and the `# ..` block
//!   that are its signature and documentation. A tool reads them from there.
//! - **Declared.** The compiler expands or generates it, and the checker types
//!   calls to it by a declaration in [`crate::BUILTIN_SIGNATURES`] named
//!   `__builtin_<name>`. That declaration and the `# ..` block above it are its
//!   signature and documentation.
//! - **Described.** Typed by a rule in the checker with no declaration to read
//!   (the operators, and the reductions whose signature is derived from their
//!   step's), so the signature and prose are written here instead. The
//!   signature uses AIPL's declaration syntax so it reads like the others, but
//!   it is a description, not a declaration the compiler checks.
//!
//! The name is not a field: a builtin's canonical name is the one
//! [`Callee::name`] returns, and its import spelling is [`Builtin::name`]
//! (that less the `__builtin_` prefix), so a third copy would only have a way
//! to disagree.
//!
//! The table is the dogfooded `builtin_table.aipl`, read once through the FFI (see [`set_table_hook`]); it is the whole importable set, in the order the arms of
//! [`Callee`] list them. The names the language reserves without an import and
//! the intrinsics passes synthesize are not here; they are [`Callee`]'s own.

use std::path::PathBuf;

use crate::ast::Callee;
use crate::callee::SeqShape;

/// Where a builtin's signature and documentation are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Definition {
    /// Written in AIPL, in this file of `aipl-mono/src/`.
    Aipl { file: &'static str },
    /// Declared as `__builtin_<name>` in [`crate::BUILTIN_SIGNATURES`].
    Declared,
    /// Neither written nor declared; described here.
    Described {
        /// The signature on one line, as a declaration:
        /// `fn len<T: any>(self: T[]) -> u64`.
        signature: &'static str,
        /// What the builtin does, as the prose a `# ..` block would hold.
        doc: &'static str,
    },
}

/// One row of the builtins table as the dogfooded `builtin_table.aipl` holds it: the same facts as a
/// [`Builtin`], with the callee as its canonical name and owned text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    /// The name of the `Callee` case the AIPL side knows it as (`Len`).
    pub arm: String,
    pub callee: String,
    pub definition: RowDefinition,
    pub exports: Vec<String>,
    pub library: String,
    pub public_name: Option<String>,
}

/// [`Definition`] with owned text, for a [`TableRow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowDefinition {
    Written { file: String },
    Declared,
    Described { signature: String, doc: String },
}

/// One importable builtin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Builtin {
    /// The arm of [`Callee`] a call to it carries. A shape-variadic builtin is
    /// listed in its undecided ([`SeqShape::Seq`]) form.
    pub callee: Callee,
    pub definition: Definition,
    /// The other names its AIPL source makes importable: a type it defines and
    /// the cases of a variant among them. Importing any of them loads the
    /// source, as importing the builtin does.
    pub exports: &'static [&'static str],
    /// The library an import takes it from: `import { len } from builtins;` is the
    /// [`MAIN_LIBRARY`], `import { parse } from json;` another. A library is only a
    /// way to group what is imported; a program sees the same builtin whichever
    /// library holds it.
    pub library: &'static str,
    /// The name its library spells it with, when that is not the callee's own: a
    /// library is the namespace, so `parse_json` is `parse` in `json`.
    pub public_name: Option<&'static str>,
}

/// The hook that reads the builtins table, installed by the compiler with [`set_table_hook`]: the
/// dogfooded `builtin_table`, run through the embedding FFI. No native fallback.
static TABLE_HOOK: std::sync::OnceLock<fn() -> Vec<TableRow>> = std::sync::OnceLock::new();

/// Install the builtins table hook. Idempotent: the first install wins.
pub fn set_table_hook(hook: fn() -> Vec<TableRow>) {
    let _ = TABLE_HOOK.set(hook);
}

/// The library `import { .. } from builtins;` names: everything not grouped into a library of its
/// own.
pub const MAIN_LIBRARY: &str = "builtins";

/// The library of the set operations: `import { has, union } from sets;`.
pub const SETS_LIBRARY: &str = "sets";

/// The library of the network operations: `import { http_get, HttpResponse } from network;`.
pub const NETWORK_LIBRARY: &str = "network";

/// The directory the AIPL sources live in, `aipl-mono/src/`: resolved at
/// compile time but *read at run time*. `include_str!` would make every edit to
/// one of these files a source change to this crate, rebuilding it, everything
/// downstream and every test binary — paid on a plain edit, and again on each
/// one the test harness's `fill_expected` refreshes.
const SOURCE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../aipl-mono/src");

impl Builtin {
    /// Every importable builtin.
    pub fn all() -> &'static [Builtin] {
        static TABLE: std::sync::OnceLock<Vec<Builtin>> = std::sync::OnceLock::new();
        TABLE.get_or_init(|| {
            let hook = TABLE_HOOK.get().expect(
                "builtins table hook not installed before reading the builtins \
                 (call install_parser_hooks first)",
            );
            hook().into_iter().map(Builtin::from_row).collect()
        })
    }

    /// `row` as a builtin. The table is read once and lives for the process, so its text is leaked
    /// to be `'static`.
    fn from_row(row: TableRow) -> Builtin {
        fn leak(text: String) -> &'static str {
            Box::leak(text.into_boxed_str())
        }
        Builtin {
            callee: Callee::from_canonical(&row.callee).unwrap_or_else(|| {
                panic!(
                    "the builtins table lists `{}`, which `Callee` has no arm for",
                    row.callee
                )
            }),
            definition: match row.definition {
                RowDefinition::Written { file } => Definition::Aipl { file: leak(file) },
                RowDefinition::Declared => Definition::Declared,
                RowDefinition::Described { signature, doc } => Definition::Described {
                    signature: leak(signature),
                    doc: leak(doc),
                },
            },
            exports: Box::leak(
                row.exports
                    .into_iter()
                    .map(leak)
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
            library: leak(row.library),
            public_name: row.public_name.map(leak),
        }
    }

    /// The builtin `callee` is, or `None` for anything that is not importable
    /// (a user function, `some`, an intrinsic). A shape-variadic builtin is
    /// found whatever shape mono decided for it.
    pub fn of(callee: &Callee) -> Option<&'static Builtin> {
        let undecided = callee.with_seq_shape(SeqShape::Seq);
        Builtin::all().iter().find(|b| b.callee == undecided)
    }

    /// The builtin an import list spells `name` — the bare `len`, not
    /// `__builtin_len` — or `None`.
    pub fn named(name: &str) -> Option<&'static Builtin> {
        Builtin::named_in(MAIN_LIBRARY, name)
    }

    /// The builtin `library` spells `name`, or `None`.
    pub fn named_in(library: &str, name: &str) -> Option<&'static Builtin> {
        Builtin::all()
            .iter()
            .find(|b| b.library == library && b.name() == name)
    }

    /// The builtin whose source exports `name` — a type or a case — or `None`.
    pub fn exporting_name(name: &str) -> Option<&'static Builtin> {
        Builtin::exporting_name_in(MAIN_LIBRARY, name)
    }

    /// The builtin of `library` whose source exports `name`, or `None`.
    pub fn exporting_name_in(library: &str, name: &str) -> Option<&'static Builtin> {
        Builtin::all()
            .iter()
            .find(|b| b.library == library && b.exports.contains(&name))
    }

    /// The library that holds `name` as a builtin or an export, if any does: where to send
    /// someone who imported it from the wrong one.
    pub fn library_of(name: &str) -> Option<&'static str> {
        Builtin::all()
            .iter()
            .find(|b| b.name() == name || b.exports.contains(&name))
            .map(|b| b.library)
    }

    /// The import that brings the builtin spelled `name` into scope — `import { len } from
    /// builtins;` — or `None` when no builtin has that name in any library.
    pub fn import_statement(name: &str) -> Option<String> {
        let found = Builtin::all().iter().find(|b| b.name() == name)?;
        Some(format!("import {{ {name} }} from {};", found.library))
    }

    /// Whether `name` is a library an import may name: the main one, or one that holds a
    /// builtin.
    pub fn is_library(name: &str) -> bool {
        name == MAIN_LIBRARY || Builtin::all().iter().any(|b| b.library == name)
    }

    /// Every library, the main one first.
    pub fn libraries() -> Vec<&'static str> {
        let mut out = vec![MAIN_LIBRARY];
        for b in Builtin::all() {
            if !out.contains(&b.library) {
                out.push(b.library);
            }
        }
        out
    }

    /// The builtin whose AIPL source is the file at `path` and whose library spelling is
    /// `name`: what a builtin's file means by its `pub fn`.
    pub fn implemented_by(path: &std::path::Path, name: &str) -> Option<&'static Builtin> {
        let file = path.file_name()?.to_str()?;
        Builtin::all().iter().find(|b| {
            b.name() == name && matches!(b.definition, Definition::Aipl { file: f } if f == file)
        })
    }

    /// The builtin whose AIPL source is the file at `path`, judged by its file
    /// name.
    pub fn implemented_at(path: &std::path::Path) -> Option<&'static Builtin> {
        let file = path.file_name()?.to_str()?;
        Builtin::all()
            .iter()
            .find(|b| matches!(b.definition, Definition::Aipl { file: f } if f == file))
    }

    /// The spelling an import list uses.
    pub fn name(&self) -> &str {
        self.public_name
            .unwrap_or_else(|| self.callee.import_name())
    }

    /// Whether it is written in AIPL.
    pub fn is_aipl_implemented(&self) -> bool {
        matches!(self.definition, Definition::Aipl { .. })
    }

    /// The path of its AIPL source, or `None` for a described builtin.
    pub fn source_path(&self) -> Option<PathBuf> {
        match self.definition {
            Definition::Aipl { file } => Some(PathBuf::from(SOURCE_DIR).join(file)),
            Definition::Declared | Definition::Described { .. } => None,
        }
    }
}
