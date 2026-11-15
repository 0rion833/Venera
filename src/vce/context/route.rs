//! Adaptive routing: classify a request and pick the cheapest sufficient strategy
//! (`TOOL.md` extension §3).
//!
//! The decision is local and heuristic. No model is called to classify: a classifier that
//! costs a request would defeat the purpose of the optimisation.
//!
//! Cost-awareness matters more than any single rule. A project map is a fixed 1000 tokens;
//! reading one named file costs whatever that file costs. The router estimates both and
//! takes the cheaper one that is still sufficient, with sufficiency expressed as
//! confidence rather than left implicit.

use serde::{Deserialize, Serialize};

use crate::config::Encoding;
use crate::provider::tokenizer;
use crate::vce::index::map::MapOptions;
use crate::vce::index::query::Engine;
use crate::vce::index::query::EngineError;
use crate::vce::index::text;

/// What the request is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestScope {
    /// One named file.
    SingleFile,
    /// One named symbol.
    SingleSymbol,
    /// Several files or modules.
    MultiFile,
    /// The project as a whole.
    ProjectWide,
    /// Text the user supplied, with no project reference.
    UserProvidedText,
    /// Not enough signal to decide.
    Unknown,
}

impl RequestScope {
    /// Short name for reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SingleFile => "single-file",
            Self::SingleSymbol => "single-symbol",
            Self::MultiFile => "multi-file",
            Self::ProjectWide => "project-wide",
            Self::UserProvidedText => "user-text",
            Self::Unknown => "unknown",
        }
    }
}

/// What the user wants to do with the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationType {
    /// Read the code.
    Read,
    /// Explain how it works.
    Explain,
    /// Find something.
    Search,
    /// Change something.
    Modify,
    /// Reason about structure.
    Analyze,
    /// Compare two things.
    Compare,
}

impl OperationType {
    /// Short name for reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Explain => "explain",
            Self::Search => "search",
            Self::Modify => "modify",
            Self::Analyze => "analyze",
            Self::Compare => "compare",
        }
    }

    /// Ceiling on one turn's reply for this kind of request, thinking included.
    ///
    /// Reading and searching need a tool call and a line reference; only modification has
    /// to carry a patch, and it is the one place where a big reply is legitimate. The
    /// numbers are ceilings, not targets: a turn that finishes in 40 tokens pays 40.
    pub fn max_output_tokens(self) -> u64 {
        match self {
            Self::Read | Self::Search => 200,
            Self::Explain | Self::Compare => 400,
            Self::Analyze => 600,
            Self::Modify => 1_200,
        }
    }
}

/// How to gather context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionStrategy {
    /// Read one file in full.
    DirectRead,
    /// Read a symbol or a line range.
    PartialRead,
    /// Search the index for whatever matters.
    IndexedRetrieval,
    /// Render the project map.
    ProjectMap,
    /// Map plus a known entry point and its dependencies.
    Hybrid,
}

impl ExecutionStrategy {
    /// Short name used by the CLI and the JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectRead => "direct-read",
            Self::PartialRead => "partial-read",
            Self::IndexedRetrieval => "indexed-retrieval",
            Self::ProjectMap => "project-map",
            Self::Hybrid => "hybrid",
        }
    }

    /// Parses a strategy name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "direct-read" | "direct" | "read" => Some(Self::DirectRead),
            "partial-read" | "partial" | "range" => Some(Self::PartialRead),
            "indexed-retrieval" | "indexed" | "search" => Some(Self::IndexedRetrieval),
            "project-map" | "map" => Some(Self::ProjectMap),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
        }
    }
}

/// What the classifier decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// Scope of the request.
    pub scope: RequestScope,
    /// Operation requested.
    pub operation: OperationType,
    /// Strategy the rules suggest before cost comparison.
    pub strategy: ExecutionStrategy,
    /// Explicit project path, when one was named.
    pub path: Option<String>,
    /// Explicit symbol, when one was named.
    pub symbol: Option<String>,
    /// Normalized query tokens.
    pub tokens: Vec<String>,
}

