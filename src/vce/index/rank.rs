//! Task-aware file ranking.
//!
//! The map and the measurement must agree on which files matter, so both go through this
//! module instead of keeping two scorers (`TOOL.md` §7).
//!
//! Signals, cheapest to most expensive, with the roadmap's starting weights:
//!
//! | signal | weight | why |
//! |---|---|---|
//! | explicit path mention | 1000 | the user already named the file |
//! | path match | 8 | a file named `auth` is about authentication |
//! | exact symbol name | 7 | the definition itself |
//! | signature match | 5 | parameters often carry the domain word |
//! | qualified name | 4 | owner plus member |
//! | import match | 3 | the dependency that owns the concept |
//! | body match | 2 | the weakest evidence: any mention counts |
//!
//! Two corrections to naive matching: every signal is capped, so one long file cannot
//! outscore everything by repetition, and symbol matches are scaled by inverse document
//! frequency, so `config` counts less than `authenticate_request`.
//!
//! Tokens use Unicode lowercasing: the previous ASCII-only fold silently dropped every
//! Cyrillic query and fell back to the default order.

use std::collections::HashMap;

use crate::vce::index::text;

/// Weight of a path match.
pub const PATH_WEIGHT: f64 = 8.0;
/// Weight of an exact symbol-name match.
pub const EXACT_SYMBOL_WEIGHT: f64 = 7.0;
/// Weight of a signature match.
pub const SIGNATURE_WEIGHT: f64 = 5.0;
/// Weight of a qualified-name match.
pub const QUALIFIED_WEIGHT: f64 = 4.0;
/// Weight of an import match.
pub const IMPORT_WEIGHT: f64 = 3.0;
/// Weight of a body match.
pub const BODY_WEIGHT: f64 = 2.0;
/// Score added when the task names the file explicitly.
pub const EXPLICIT_PATH_BONUS: f64 = 1000.0;

/// Most repetitions of one symbol signal that still count.
const SIGNAL_CAP: f64 = 5.0;

/// Most body mentions that still count, across every token.
///
/// Body evidence is the weakest signal: a file that repeats the word twenty times is not
/// more about it than a file whose path names it. Capped so `BODY_WEIGHT * BODY_CAP`
/// stays below one path match.
const BODY_CAP: f64 = 3.0;

/// A normalized task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    tokens: Vec<String>,
    lower: String,
}

impl Query {
    /// Normalizes `task`; `None` when it contains no usable token.
    pub fn new(task: &str) -> Option<Self> {
        let lower = task.to_lowercase();
        let mut tokens: Vec<String> = lower
            .split(|ch: char| !text::is_name_char(ch))
            .filter(|token| token.chars().count() >= text::MIN_TOKEN)
            .map(str::to_owned)
            .collect();

        tokens.sort();
        tokens.dedup();

        if tokens.is_empty() {
            None
        } else {
            Some(Self { tokens, lower })
        }
    }

    /// The normalized tokens.
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    /// Whether the task mentions `path` literally.
    pub fn mentions_path(&self, path: &str) -> bool {
        !path.is_empty() && self.lower.contains(&path.to_lowercase())
    }
}

/// What a file offers to match against.
#[derive(Debug)]
pub struct Signals<'a> {
    /// Project-relative path.
    pub path: &'a str,
    /// Plain symbol names.
    pub names: &'a [String],
    /// Qualified symbol names.
    pub qualified_names: &'a [String],
    /// Symbol signatures.
    pub signatures: &'a [String],
    /// Imported module and symbol names.
    pub imports: &'a [String],
    /// Full text, when the caller has it.
    pub body: Option<&'a str>,
    /// Project-wide token counts, used for IDF.
    pub mentions: &'a HashMap<String, u32>,
}

impl Signals<'_> {
    /// Relevance of this file to `query`, in arbitrary units.
    pub fn score(&self, query: &Query) -> f64 {
        if query.mentions_path(self.path) {
            return EXPLICIT_PATH_BONUS;
        }

        let path = self.path.to_lowercase();
        let body = self.body.map(str::to_lowercase);
        let total: u64 = self.mentions.values().map(|count| u64::from(*count)).sum();
        let mut score = 0.0;
        let mut body_hits = 0.0;

        for token in query.tokens() {
            if path.contains(token.as_str()) {
                score += PATH_WEIGHT;
            }

            score +=
                EXACT_SYMBOL_WEIGHT * idf(token, self.mentions, total) * self.exact_names(token);

            if self
                .signatures
                .iter()
                .any(|sig| sig.to_lowercase().contains(token.as_str()))
            {
                score += SIGNATURE_WEIGHT;
            }

            if self
                .qualified_names
                .iter()
                .any(|name| name.to_lowercase().contains(token.as_str()))
            {
                score += QUALIFIED_WEIGHT;
            }

            if self
                .imports
                .iter()
                .any(|import| import.to_lowercase().contains(token.as_str()))
            {
                score += IMPORT_WEIGHT;
            }

            if let Some(body) = &body {
                body_hits += matches(body, token);
            }
        }

        score + BODY_WEIGHT * body_hits.min(BODY_CAP)
    }

    /// How many declared symbols are named exactly `token`, capped.
    fn exact_names(&self, token: &str) -> f64 {
        let count = self
            .names
            .iter()
            .filter(|name| name.to_lowercase() == token)
            .count();

        capped(f64::from(count as u32))
    }
}

