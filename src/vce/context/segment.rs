//! Context segmentation: source and user text split into addressable pieces.
//!
//! The compressor must not receive an indivisible blob (`TOOL.md` extension §4). Every
//! piece keeps its provenance — path, symbol, line range, hash — so a later stage can drop
//! it, keep it, or fetch it again from the index.
//!
//! Source segmentation reuses the symbol ranges already stored by the index; it never
//! parses anything a second time. User text is split by paragraphs and headings, because
//! that is what a specification actually consists of.

use crate::config::Encoding;
use crate::provider::tokenizer;
use crate::vce::index::m1::SymbolRow;

/// What a segment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// An import or use declaration.
    Import,
    /// A type, struct, class, enum, or trait.
    Type,
    /// A constant or static.
    Constant,
    /// A free function or method.
    Function,
    /// An impl block or its body.
    Implementation,
    /// A test.
    Test,
    /// Documentation, comments, headings.
    Documentation,
    /// A user requirement.
    Requirement,
    /// A hard constraint or prohibition.
    Constraint,
    /// An explanation or narrative.
    Explanation,
    /// An error log or stack trace.
    ErrorLog,
}

impl SegmentKind {
    /// Short stable name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Type => "type",
            Self::Constant => "const",
            Self::Function => "fn",
            Self::Implementation => "impl",
            Self::Test => "test",
            Self::Documentation => "doc",
            Self::Requirement => "requirement",
            Self::Constraint => "constraint",
            Self::Explanation => "text",
            Self::ErrorLog => "error",
        }
    }
}

/// How badly a segment must survive compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Importance {
    /// Must never be dropped automatically.
    Critical,
    /// Needed to understand or edit the code.
    High,
    /// Useful context.
    Normal,
    /// Droppable when space is short.
    Low,
    /// Noise for this request.
    Irrelevant,
}

impl Importance {
    /// Short stable name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Normal => "normal",
            Self::Low => "low",
            Self::Irrelevant => "irrelevant",
        }
    }

    /// Whether compression may drop this segment.
    pub fn droppable(self) -> bool {
        matches!(self, Self::Low | Self::Irrelevant)
    }
}

/// One addressable piece of context.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSegment {
    /// Stable id built from provenance.
    pub id: String,
    /// File the segment came from, when it is source.
    pub source_path: Option<String>,
    /// Qualified symbol name, when the segment is a declaration.
    pub symbol: Option<String>,
    /// First line, one-based.
    pub start_line: Option<u32>,
    /// Last line, one-based.
    pub end_line: Option<u32>,
    /// Tokens of [`Self::text`].
    pub token_count: usize,
    /// Relevance to the current request; filled by the distributor.
    pub relevance_score: f32,
    /// How important the segment is.
    pub importance: Importance,
    /// BLAKE3 of [`Self::text`].
    pub content_hash: String,
    /// What kind of content this is.
    pub kind: SegmentKind,
    /// The content itself.
    pub text: String,
}

impl ContextSegment {
    fn new(
        encoding: Encoding,
        id: String,
        kind: SegmentKind,
        importance: Importance,
        text: String,
    ) -> Self {
        Self {
            id,
            source_path: None,
            symbol: None,
            start_line: None,
            end_line: None,
            token_count: tokenizer::count(encoding, &text),
            relevance_score: 0.0,
            importance,
            content_hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
            kind,
            text,
        }
    }
}

