//! Which files a workspace holds, and cheap textual tests the requests use to
//! decide which of them are worth looking at.

use std::path::{Path, PathBuf};

/// Every `.aipl` file under `roots`, in path order. Hidden directories, `target`
/// and `node_modules` hold no source the author wrote, and a symlink is not
/// followed, so a link back up the tree cannot loop the walk.
pub(crate) fn aipl_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(directory: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                if !name.starts_with('.') && name != "target" && name != "node_modules" {
                    walk(&path, found);
                }
            } else if kind.is_file() && path.extension().is_some_and(|e| e == "aipl") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    for root in roots {
        walk(root, &mut found);
    }
    found.sort();
    found.dedup();
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
    fn the_walk_finds_aipl_files_and_skips_hidden_and_build_directories() {
        let root = std::env::temp_dir().join(format!("aipl-lsp-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for file in [
            "a.aipl",
            "sub/b.aipl",
            "sub/c.txt",
            ".git/d.aipl",
            "target/e.aipl",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
            std::fs::write(path, "").expect("file");
        }
        let found = aipl_files(std::slice::from_ref(&root));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found, vec![root.join("a.aipl"), root.join("sub/b.aipl")]);
    }

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
}