/// Estimated cost of one strategy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrategyEstimate {
    /// Strategy this estimate belongs to.
    pub strategy: ExecutionStrategy,
    /// Tokens the retrieval itself returns.
    pub retrieval_tokens: usize,
    /// Tokens spent finding it (map rendering, hashing, range extraction).
    pub preparation_cost: usize,
    /// Tokens the answer itself is expected to need.
    pub expected_output_tokens: usize,
    /// How likely this is to be sufficient, in `0.0..=1.0`.
    pub confidence: f32,
}

impl StrategyEstimate {
    /// Total input cost of the strategy.
    pub fn total_tokens(&self) -> usize {
        self.retrieval_tokens + self.preparation_cost
    }
}

/// Classification plus the estimate that won.
#[derive(Debug, Clone)]
pub struct Chosen {
    /// The classification.
    pub classified: Classified,
    /// The estimate that won.
    pub estimate: StrategyEstimate,
    /// Other estimates, for reports and debugging.
    pub considered: Vec<StrategyEstimate>,
}

/// Confidence below which a strategy is considered insufficient regardless of price.
const MIN_CONFIDENCE: f32 = 0.5;

/// Classifies `input` without touching the index.
pub fn classify(input: &str) -> Classified {
    let lower = input.to_lowercase();
    let tokens = text::name_counts(&lower).into_keys().collect::<Vec<_>>();
    let paths = explicit_paths(input);
    let symbol = explicit_symbol(input, &paths);
    let operation = detect_operation(&lower);

    let scope = if paths.len() > 1 {
        RequestScope::MultiFile
    } else if !paths.is_empty() {
        RequestScope::SingleFile
    } else if symbol.is_some() {
        RequestScope::SingleSymbol
    } else if operation == OperationType::Analyze || mentions_architecture(&lower) {
        RequestScope::ProjectWide
    } else if is_long_prose(input) {
        RequestScope::UserProvidedText
    } else {
        RequestScope::Unknown
    };

    let strategy = match (scope, operation) {
        (RequestScope::SingleSymbol, _) => ExecutionStrategy::PartialRead,
        (RequestScope::SingleFile, _) => ExecutionStrategy::DirectRead,
        (RequestScope::MultiFile, _) => ExecutionStrategy::Hybrid,
        (RequestScope::ProjectWide, _) => ExecutionStrategy::ProjectMap,
        _ => ExecutionStrategy::IndexedRetrieval,
    };

    Classified {
        scope,
        operation,
        strategy,
        path: paths.first().cloned(),
        symbol,
        tokens,
    }
}

