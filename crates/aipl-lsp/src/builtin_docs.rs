//! A place to go for a builtin that has no source.
//!
//! A builtin the compiler expands itself is declared in a table, not written in a file, so
//! "go to definition" has nowhere to land. Rather than answer with an error, the server writes
//! the builtin's declaration and documentation to a small file and sends the editor there: the
//! same two parts a hover shows, in a place that can be opened, read and searched like the source
//! of a builtin written in AIPL.
//!
//! The file is AIPL, so the editor highlights it as such: a detached comment saying what the
//! file is, then the builtin's signature as a function whose body is a comment, with its
//! documentation above it. It is not a program (the body is not the builtin), so nothing is
//! checked in it, and it is written read-only because editing it changes nothing.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use aipl_index::Symbol;
use aipl_syntax::Span;

/// The directory the documentation files live in.
fn directory() -> PathBuf {
    std::env::temp_dir().join("aipl-builtin-docs")
}

/// Whether `path` is one of the documentation files this module writes. They are not programs,
/// so nothing is checked in them: an empty program is an error, which an editor would show on a
/// file that is only there to be read.
pub(crate) fn is_documentation(path: &std::path::Path) -> bool {
    let directory = std::fs::canonicalize(directory()).unwrap_or_else(|_| directory());
    path.starts_with(directory)
}

/// The documentation file for `symbol`, written if it is not already there as it should be:
/// its path, its text, and the span of the builtin's name in its signature line. `None` when
/// the file cannot be written, or the name is not a plain identifier.
pub(crate) fn document(symbol: &Symbol) -> Option<(PathBuf, String, Span)> {
    if symbol.name.is_empty()
        || !symbol
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let mut text = format!(
        "# `{}` is a builtin the compiler expands itself: there is no source to show, so this is\n\
         # its signature and documentation.\n\
         \n",
        symbol.name
    );
    if let Some(doc) = &symbol.doc {
        for line in doc.lines() {
            if line.is_empty() {
                text.push_str("#\n");
            } else {
                text.push_str(&format!("# {line}\n"));
            }
        }
    }
    let name_at = text.len() + symbol.detail.find(&symbol.name)?;
    text.push_str(&symbol.detail);
    text.push_str(" {\n    // Implemented by the compiler.\n}\n");
    let path = directory().join(format!("{}.aipl", symbol.name));
    write_once(&path, &text)?;
    Some((path, text, name_at..name_at + symbol.name.len()))
}

/// Make `path` hold `text`, reading and writing it at most once per run of the server.
fn write_once(path: &PathBuf, text: &str) -> Option<()> {
    static WRITTEN: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    let mut written = WRITTEN.lock().unwrap_or_else(|p| p.into_inner());
    let written = written.get_or_insert_with(HashSet::new);
    if written.contains(path) {
        return Some(());
    }
    if std::fs::read_to_string(path).ok().as_deref() != Some(text) {
        std::fs::create_dir_all(path.parent()?).ok()?;
        // A read-only file cannot be overwritten in place.
        let _ = std::fs::remove_file(path);
        std::fs::write(path, text).ok()?;
        let mut permissions = std::fs::metadata(path).ok()?.permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions).ok()?;
    }
    written.insert(path.clone());
    Some(())
}
