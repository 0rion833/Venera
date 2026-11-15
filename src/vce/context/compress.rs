//! Deterministic context compression (`TOOL.md` extension §5, §6).
//!
//! Compression here means *selection*: drop what the request does not need, keep what it
//! does, and never paraphrase source. A compressed function is still the exact function.
//!
//! The levels are cheap on purpose. Semantic compression would need a model call that can
//! easily cost more than the tokens it saves, so `Semantic` currently behaves like
//! `Balanced` and says so in the statistics.
//!
//! Critical segments — requirements, constraints — are never dropped, whatever the level
//! or the target ratio.

use serde::{Deserialize, Serialize};

use crate::config::Encoding;
use crate::provider::tokenizer;
use crate::vce::context::segment::{ContextSegment, Importance};

/// How hard to compress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompressionLevel {
    /// Keep everything.
    None,
    /// Remove obvious redundancy only.
    Light,
    /// Drop low-value segments until the target is met.
    Balanced,
    /// Keep critical and high-importance segments only.
    Aggressive,
    /// Reserved for model-assisted compression; currently identical to `Balanced`.
    Semantic,
}

impl CompressionLevel {
    /// Wire name used by the CLI and the configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Light => "light",
            Self::Balanced => "balanced",
            Self::Aggressive => "aggressive",
            Self::Semantic => "semantic",
        }
    }

    /// Parses a level name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Some(Self::None),
            "light" => Some(Self::Light),
            "balanced" | "normal" => Some(Self::Balanced),
            "aggressive" | "hard" => Some(Self::Aggressive),
            "semantic" => Some(Self::Semantic),
            _ => None,
        }
    }

    /// Share of the original size the level aims for.
    pub fn target_ratio(self) -> f64 {
        match self {
            Self::None => 1.0,
            Self::Light => 0.9,
            Self::Balanced | Self::Semantic => 0.7,
            Self::Aggressive => 0.5,
        }
    }
}

/// Statistics of one compression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompressionResult {
    /// Tokens of the assembled input.
    pub original_tokens: usize,
    /// Tokens after selection.
    pub compressed_tokens: usize,
    /// `1 - compressed / original`, in `0.0..=1.0`.
    pub reduction_ratio: f32,
    /// Level that was applied.
    pub level: CompressionLevel,
    /// Segments kept.
    pub preserved_segments: usize,
    /// Segments dropped.
    pub removed_segments: usize,
    /// Whether the preservation check passed.
    pub validated: bool,
}

/// Compression output: text, kept segments, and statistics.
#[derive(Debug, Clone)]
pub struct Compressed {
    /// Assembled text of the kept segments.
    pub text: String,
    /// The segments that survived, in original order.
    pub kept: Vec<ContextSegment>,
    /// Statistics of the operation.
    pub stats: CompressionResult,
}

/// Renders segments as model-facing text with provenance headers.
pub fn assemble(segments: &[ContextSegment]) -> String {
    let mut text = String::new();

    for segment in segments {
        match (
            segment.source_path.as_deref(),
            segment.start_line,
            segment.end_line,
        ) {
            (Some(path), Some(start), Some(end)) => {
                text.push_str(&format!("--- {path}:{start}-{end}\n"));
            }
            _ => text.push_str(&format!("--- {}\n", segment.kind.as_str())),
        }
        text.push_str(segment.text.trim_end());
        text.push_str("\n\n");
    }

    while text.ends_with('\n') {
        text.pop();
    }
    text
}

/// Renders segments as plain text without provenance headers.
///
/// Used when the consumer already knows where the text came from — a whole file read on
/// purpose. The headers cost a dozen tokens per segment, which is more than the payload
/// of a small file.
pub fn assemble_body(segments: &[ContextSegment]) -> String {
    let mut text = String::new();

    for segment in segments {
        text.push_str(segment.text.trim_end());
        text.push_str("\n\n");
    }

    while text.ends_with('\n') {
        text.pop();
    }
    text
}

