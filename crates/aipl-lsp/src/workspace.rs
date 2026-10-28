//! Cheap textual tests the requests use to decide which files are worth looking
//! at.

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
}