/// Occurrences of `needle` in `haystack`, counted without overlapping logic.
fn matches(haystack: &str, needle: &str) -> f64 {
    haystack.matches(needle).count() as f64
}

fn capped(value: f64) -> f64 {
    value.min(SIGNAL_CAP)
}

/// Inverse document frequency of a token, from project-wide occurrence counts.
///
/// Common names (`config`, `data`, `result`) get a small factor, rare ones a large one.
/// The floor keeps a very common name from vanishing entirely.
pub fn idf(token: &str, mentions: &HashMap<String, u32>, total: u64) -> f64 {
    if total == 0 {
        return 1.0;
    }

    let count = f64::from(mentions.get(token).copied().unwrap_or(0));
    let factor = (1.0 + total as f64 / (1.0 + count)).ln();
    factor.clamp(0.25, 8.0)
}

/// Orders files by descending score, ties broken by path.
///
/// Deterministic for identical input: the sort key is total, never partial.
pub fn order<F>(paths: &[String], mut score: F) -> Vec<usize>
where
    F: FnMut(usize) -> f64,
{
    let mut scored: Vec<(usize, f64)> = (0..paths.len())
        .map(|index| (index, score(index)))
        .collect();

    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| paths[left.0].cmp(&paths[right.0]))
    });

    scored.into_iter().map(|(index, _)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mentions(pairs: &[(&str, u32)]) -> HashMap<String, u32> {
        pairs
            .iter()
            .map(|(name, count)| ((*name).to_owned(), *count))
            .collect()
    }

    fn signals<'a>(
        path: &'a str,
        names: &'a [String],
        body: &'a str,
        mentions: &'a HashMap<String, u32>,
    ) -> Signals<'a> {
        Signals {
            path,
            names,
            qualified_names: &[],
            signatures: &[],
            imports: &[],
            body: Some(body),
            mentions,
        }
    }

    #[test]
    fn normalizes_a_russian_task() {
        let query = Query::new("Сделай кнопку красной!").expect("tokens");

        assert!(query.tokens().contains(&"кнопку".to_owned()));
        assert!(query.tokens().contains(&"красной".to_owned()));
        assert!(Query::new("!! ?? ..").is_none());
    }

    #[test]
    fn an_explicit_path_wins_over_everything() {
        let counts = mentions(&[("auth", 50)]);
        let query = Query::new("починить auth в src/auth.rs").expect("query");
        let file = signals("src/auth.rs", &[], "fn x() {}", &counts);

        assert_eq!(file.score(&query), EXPLICIT_PATH_BONUS);
    }

    #[test]
    fn path_matches_beat_body_matches() {
        let counts = mentions(&[("auth", 1), ("x", 1)]);
        let query = Query::new("auth").expect("query");

        let in_path = signals("src/auth/mod.rs", &[], "", &counts);
        let mut noisy_body = String::new();
        for _ in 0..50 {
            noisy_body.push_str("auth ");
        }
        let in_body = signals("src/other.rs", &[], &noisy_body, &counts);

        assert!(in_path.score(&query) > in_body.score(&query));
    }

    #[test]
    fn exact_symbol_names_outrank_a_pass_in_the_body() {
        let names = vec!["authenticate".to_owned()];
        let counts = mentions(&[("authenticate", 2)]);
        let query = Query::new("authenticate").expect("query");

        let symbol = signals("src/auth.rs", &names, "", &counts);
        let body = signals("src/log.rs", &[], "authenticate", &counts);

        assert!(symbol.score(&query) > body.score(&query));
    }

    #[test]
    fn rare_names_count_more_than_common_ones() {
        let counts = mentions(&[("config", 400), ("authenticate", 2)]);
        let total = 402;

        assert!(idf("authenticate", &counts, total) > idf("config", &counts, total) * 2.0);
        assert!(idf("absent", &counts, total) > 1.0);
    }

    #[test]
    fn repeated_body_hits_stop_counting_at_the_cap() {
        let counts = mentions(&[("auth", 1)]);
        let query = Query::new("auth").expect("query");
        let once = "auth";
        let many = vec!["auth"; 40].join(" ");

        let single = signals("a.rs", &[], once, &counts).score(&query);
        let capped = signals("a.rs", &[], &many, &counts).score(&query);

        assert_eq!(capped - single, BODY_WEIGHT * (BODY_CAP - 1.0));
    }

    #[test]
    fn ordering_is_deterministic_with_ties() {
        let paths = vec!["b.rs".to_owned(), "a.rs".to_owned(), "c.rs".to_owned()];
        let order = order(&paths, |_| 1.0);

        assert_eq!(order, vec![1, 0, 2], "tie broken by path");
    }
}
