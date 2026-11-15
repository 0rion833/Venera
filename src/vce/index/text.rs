//! Identifier tokenization shared by the map, the indexer, and task ranking.
//!
//! One definition of "a name" keeps the weights in `map` and the `file_mentions` table in
//! the database from drifting apart: a difference there would make the map depend on
//! whether it was rendered from memory or from SQLite.

use std::collections::HashMap;

/// Shortest token kept; shorter names match too much noise.
pub const MIN_TOKEN: usize = 3;

/// Characters that may appear inside a name: letters, digits, `_`, `-`.
pub fn is_name_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '-'
}

/// Counts identifier-like tokens of at least [`MIN_TOKEN`] characters.
pub fn name_counts(text: &str) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    add_name_counts(text, &mut counts);
    counts
}

/// Adds the counts of `text` into an existing map, so a caller can merge files cheaply.
pub fn add_name_counts(text: &str, counts: &mut HashMap<String, u32>) {
    for token in text.split(|ch: char| !is_name_char(ch)) {
        if token.chars().count() >= MIN_TOKEN {
            *counts.entry(token.to_owned()).or_insert(0) += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_long_tokens_only() {
        let counts = name_counts("let ab = alpha + alpha; ab");

        assert_eq!(counts.get("alpha"), Some(&2));
        assert_eq!(counts.get("let"), Some(&1));
        assert_eq!(counts.get("ab"), None, "two characters is noise");
    }

    #[test]
    fn keeps_underscores_and_dashes() {
        let counts = name_counts("fn read_file() { --brand: x; }");

        assert_eq!(counts.get("read_file"), Some(&1));
        assert_eq!(
            counts.get("--brand"),
            Some(&1),
            "a dash is part of a name, so a CSS custom property stays whole"
        );
    }
}
