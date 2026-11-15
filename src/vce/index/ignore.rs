//! A pragmatic `.gitignore` matcher.
//!
//! The scanner needs to skip generated and vendored files the way the project already
//! says they are skipped, instead of keeping a second exclusion list in Rust. This is a
//! deliberate subset of the git format (`TOOL.md` §4):
//!
//! - `#` comments and blank lines;
//! - `!pattern` re-includes, and the **last** matching rule wins;
//! - a trailing `/` matches directories only;
//! - a leading `/` anchors the pattern to the project root;
//! - `*` and `?` match inside one path segment, `**` spans segments.
//!
//! Not supported, by design: nested `.gitignore` files, character classes (`[a-z]`), and
//! negating a file whose parent directory is ignored. Those are documented limits, not
//! silent behaviour: a pattern that cannot be honoured simply does not match.

use std::path::Path;

/// Files read from the project root, in order.
const RULE_FILES: &[&str] = &[".gitignore", ".veneraignore"];

/// One parsed line of a rule file.
#[derive(Debug, Clone)]
struct Rule {
    pattern: String,
    negated: bool,
    dir_only: bool,
    anchored: bool,
}

impl Rule {
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }

        let (negated, rest) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line),
        };

        let dir_only = rest.ends_with('/');
        let rest = rest.trim_end_matches('/');
        let anchored = rest.starts_with('/');
        let pattern = rest.trim_start_matches('/').to_owned();

        if pattern.is_empty() {
            return None;
        }

        Some(Self {
            pattern,
            negated,
            dir_only,
            anchored,
        })
    }

    /// Whether this rule names `path`.
    fn matches(&self, path: &str, is_dir: bool) -> bool {
        // A directory-only rule never matches a file by its own path, but it does match
        // that file through an ignored ancestor.
        let mut targets: Vec<&str> = Vec::new();
        if !self.dir_only || is_dir {
            targets.push(path);
        }
        let mut rest = path;
        while let Some(index) = rest.rfind('/') {
            rest = &rest[..index];
            targets.push(rest);
        }

        targets.into_iter().any(|target| {
            if self.anchored || self.pattern.contains('/') {
                glob(&self.pattern, target)
            } else {
                suffix_glob(&self.pattern, target)
            }
        })
    }
}

/// Rules read from the project root.
#[derive(Debug, Clone, Default)]
pub struct IgnoreRules {
    rules: Vec<Rule>,
}

impl IgnoreRules {
    /// No rules: nothing is ignored.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Reads `.gitignore` and `.veneraignore` from `root`, ignoring unreadable files.
    pub fn load(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        let mut rules = Vec::new();

        for name in RULE_FILES {
            let Ok(text) = std::fs::read_to_string(root.join(name)) else {
                continue;
            };
            for line in text.lines() {
                if let Some(rule) = Rule::parse(line) {
                    rules.push(rule);
                }
            }
        }

        Self { rules }
    }

    /// Whether the rules cover `rel_path`.
    pub fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        // The last matching rule wins, exactly like git.
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches(rel_path, is_dir) {
                ignored = !rule.negated;
            }
        }
        ignored
    }

    /// Number of loaded rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether no rule was loaded.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Matches an unanchored pattern against `path` at any component boundary.
fn suffix_glob(pattern: &str, path: &str) -> bool {
    if glob(pattern, path) {
        return true;
    }

    path.match_indices('/')
        .any(|(index, _)| glob(pattern, &path[index + 1..]))
}

/// Segment-wise glob match with `*`, `?`, and `**`.
fn glob(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    match_segments(&pattern, &path)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.split_first(), path.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&"**", rest)), _) => {
            // `**` may consume zero or more leading segments.
            (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]))
        }
        (Some(_), None) => false,
        (Some((segment, rest)), Some((part, path_rest))) => {
            segment_match(segment, part) && match_segments(rest, path_rest)
        }
    }
}

/// Matches one segment, where `*` and `?` never cross a path separator.
fn segment_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut memo = vec![vec![None; text.len() + 1]; pattern.len() + 1];

    fn walk(
        pattern: &[char],
        text: &[char],
        row: usize,
        column: usize,
        memo: &mut [Vec<Option<bool>>],
    ) -> bool {
        if let Some(result) = memo[row][column] {
            return result;
        }

        let result = match pattern.split_first() {
            None => column == text.len(),
            Some(('*', rest)) => {
                walk(rest, text, row + 1, column, memo)
                    || (column < text.len() && walk(pattern, text, row, column + 1, memo))
            }
            Some(('?', rest)) => column < text.len() && walk(rest, text, row + 1, column + 1, memo),
            Some((head, rest)) => {
                column < text.len()
                    && text[column] == *head
                    && walk(rest, text, row + 1, column + 1, memo)
            }
        };

        memo[row][column] = Some(result);
        result
    }

    walk(&pattern, &text, 0, 0, &mut memo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(lines: &[&str]) -> IgnoreRules {
        IgnoreRules {
            rules: lines.iter().filter_map(|line| Rule::parse(line)).collect(),
        }
    }

    #[test]
    fn matches_a_bare_name_at_any_depth() {
        let rules = rules(&["node_modules/"]);

        assert!(rules.is_ignored("node_modules", true));
        assert!(rules.is_ignored("node_modules/pkg/index.js", false));
        assert!(rules.is_ignored("packages/web/node_modules", true));
        assert!(!rules.is_ignored("src/main.rs", false));
    }

    #[test]
    fn honours_anchoring() {
        let rules = rules(&["/dist/", "build"]);

        assert!(rules.is_ignored("dist", true));
        assert!(!rules.is_ignored("app/dist", true), "anchored to the root");
        assert!(
            rules.is_ignored("app/build", true),
            "unanchored still matches"
        );
    }

    #[test]
    fn wildcards_stay_inside_one_segment() {
        let rules = rules(&["*.log", "src/*.tmp"]);

        assert!(rules.is_ignored("debug.log", false));
        assert!(rules.is_ignored("logs/debug.log", false));
        assert!(rules.is_ignored("src/a.tmp", false));
        assert!(!rules.is_ignored("src/deep/a.tmp", false));
    }

    #[test]
    fn double_star_spans_segments() {
        let rules = rules(&["**/generated/*.rs", "docs/**"]);

        assert!(rules.is_ignored("a/b/generated/x.rs", false));
        assert!(rules.is_ignored("generated/x.rs", false));
        assert!(rules.is_ignored("docs/readme.md", false));
        assert!(!rules.is_ignored("readme.md", false));
    }

    #[test]
    fn the_last_matching_rule_wins() {
        let rules = rules(&["*.log", "!keep.log"]);

        assert!(rules.is_ignored("debug.log", false));
        assert!(!rules.is_ignored("keep.log", false));
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let rules = rules(&["# comment", "", "   ", "target/"]);

        assert_eq!(rules.len(), 1);
        assert!(rules.is_ignored("target", true));
    }

    #[test]
    fn reads_the_rule_files_from_the_root() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(".gitignore"), "target/\n*.log\n").expect("write");
        std::fs::write(dir.path().join(".veneraignore"), "scratch\n").expect("write");

        let rules = IgnoreRules::load(dir.path());
        assert_eq!(rules.len(), 3);
        assert!(rules.is_ignored("target/debug/x", false));
        assert!(rules.is_ignored("scratch", true));
        assert!(!rules.is_ignored("src/lib.rs", false));
    }

    #[test]
    fn an_empty_root_yields_no_rules() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rules = IgnoreRules::load(dir.path());

        assert!(rules.is_empty());
        assert!(!rules.is_ignored("anything", false));
    }
}