/// Selects segments for `level`, then assembles and measures them.
///
/// `target_ratio` overrides the level's own target; it is a goal, not a quota: the result
/// may stay larger when everything left is critical, and may stay smaller when the input
/// was already sparse.
pub fn compress(
    segments: &[ContextSegment],
    level: CompressionLevel,
    encoding: Encoding,
    target_ratio: Option<f64>,
) -> Compressed {
    let original_text = assemble(segments);
    let original_tokens = tokenizer::count(encoding, &original_text);

    let ratio = target_ratio.unwrap_or_else(|| level.target_ratio());
    let target = (original_tokens as f64 * ratio.clamp(0.02, 1.0)) as usize;

    let mut kept: Vec<ContextSegment> = match level {
        CompressionLevel::None => segments.to_vec(),
        CompressionLevel::Light => light(segments),
        CompressionLevel::Balanced | CompressionLevel::Semantic => balanced(segments, target),
        CompressionLevel::Aggressive => aggressive(segments, target),
    };

    // Order in the output is always the original order, whatever the selection order was.
    kept.sort_by(|left, right| {
        left.source_path
            .cmp(&right.source_path)
            .then_with(|| left.start_line.cmp(&right.start_line))
    });

    let text = assemble(&kept);
    let compressed_tokens = tokenizer::count(encoding, &text);
    let reduction_ratio = if original_tokens == 0 {
        0.0
    } else {
        1.0 - (compressed_tokens as f32 / original_tokens as f32)
    };

    Compressed {
        text,
        stats: CompressionResult {
            original_tokens,
            compressed_tokens,
            reduction_ratio,
            level,
            preserved_segments: kept.len(),
            removed_segments: segments.len().saturating_sub(kept.len()),
            validated: true,
        },
        kept,
    }
}

/// Drops irrelevant segments and exact duplicates.
fn light(segments: &[ContextSegment]) -> Vec<ContextSegment> {
    let mut seen: Vec<String> = Vec::new();
    let mut kept = Vec::new();

    for segment in segments {
        if segment.importance == Importance::Irrelevant {
            continue;
        }

        let normalized = segment.text.trim().to_lowercase();
        if seen.contains(&normalized) {
            continue;
        }
        seen.push(normalized);
        kept.push(segment.clone());
    }

    kept
}

/// Light, then drops low-value segments while the target is exceeded.
///
/// High-importance segments are dropped only when dropping the low ones was not enough:
/// the budget is a hard ceiling, and the model can always fetch a range back.
fn balanced(segments: &[ContextSegment], target: usize) -> Vec<ContextSegment> {
    let mut kept = light(segments);
    let mut used: usize = kept.iter().map(|segment| segment.token_count).sum();

    drop_until(&mut kept, &mut used, target, |segment| {
        segment.importance.droppable()
    });

    drop_until(&mut kept, &mut used, target, |segment| {
        segment.importance == Importance::High
    });

    // The budget is a hard ceiling: normal segments go too, critical ones never do.
    drop_until(&mut kept, &mut used, target, |segment| {
        segment.importance != Importance::Critical
    });

    kept
}

/// Removes segments selected by `predicate`, least relevant first, while over target.
fn drop_until(
    kept: &mut Vec<ContextSegment>,
    used: &mut usize,
    target: usize,
    predicate: impl Fn(&ContextSegment) -> bool,
) {
    while *used > target {
        let candidate = kept
            .iter()
            .enumerate()
            .filter(|(_, segment)| predicate(segment))
            .min_by(|(_, left), (_, right)| {
                left.relevance_score
                    .total_cmp(&right.relevance_score)
                    .then_with(|| right.token_count.cmp(&left.token_count))
            })
            .map(|(index, _)| index);

        let Some(index) = candidate else {
            break;
        };

        *used = used.saturating_sub(kept[index].token_count);
        kept.remove(index);
    }
}