/// Classifies, estimates every candidate, and returns the cheapest sufficient one.
pub fn choose(
    engine: &Engine,
    input: &str,
    max_tokens: usize,
    encoding: Encoding,
) -> Result<Chosen, EngineError> {
    let classified = classify(input);
    let mut considered = Vec::new();

    if let Some(path) = &classified.path
        && engine.store().file(path)?.is_some()
    {
        let whole = engine.source(path, None, None)?;
        let tokens = tokenizer::count(encoding, &whole.text);

        considered.push(StrategyEstimate {
            strategy: ExecutionStrategy::DirectRead,
            retrieval_tokens: tokens,
            preparation_cost: 0,
            expected_output_tokens: tokens / 2,
            confidence: if classified.scope == RequestScope::SingleFile {
                0.95
            } else {
                0.6
            },
        });

        if let Some(range) = best_range(engine, &classified, encoding)? {
            considered.push(StrategyEstimate {
                strategy: ExecutionStrategy::PartialRead,
                retrieval_tokens: range,
                preparation_cost: 0,
                expected_output_tokens: range / 2,
                confidence: 0.85,
            });
        }
    } else if let Some(range) = best_range(engine, &classified, encoding)? {
        considered.push(StrategyEstimate {
            strategy: ExecutionStrategy::PartialRead,
            retrieval_tokens: range,
            preparation_cost: 0,
            expected_output_tokens: range / 2,
            confidence: 0.8,
        });
    }

    let map_budget = max_tokens.min(1_000);
    let map_tokens = engine
        .map(
            &MapOptions {
                max_tokens: map_budget,
                ..MapOptions::default()
            },
            None,
        )?
        .tokens;

    considered.push(StrategyEstimate {
        strategy: ExecutionStrategy::ProjectMap,
        retrieval_tokens: map_tokens,
        preparation_cost: 0,
        expected_output_tokens: map_tokens / 2,
        confidence: if classified.scope == RequestScope::ProjectWide {
            0.9
        } else {
            0.3
        },
    });

    considered.push(StrategyEstimate {
        strategy: ExecutionStrategy::IndexedRetrieval,
        retrieval_tokens: map_tokens,
        preparation_cost: 0,
        expected_output_tokens: map_tokens / 2,
        confidence: match classified.scope {
            RequestScope::Unknown | RequestScope::MultiFile => 0.65,
            RequestScope::UserProvidedText => 0.4,
            _ => 0.55,
        },
    });

    considered.push(StrategyEstimate {
        strategy: ExecutionStrategy::Hybrid,
        retrieval_tokens: map_tokens.saturating_add(map_tokens / 2),
        preparation_cost: 0,
        expected_output_tokens: map_tokens,
        confidence: if classified.scope == RequestScope::MultiFile {
            0.75
        } else {
            0.35
        },
    });

    let chosen = considered
        .iter()
        .filter(|estimate| estimate.confidence >= MIN_CONFIDENCE)
        .min_by(|left, right| {
            left.total_tokens()
                .cmp(&right.total_tokens())
                .then_with(|| right.confidence.total_cmp(&left.confidence))
        })
        .copied()
        .unwrap_or_else(|| {
            considered
                .iter()
                .max_by(|left, right| left.confidence.total_cmp(&right.confidence))
                .copied()
                .unwrap_or(StrategyEstimate {
                    strategy: ExecutionStrategy::IndexedRetrieval,
                    retrieval_tokens: 0,
                    preparation_cost: 0,
                    expected_output_tokens: 0,
                    confidence: 0.0,
                })
        });

    Ok(Chosen {
        classified,
        estimate: chosen,
        considered,
    })
}

/// Tokens of the best matching symbol range, if the request names a symbol.
fn best_range(
    engine: &Engine,
    classified: &Classified,
    encoding: Encoding,
) -> Result<Option<usize>, EngineError> {
    let Some(symbol) = &classified.symbol else {
        return Ok(None);
    };

    let hits = engine.search_symbols(symbol, 5)?;
    let Some(hit) = hits.first() else {
        return Ok(None);
    };

    let view = engine.source(&hit.path, Some(hit.start_line), Some(hit.end_line))?;
    Ok(Some(tokenizer::count(encoding, &view.text)))
}

/// Project paths named in the request.
fn explicit_paths(input: &str) -> Vec<String> {
    let mut paths = Vec::new();

    for word in input.split_whitespace() {
        let cleaned = word.trim_matches(|ch: char| {
            matches!(
                ch,
                ',' | ';' | ':' | '(' | ')' | '[' | ']' | '"' | '\'' | '`' | '.'
            )
        });
        let cleaned = if cleaned.is_empty() { word } else { cleaned };

        if looks_like_path(cleaned) && !paths.iter().any(|existing| existing == cleaned) {
            paths.push(cleaned.trim_matches('.').to_owned());
        }
    }

    paths
}

fn looks_like_path(word: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        ".rs", ".js", ".mjs", ".cjs", ".ts", ".tsx", ".jsx", ".css", ".scss", ".html", ".htm",
        ".md", ".json", ".toml", ".yaml", ".yml", ".py", ".go", ".c", ".h", ".cpp",
    ];

    (word.contains('/') && EXTENSIONS.iter().any(|extension| word.ends_with(extension)))
        || (word.starts_with("src/") || word.starts_with("./"))
        || EXTENSIONS.iter().any(|extension| word.ends_with(extension))
}