/// Splits a retrieved source range into symbol segments plus any uncovered text.
///
/// `base_line` is the one-based line the first line of `text` has in the file. Symbols
/// outside the range are ignored; text that no symbol covers becomes an explanation
/// segment, so nothing silently disappears.
pub fn from_source(
    encoding: Encoding,
    path: &str,
    text: &str,
    base_line: u32,
    symbols: &[SymbolRow],
) -> Vec<ContextSegment> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let last_line = base_line + lines.len() as u32 - 1;
    let mut segments = Vec::new();
    let mut cursor = base_line;

    let mut relevant: Vec<&SymbolRow> = symbols
        .iter()
        .filter(|symbol| symbol.start_line <= last_line && symbol.end_line >= base_line)
        .collect();
    relevant.sort_by_key(|symbol| (symbol.start_line, symbol.end_line));

    let push_text = |segments: &mut Vec<ContextSegment>, start: u32, end: u32| {
        let body = slice(&lines, base_line, start, end);
        if body.trim().is_empty() {
            return;
        }

        let mut segment = ContextSegment::new(
            encoding,
            format!("{path}:{start}-{end}"),
            SegmentKind::Explanation,
            Importance::Normal,
            body,
        );
        segment.source_path = Some(path.to_owned());
        segment.start_line = Some(start);
        segment.end_line = Some(end);
        segments.push(segment);
    };

    for symbol in relevant {
        let start = symbol.start_line.max(cursor).max(base_line);
        let end = symbol.end_line.min(last_line);
        if start > end {
            continue;
        }

        // Text before the declaration (comments, attributes) is context too.
        if start > cursor {
            push_text(&mut segments, cursor, start - 1);
        }

        let body = slice(&lines, base_line, start, end);
        let mut segment = ContextSegment::new(
            encoding,
            format!("{path}:{start}-{end}"),
            kind_of(&symbol.kind),
            importance_of(&symbol.kind, &symbol.qualified_name),
            body,
        );
        segment.source_path = Some(path.to_owned());
        segment.symbol = Some(symbol.qualified_name.clone());
        segment.start_line = Some(start);
        segment.end_line = Some(end);
        segments.push(segment);

        cursor = cursor.max(end + 1);
    }

    if cursor <= last_line {
        push_text(&mut segments, cursor, last_line);
    }

    segments
}

/// Splits free text (a task, a specification) into paragraphs and headings.
pub fn from_text(encoding: Encoding, text: &str) -> Vec<ContextSegment> {
    let mut segments = Vec::new();
    let mut index = 0usize;

    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }

        index += 1;
        let kind = classify_text(paragraph);
        let importance = match kind {
            SegmentKind::Constraint | SegmentKind::Requirement => Importance::Critical,
            SegmentKind::Documentation => Importance::Normal,
            SegmentKind::ErrorLog => Importance::High,
            _ => Importance::Low,
        };

        segments.push(ContextSegment::new(
            encoding,
            format!("text:{index}"),
            kind,
            importance,
            paragraph.to_owned(),
        ));
    }

    segments
}

fn slice(lines: &[&str], base_line: u32, start: u32, end: u32) -> String {
    let from = (start - base_line) as usize;
    let to = (end - base_line) as usize;
    if from >= lines.len() || to >= lines.len() || from > to {
        return String::new();
    }
    lines[from..=to].join("\n")
}

/// Maps a stored kind name to a segment kind.
fn kind_of(kind: &str) -> SegmentKind {
    match kind {
        "use" | "import" => SegmentKind::Import,
        "struct" | "enum" | "trait" | "type" | "mod" | "class" => SegmentKind::Type,
        "const" | "var" => SegmentKind::Constant,
        "fn" => SegmentKind::Function,
        "impl" => SegmentKind::Implementation,
        _ => SegmentKind::Documentation,
    }
}

fn importance_of(kind: &str, qualified_name: &str) -> Importance {
    if qualified_name.contains("test") || qualified_name.starts_with("tests::") {
        return Importance::Normal;
    }

    match kind {
        "fn" | "impl" => Importance::High,
        "struct" | "enum" | "trait" | "type" | "mod" | "class" => Importance::Normal,
        "use" | "import" | "const" | "var" => Importance::Low,
        _ => Importance::Normal,
    }
}