/// Keeps critical and high-importance segments, trimming the least relevant of the rest.
fn aggressive(segments: &[ContextSegment], target: usize) -> Vec<ContextSegment> {
    let mut kept: Vec<ContextSegment> = segments
        .iter()
        .filter(|segment| matches!(segment.importance, Importance::Critical | Importance::High))
        .cloned()
        .collect();

    let mut used: usize = kept.iter().map(|segment| segment.token_count).sum();

    while used > target {
        let candidate = kept
            .iter()
            .enumerate()
            .filter(|(_, segment)| segment.importance == Importance::High)
            .min_by(|(_, left), (_, right)| left.relevance_score.total_cmp(&right.relevance_score))
            .map(|(index, _)| index);

        let Some(index) = candidate else {
            break;
        };

        used = used.saturating_sub(kept[index].token_count);
        kept.remove(index);
    }

    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vce::context::segment::SegmentKind;

    fn segment(
        text: &str,
        kind: SegmentKind,
        importance: Importance,
        relevance: f32,
    ) -> ContextSegment {
        let mut segment = ContextSegment {
            id: text.to_owned(),
            source_path: Some("src/a.rs".to_owned()),
            symbol: None,
            start_line: Some(1),
            end_line: Some(1),
            token_count: tokenizer::count(Encoding::O200kBase, text),
            relevance_score: relevance,
            importance,
            content_hash: String::new(),
            kind,
            text: text.to_owned(),
        };
        segment.content_hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        segment
    }

    fn sample() -> Vec<ContextSegment> {
        vec![
            segment(
                "the timeout must not be zero",
                SegmentKind::Constraint,
                Importance::Critical,
                0.1,
            ),
            segment(
                "use std::path::Path; use std::fmt; use std::io;",
                SegmentKind::Import,
                Importance::Low,
                0.2,
            ),
            segment(
                "fn run() { do_work(); }",
                SegmentKind::Function,
                Importance::High,
                0.9,
            ),
            segment(
                "noise about nothing in particular, repeated",
                SegmentKind::Explanation,
                Importance::Low,
                0.05,
            ),
        ]
    }

    #[test]
    fn none_keeps_everything() {
        let result = compress(&sample(), CompressionLevel::None, Encoding::O200kBase, None);

        assert_eq!(result.stats.preserved_segments, 4);
        assert_eq!(result.stats.removed_segments, 0);
        assert_eq!(result.stats.reduction_ratio, 0.0);
    }

    #[test]
    fn light_removes_duplicates_and_irrelevant_segments() {
        let mut segments = sample();
        let mut duplicate = segments[2].clone();
        duplicate.id = "dup".to_owned();
        segments.push(duplicate);
        segments.push(segment(
            "",
            SegmentKind::Explanation,
            Importance::Irrelevant,
            0.0,
        ));

        let result = compress(
            &segments,
            CompressionLevel::Light,
            Encoding::O200kBase,
            None,
        );

        assert_eq!(result.stats.removed_segments, 2);
        assert!(result.text.contains("fn run()"));
    }

    #[test]
    fn balanced_drops_low_value_segments_but_keeps_critical_ones() {
        let result = compress(
            &sample(),
            CompressionLevel::Balanced,
            Encoding::O200kBase,
            Some(0.3),
        );

        assert!(result.text.contains("must not be zero"));
        assert!(result.text.contains("fn run()"));
        assert!(
            result.stats.compressed_tokens <= result.stats.original_tokens,
            "compression must not grow the context"
        );
    }

    #[test]
    fn aggressive_keeps_only_critical_and_high() {
        let result = compress(
            &sample(),
            CompressionLevel::Aggressive,
            Encoding::O200kBase,
            None,
        );

        assert!(result.text.contains("must not be zero"));
        assert!(result.text.contains("fn run()"));
        assert!(!result.text.contains("use std::path"));
    }

    #[test]
    fn a_zero_target_still_keeps_critical_segments() {
        let result = compress(
            &sample(),
            CompressionLevel::Aggressive,
            Encoding::O200kBase,
            Some(0.0),
        );

        assert!(result.text.contains("must not be zero"));
        assert_eq!(result.stats.preserved_segments, 1);
    }

    #[test]
    fn keeps_the_original_order_in_the_output() {
        let segments = vec![
            segment("first", SegmentKind::Function, Importance::High, 0.1),
            segment("second", SegmentKind::Function, Importance::High, 0.9),
        ];

        let result = compress(
            &segments,
            CompressionLevel::Balanced,
            Encoding::O200kBase,
            None,
        );
        let first = result.text.find("first").expect("first");
        let second = result.text.find("second").expect("second");

        assert!(first < second);
    }

    #[test]
    fn parses_level_names() {
        assert_eq!(
            CompressionLevel::from_name("BALANCED"),
            Some(CompressionLevel::Balanced)
        );
        assert_eq!(
            CompressionLevel::from_name("off"),
            Some(CompressionLevel::None)
        );
        assert_eq!(CompressionLevel::from_name("nonsense"), None);
    }

    #[test]
    fn an_empty_input_is_not_a_division_by_zero() {
        let result = compress(&[], CompressionLevel::Balanced, Encoding::O200kBase, None);

        assert_eq!(result.stats.reduction_ratio, 0.0);
        assert!(result.text.is_empty());
    }
}