/// A symbol named with backticks, `name()`, or after a declaration keyword.
fn explicit_symbol(input: &str, paths: &[String]) -> Option<String> {
    for span in input.split('`').skip(1).step_by(2) {
        let span = span.trim();
        let candidate = span.trim_end_matches("()");
        if candidate.chars().count() >= 3
            && candidate.chars().any(|ch| ch.is_alphabetic())
            && !looks_like_path(candidate)
            && !candidate.contains(' ')
        {
            return Some(candidate.to_owned());
        }
    }

    for word in input.split_whitespace() {
        let bare =
            word.trim_matches(|ch: char| matches!(ch, ',' | ';' | ':' | '.' | '"' | '\'' | '`'));
        let cleaned = bare.trim_matches(['(', ')']);

        if paths.iter().any(|path| path == cleaned) {
            continue;
        }
        if bare.ends_with("()") && bare.len() > 3 {
            return Some(bare.trim_end_matches("()").to_owned());
        }
    }

    None
}

fn detect_operation(lower: &str) -> OperationType {
    const ANALYZE: &[&str] = &[
        "analyze",
        "architecture",
        "design",
        "проанализируй",
        "архитектур",
    ];
    const COMPARE: &[&str] = &["compare", "difference", "versus", "сравни", "разниц"];
    const MODIFY: &[&str] = &[
        "fix",
        "change",
        "add",
        "remove",
        "refactor",
        "implement",
        "update",
        "rename",
        "исправь",
        "измени",
        "добавь",
        "удали",
        "рефактор",
        "реализуй",
        "обнови",
        "почини",
    ];
    const EXPLAIN: &[&str] = &[
        "explain",
        "why",
        "how does",
        "how do",
        "what is",
        "объясни",
        "почему",
        "как работает",
        "что такое",
    ];
    const SEARCH: &[&str] = &["find", "where", "search", "locate", "найди", "где", "поиск"];

    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));

    if has(ANALYZE) {
        OperationType::Analyze
    } else if has(COMPARE) {
        OperationType::Compare
    } else if has(MODIFY) {
        OperationType::Modify
    } else if has(EXPLAIN) {
        OperationType::Explain
    } else if has(SEARCH) {
        OperationType::Search
    } else {
        OperationType::Read
    }
}

fn mentions_architecture(lower: &str) -> bool {
    const WORDS: &[&str] = &[
        "module",
        "modules",
        "structure",
        "overview",
        "модул",
        "структур",
        "обзор",
    ];
    WORDS.iter().any(|word| lower.contains(word))
}

