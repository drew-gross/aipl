//! Finding the `.aipl` files of a workspace, for the requests that have to look
//! beyond the documents the editor has open.
//!
//! The editor tells the server which folders it has open (`initialize`), and
//! nothing about the files inside them, so a question like "where is this used"
//! is answered by walking the folders.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories that hold build output or someone else's files, never the
/// workspace's own sources. A name starting with `.` is skipped too.
const SKIPPED_DIRECTORIES: &[&str] = &["target", "node_modules"];

/// Every `.aipl` file under `root`, in a stable order.
///
/// A symbolic link is not followed — it could lead back out of the workspace or
/// into a loop — and an unreadable directory is skipped rather than reported,
/// since a search that stopped at the first one would answer nothing.
pub(crate) fn aipl_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let path = entry.path();
            if kind.is_dir() {
                if !name.starts_with('.') && !SKIPPED_DIRECTORIES.contains(&name.as_ref()) {
                    pending.push(path);
                }
            } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "aipl") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Whether `word` occurs in `text` as a whole identifier — not as part of a
/// longer one. A cheap test that a file *could* mention a name, so that the
/// files which cannot are never lexed or parsed.
pub(crate) fn mentions_word(text: &str, word: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_is_not_found_inside_a_longer_one() {
        assert!(mentions_word("fn add() {}", "add"));
        assert!(mentions_word("add", "add"));
        assert!(mentions_word("x.add(1)", "add"));
        assert!(!mentions_word("fn adder() {}", "add"));
        assert!(!mentions_word("fn my_add() {}", "add"));
        assert!(!mentions_word("fn add2() {}", "add"));
        assert!(mentions_word("fn adder() { add() }", "add"));
    }

    #[test]
    fn walks_sources_and_skips_build_output() {
        let dir = std::env::temp_dir().join(format!("aipl-lsp-walk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for file in [
            "a.aipl",
            "sub/b.aipl",
            "sub/notes.txt",
            "target/c.aipl",
            ".hidden/d.aipl",
        ] {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().expect("parent")).expect("create directory");
            fs::write(path, "").expect("write file");
        }
        let found = aipl_files(&dir);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(found, vec![dir.join("a.aipl"), dir.join("sub/b.aipl")]);
    }
}