/// Heuristic kind of a user-text paragraph.
fn classify_text(paragraph: &str) -> SegmentKind {
    let lower = paragraph.to_lowercase();

    const CONSTRAINTS: &[&str] = &[
        "must not",
        "mustn't",
        "never",
        "forbidden",
        "cannot",
        "can't",
        "нельзя",
        "не должен",
        "запрещ",
        "обязан",
        "required",
        "must ",
    ];
    const REQUIREMENTS: &[&str] = &["should", "need", "want", "нужно", "надо", "требу", "долж"];
    const ERRORS: &[&str] = &[
        "error",
        "panic",
        "exception",
        "traceback",
        "ошибка",
        "стектрейс",
    ];

    if paragraph.starts_with('#') {
        return SegmentKind::Documentation;
    }
    if CONSTRAINTS.iter().any(|needle| lower.contains(needle)) {
        return SegmentKind::Constraint;
    }
    if ERRORS.iter().any(|needle| lower.contains(needle)) {
        return SegmentKind::ErrorLog;
    }
    if REQUIREMENTS.iter().any(|needle| lower.contains(needle)) {
        return SegmentKind::Requirement;
    }
    SegmentKind::Explanation
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, kind: &str, start: u32, end: u32) -> SymbolRow {
        SymbolRow {
            id: 1,
            file_id: 1,
            name: name.to_owned(),
            qualified_name: name.to_owned(),
            kind: kind.to_owned(),
            signature: format!("fn {name}()"),
            start_line: start,
            end_line: end,
        }
    }

    #[test]
    fn splits_source_into_symbol_segments() {
        let text = "use std::path::Path;\n\nfn alpha() {\n    one();\n}\n\nfn beta() {}\n";
        let symbols = vec![
            symbol("Path", "use", 1, 1),
            symbol("alpha", "fn", 3, 5),
            symbol("beta", "fn", 7, 7),
        ];

        let segments = from_source(Encoding::O200kBase, "src/a.rs", text, 1, &symbols);
        let ids: Vec<&str> = segments.iter().map(|segment| segment.id.as_str()).collect();

        assert_eq!(ids, vec!["src/a.rs:1-1", "src/a.rs:3-5", "src/a.rs:7-7"]);
        assert_eq!(segments[1].kind, SegmentKind::Function);
        assert_eq!(segments[1].importance, Importance::High);
        assert_eq!(segments[0].importance, Importance::Low);
        assert!(segments[1].text.contains("fn alpha"));
    }

    #[test]
    fn keeps_text_that_no_symbol_covers() {
        let text = "// top comment\nfn alpha() {}\n// trailing\n";
        let symbols = vec![symbol("alpha", "fn", 2, 2)];

        let segments = from_source(Encoding::O200kBase, "src/a.rs", text, 1, &symbols);

        assert_eq!(segments.len(), 3, "comment, function, comment");
        assert!(
            segments
                .first()
                .expect("first")
                .text
                .contains("top comment")
        );
        assert!(segments.last().expect("last").text.contains("trailing"));
    }

    #[test]
    fn honours_a_non_zero_base_line() {
        let text = "    check();\n}";
        let symbols = vec![symbol("guard", "fn", 1, 3)];

        let segments = from_source(Encoding::O200kBase, "src/a.rs", text, 2, &symbols);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start_line, Some(2));
        assert_eq!(segments[0].end_line, Some(3));
    }

    #[test]
    fn marks_tests_as_normal_importance() {
        let text = "fn test_alpha() {}\n";
        let symbols = vec![symbol("tests::test_alpha", "fn", 1, 1)];

        let segments = from_source(Encoding::O200kBase, "src/a.rs", text, 1, &symbols);

        assert_eq!(segments[0].importance, Importance::Normal);
    }

    #[test]
    fn classifies_specification_paragraphs() {
        let text = "# Rules\n\nThe timeout must not be zero.\n\nWe should add a retry.\n\npanic: index out of bounds\n\nJust prose.";
        let segments = from_text(Encoding::O200kBase, text);
        let kinds: Vec<SegmentKind> = segments.iter().map(|segment| segment.kind).collect();

        assert_eq!(
            kinds,
            vec![
                SegmentKind::Documentation,
                SegmentKind::Constraint,
                SegmentKind::Requirement,
                SegmentKind::ErrorLog,
                SegmentKind::Explanation,
            ]
        );
        assert_eq!(segments[1].importance, Importance::Critical);
        assert_eq!(segments[4].importance, Importance::Low);
    }

    #[test]
    fn every_segment_carries_tokens_and_a_hash() {
        let segments = from_text(Encoding::O200kBase, "The timeout must not be zero.");

        assert!(segments[0].token_count > 0);
        assert_eq!(segments[0].content_hash.len(), 64);
    }
}