/// Long prose without code references is a specification, not a navigation task.
fn is_long_prose(input: &str) -> bool {
    input.len() > 400 && !input.contains("```")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vce::index::query::Engine;
    use crate::vce::index::refresh::{RefreshOptions, refresh};

    fn engine(files: &[(&str, &str)]) -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, contents).expect("write");
        }

        let engine = Engine::open(dir.path(), ".venera/index.db").expect("engine");
        refresh(engine.store(), dir.path(), &RefreshOptions::default()).expect("refresh");
        (dir, engine)
    }

    fn site() -> (tempfile::TempDir, Engine) {
        engine(&[
            (
                "src/main.rs",
                "use crate::auth;\nfn main() { auth::run(); }\n",
            ),
            (
                "src/auth.rs",
                "pub fn authenticate(token: &str) -> bool {\n    token == \"secret\"\n}\n\npub fn logout() {}\n",
            ),
            ("src/db.rs", "pub fn connect() -> u32 { 0 }\n"),
        ])
    }

    #[test]
    fn an_explicit_path_is_a_single_file_read() {
        let classified = classify("Read src/auth.rs and explain the guard");

        assert_eq!(classified.scope, RequestScope::SingleFile);
        assert_eq!(classified.strategy, ExecutionStrategy::DirectRead);
        assert_eq!(classified.path.as_deref(), Some("src/auth.rs"));
    }

    #[test]
    fn an_explicit_symbol_is_a_partial_read() {
        let classified = classify("explain `authenticate`");

        assert_eq!(classified.scope, RequestScope::SingleSymbol);
        assert_eq!(classified.strategy, ExecutionStrategy::PartialRead);
        assert_eq!(classified.symbol.as_deref(), Some("authenticate"));
    }

    #[test]
    fn a_bare_symbol_with_parentheses_is_detected() {
        let classified = classify("why does connect() return zero");

        assert_eq!(classified.symbol.as_deref(), Some("connect"));
        assert_eq!(classified.operation, OperationType::Explain);
    }

    #[test]
    fn an_architecture_question_asks_for_the_map() {
        let classified = classify("Analyze the architecture of the modules");

        assert_eq!(classified.scope, RequestScope::ProjectWide);
        assert_eq!(classified.strategy, ExecutionStrategy::ProjectMap);
        assert_eq!(classified.operation, OperationType::Analyze);
    }

    #[test]
    fn a_russian_modification_request_is_modify() {
        let classified = classify("исправь ошибку в src/auth.rs");

        assert_eq!(classified.operation, OperationType::Modify);
        assert_eq!(classified.path.as_deref(), Some("src/auth.rs"));
    }

    #[test]
    fn long_prose_without_code_is_user_text() {
        let prose = "Мы хотим, чтобы сервис ".to_owned()
            + &"работал надёжно и предсказуемо. ".repeat(30)
            + "Нельзя терять данные.";

        let classified = classify(&prose);
        assert_eq!(classified.scope, RequestScope::UserProvidedText);
    }

    #[test]
    fn an_unknown_request_falls_back_to_indexed_retrieval() {
        let classified = classify("where is the button handler");

        assert_eq!(classified.scope, RequestScope::Unknown);
        assert_eq!(classified.strategy, ExecutionStrategy::IndexedRetrieval);
    }

    #[test]
    fn a_direct_read_wins_on_cost_for_a_named_small_file() {
        let (_dir, engine) = site();

        let chosen =
            choose(&engine, "read src/auth.rs", 1000, Encoding::O200kBase).expect("choose");

        assert_eq!(chosen.estimate.strategy, ExecutionStrategy::DirectRead);
        assert!(
            chosen.estimate.total_tokens() < 1000,
            "a small file must cost less than the map: {:?}",
            chosen.estimate
        );
    }

    #[test]
    fn the_map_wins_for_a_project_question() {
        let (dir, engine) = site();
        // Make the project big enough that reading it all would cost more than the map.
        for index in 0..40 {
            std::fs::write(
                dir.path().join(format!("src/module_{index}.rs")),
                "pub fn run() {}\n",
            )
            .expect("write");
        }
        refresh(engine.store(), dir.path(), &RefreshOptions::default()).expect("refresh");

        let chosen = choose(
            &engine,
            "analyze the architecture of the modules",
            1000,
            Encoding::O200kBase,
        )
        .expect("choose");

        assert_eq!(chosen.estimate.strategy, ExecutionStrategy::ProjectMap);
    }

    #[test]
    fn every_candidate_is_reported() {
        let (_dir, engine) = site();

        let chosen =
            choose(&engine, "explain src/auth.rs", 1000, Encoding::O200kBase).expect("choose");

        let strategies: Vec<ExecutionStrategy> = chosen
            .considered
            .iter()
            .map(|estimate| estimate.strategy)
            .collect();
        assert!(strategies.contains(&ExecutionStrategy::DirectRead));
        assert!(strategies.contains(&ExecutionStrategy::ProjectMap));
        assert!(strategies.contains(&ExecutionStrategy::IndexedRetrieval));
        assert!(strategies.contains(&ExecutionStrategy::Hybrid));
    }

    #[test]
    fn modification_gets_the_largest_reply_ceiling() {
        let modify = OperationType::Modify.max_output_tokens();

        for operation in [
            OperationType::Read,
            OperationType::Search,
            OperationType::Explain,
            OperationType::Analyze,
            OperationType::Compare,
        ] {
            assert!(
                operation.max_output_tokens() < modify,
                "{} should be capped below modify",
                operation.as_str()
            );
        }

        assert_eq!(OperationType::Modify.max_output_tokens(), 1_200);
    }

    #[test]
    fn reading_and_searching_get_the_smallest_ceiling() {
        assert_eq!(OperationType::Read.max_output_tokens(), 200);
        assert_eq!(OperationType::Search.max_output_tokens(), 200);
        assert_eq!(OperationType::Explain.max_output_tokens(), 400);
    }
}
