//! Preservation validation (`TOOL.md` extension §6).
//!
//! Compression is only allowed to remove information that the request does not depend on.
//! Before a compressed context is accepted, this module extracts the parts that carry
//! meaning — paths, identifiers, numbers, quoted strings, and whole constraint lines — and
//! checks they survived. A failure is reported, never silently accepted.
//!
//! The check is deliberately lexical: it cannot prove that meaning is preserved, but it
//! reliably catches the common failure — a constraint dropped because it looked like
//! prose.

/// Result of one preservation check.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Validation {
    /// Whether every extracted token survived.
    pub ok: bool,
    /// Tokens that are missing from the compressed text.
    pub missing: Vec<String>,
    /// How many tokens were checked.
    pub checked: usize,
}

/// Checks that `compressed` still contains everything meaningful from `original`.
pub fn preserve(original: &str, compressed: &str) -> Validation {
    let tokens = critical_tokens(original);
    let haystack = compressed.to_lowercase();
    let mut missing = Vec::new();

    for token in &tokens {
        if !haystack.contains(&token.to_lowercase()) {
            missing.push(token.clone());
        }
    }

    Validation {
        ok: missing.is_empty(),
        missing,
        checked: tokens.len(),
    }
}

/// Extracts the parts of `text` that must not be lost.
///
/// Paths, numbers, quoted strings, backticked spans, and identifiers from constraint lines.
/// Duplicates are removed, order is kept, so the report is stable.
pub fn critical_tokens(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        if is_constraint_line(line) {
            for word in line.split(|ch: char| !(ch.is_alphanumeric() || ch == '_')) {
                if word.chars().count() >= 4 && !is_stopword(word) {
                    push(&mut tokens, word.to_owned());
                }
            }
        }

        for word in line.split_whitespace() {
            let cleaned = word.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | '.' | ';' | ':' | '(' | ')' | '[' | ']' | '"' | '\'' | '`'
                )
            });
            if cleaned.is_empty() {
                continue;
            }

            if looks_like_path(cleaned) {
                push(&mut tokens, cleaned.to_owned());
            }

            if cleaned
                .chars()
                .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == '_')
                && cleaned.chars().any(|ch| ch.is_ascii_digit())
            {
                push(&mut tokens, cleaned.to_owned());
            }
        }

        for quoted in quoted_spans(line) {
            if !quoted.is_empty() {
                push(&mut tokens, quoted);
            }
        }

        for ticked in line.split('`').skip(1).step_by(2) {
            let ticked = ticked.trim();
            if !ticked.is_empty() {
                push(&mut tokens, ticked.to_owned());
            }
        }
    }

    tokens
}

fn push(tokens: &mut Vec<String>, token: String) {
    if !tokens.iter().any(|existing| existing == &token) {
        tokens.push(token);
    }
}

fn is_constraint_line(line: &str) -> bool {
    const MARKERS: &[&str] = &[
        "must",
        "never",
        "cannot",
        "forbidden",
        "required",
        "нельзя",
        "не должен",
        "обязан",
        "запрещ",
        "требуется",
    ];
    let lower = line.to_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}

fn is_stopword(word: &str) -> bool {
    const STOPWORDS: &[&str] = &[
        "must",
        "never",
        "cannot",
        "forbidden",
        "required",
        "should",
        "there",
        "their",
        "which",
        "when",
        "that",
        "this",
        "with",
        "from",
        "into",
        "does",
        "mustn",
        "нельзя",
        "должен",
        "обязан",
        "запрещ",
        "требуется",
        "который",
        "чтобы",
        "всегда",
        "также",
    ];
    let lower = word.to_lowercase();
    STOPWORDS.contains(&lower.as_str())
}

fn looks_like_path(word: &str) -> bool {
    (word.contains('/') && word.contains('.')) || word.starts_with("src/") || word.starts_with("./")
}

fn quoted_spans(line: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut current: Option<(char, String)> = None;

    for ch in line.chars() {
        match &mut current {
            Some((quote, buffer)) if *quote == ch => {
                spans.push(buffer.clone());
                current = None;
            }
            Some((_, buffer)) => buffer.push(ch),
            None if ch == '"' || ch == '\'' => current = Some((ch, String::new())),
            None => {}
        }
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_constraint_is_reported() {
        let original =
            "The timeout must not be zero.\nUse `validate_timeout` in src/config.rs.\nLimit: 30.";
        let compressed = "The timeout must not be zero.";

        let validation = preserve(original, compressed);

        assert!(!validation.ok);
        assert!(
            validation
                .missing
                .iter()
                .any(|token| token.contains("validate_timeout"))
        );
        assert!(
            validation
                .missing
                .iter()
                .any(|token| token == "src/config.rs")
        );
        assert!(validation.missing.iter().any(|token| token == "30"));
    }

    #[test]
    fn a_full_copy_passes() {
        let text = "The timeout must not be zero.\nUse `validate_timeout` in src/config.rs.";

        let validation = preserve(text, text);

        assert!(validation.ok, "missing: {:?}", validation.missing);
        assert!(validation.checked >= 2);
    }

    #[test]
    fn identifiers_of_constraint_lines_are_collected() {
        let tokens = critical_tokens("Requests must never bypass PathGuard.");

        assert!(tokens.iter().any(|token| token == "Requests"));
        assert!(tokens.iter().any(|token| token == "bypass"));
        assert!(tokens.iter().any(|token| token == "PathGuard"));
        assert!(!tokens.iter().any(|token| token == "must"));
    }

    #[test]
    fn numbers_are_collected_even_outside_constraints() {
        let tokens = critical_tokens("Return 404 when the window is 272000 tokens.");

        assert!(tokens.iter().any(|token| token == "404"));
        assert!(tokens.iter().any(|token| token == "272000"));
    }

    #[test]
    fn quoted_strings_are_collected() {
        let tokens = critical_tokens("Return \"Timeout cannot be zero\" on failure.");

        assert!(tokens.iter().any(|token| token == "Timeout cannot be zero"));
    }

    #[test]
    fn an_empty_original_passes_trivially() {
        let validation = preserve("", "anything");

        assert!(validation.ok);
        assert_eq!(validation.checked, 0);
    }

    #[test]
    fn duplicates_are_removed() {
        let tokens = critical_tokens("must check src/a.rs and src/a.rs again");

        assert_eq!(
            tokens.iter().filter(|token| *token == "src/a.rs").count(),
            1
        );
    }
}
