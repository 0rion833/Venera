//! Context distributor (`TOOL.md` extension §9): one call from a request to ready context.
//!
//! The pipeline is: classify → choose a strategy by cost → retrieve → segment → score →
//! compress → validate → assemble → cache. It does not depend on an agent loop and makes
//! no model calls, so it can be measured and tested on its own.
//!
//! Two rules keep it honest:
//!
//! - a strategy is only chosen when it is expected to be *sufficient*, not merely cheap;
//! - compression is reverted when the preservation check fails, and skipped when it saves
//!   almost nothing.

use serde::{Deserialize, Serialize};

use crate::config::Encoding;
use crate::core::budget::{Committed, ContextBudget};
use crate::provider::tokenizer;
use crate::vce::cache::{CacheManager, cache_key};
use crate::vce::context::compress::{self, CompressionLevel, CompressionResult};
use crate::vce::context::route::{self, Chosen, ExecutionStrategy};
use crate::vce::context::segment::{self, ContextSegment, Importance, SegmentKind};
use crate::vce::context::validate;
use crate::vce::index::m1::StoreError;
use crate::vce::index::map::MapOptions;
use crate::vce::index::query::{Engine, EngineError};

/// A request for prepared context.
#[derive(Debug, Clone)]
pub struct ContextRequest {
    /// The user's input.
    pub input: String,
    /// Hard token ceiling for the prepared context.
    pub max_tokens: usize,
    /// How hard to compress.
    pub compression: CompressionLevel,
    /// Forces a strategy; `None` lets the router decide.
    pub strategy: Option<ExecutionStrategy>,
}

impl ContextRequest {
    /// A request with the default compression level.
    pub fn new(input: impl Into<String>, max_tokens: usize) -> Self {
        Self {
            input: input.into(),
            max_tokens,
            compression: CompressionLevel::Balanced,
            strategy: None,
        }
    }
}

/// Pseudo-source marking a context that embeds the project map.
///
/// It never reaches [`ContextOutput::sources`]: `prepare` strips it and records
/// [`ContextOutput::map_included`] instead.
pub const MAP_SOURCE: &str = "<project-map>";

/// Prepared context and the accounting behind it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextOutput {
    /// Assembled context.
    pub content: String,
    /// Tokens before compression.
    pub original_tokens: usize,
    /// Tokens handed to the consumer.
    pub final_tokens: usize,
    /// Served from cache.
    pub cache_hits: usize,
    /// Rebuilt from scratch.
    pub cache_misses: usize,
    /// Strategy that produced it.
    pub strategy: ExecutionStrategy,
    /// Compression statistics.
    pub compression: CompressionResult,
    /// Segments kept.
    pub segments: usize,
    /// Files the context came from.
    pub sources: Vec<String>,
    /// True when the context contains the project map.
    ///
    /// The map is derived from the whole project, so an entry containing it cannot be
    /// validated against its listed sources alone: changing a file outside them changes
    /// the map too. Such entries are generation-bound in the cache instead.
    #[serde(default)]
    pub map_included: bool,
    /// Tokens the preservation check could not find; empty when it passed.
    pub validation_missing: Vec<String>,
    /// True when the final context exceeds the requested token budget after fallbacks.
    pub budget_exceeded: bool,
}

/// A distributor failure.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// The engine failed.
    #[error(transparent)]
    Engine(#[from] EngineError),

    /// The database failed.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// A cached entry could not be encoded.
    #[error("cannot encode the prepared context: {0}")]
    Json(#[from] serde_json::Error),
}

/// Anything that can turn a request into context.
pub trait ContextProvider {
    /// Prepares context for `request`.
    fn prepare(
        &mut self,
        engine: &Engine,
        request: &ContextRequest,
    ) -> Result<ContextOutput, ContextError>;
}

/// The default provider: routing, segmentation, compression, and caching.
pub struct Distributor {
    encoding: Encoding,
    budget: ContextBudget,
    cache: CacheManager,
    prefer_partial: bool,
    avoid_map_for_direct: bool,
    allow_expansion: bool,
    validate_output: bool,
    min_savings: f64,
    target_ratio: f64,
}

impl Distributor {
    /// Builds a distributor for a model window.
    pub fn new(encoding: Encoding, window: u64, max_output: Option<u64>) -> Self {
        Self {
            encoding,
            budget: ContextBudget::for_model(window, max_output),
            cache: CacheManager::new(256, 86_400, true),
            prefer_partial: true,
            avoid_map_for_direct: true,
            allow_expansion: true,
            validate_output: true,
            min_savings: 0.1,
            target_ratio: 0.6,
        }
    }

    /// Overrides the cache settings.
    pub fn with_cache(mut self, capacity: usize, ttl_seconds: i64, enabled: bool) -> Self {
        self.cache = CacheManager::new(capacity, ttl_seconds, enabled);
        self
    }

    /// Overrides the distribution rules.
    pub fn with_rules(
        mut self,
        prefer_partial: bool,
        avoid_map_for_direct: bool,
        allow_expansion: bool,
    ) -> Self {
        self.prefer_partial = prefer_partial;
        self.avoid_map_for_direct = avoid_map_for_direct;
        self.allow_expansion = allow_expansion;
        self
    }

    /// Overrides the compression targets.
    pub fn with_compression(mut self, target_ratio: f64, min_savings: f64) -> Self {
        self.target_ratio = target_ratio;
        self.min_savings = min_savings;
        self
    }

    /// Turns the preservation check on or off.
    pub fn with_validation(mut self, validate_output: bool) -> Self {
        self.validate_output = validate_output;
        self
    }

    /// Cache statistics of this distributor.
    pub fn cache(&self) -> &CacheManager {
        &self.cache
    }

    /// Tokens this request may spend on retrieval.
    fn allowance(&self, max_tokens: usize) -> usize {
        let allocation = self.budget.allocate(&Committed::default()).retrieval;
        (allocation.min(max_tokens as u64) as usize).max(1)
    }

    fn choose(&self, engine: &Engine, request: &ContextRequest) -> Result<Chosen, ContextError> {
        let mut chosen = route::choose(engine, &request.input, request.max_tokens, self.encoding)?;

        if let Some(forced) = request.strategy {
            chosen.estimate = route::StrategyEstimate {
                strategy: forced,
                ..chosen.estimate
            };
        }

        // A direct hit on one named file never needs the map: the map would be cheaper
        // only in tokens, not in usefulness, because the file itself is the answer.
        if self.avoid_map_for_direct
            && chosen.classified.scope == route::RequestScope::SingleFile
            && chosen.classified.symbol.is_none()
            && chosen.classified.path.is_some()
            && !matches!(
                chosen.estimate.strategy,
                ExecutionStrategy::DirectRead | ExecutionStrategy::PartialRead
            )
            && let Some(direct) = chosen
                .considered
                .iter()
                .find(|estimate| estimate.strategy == ExecutionStrategy::DirectRead)
        {
            chosen.estimate = *direct;
        }

        // Prefer a range over a whole file when both would do.
        if self.prefer_partial
            && chosen.classified.symbol.is_some()
            && chosen.estimate.strategy == ExecutionStrategy::DirectRead
            && chosen
                .considered
                .iter()
                .any(|estimate| estimate.strategy == ExecutionStrategy::PartialRead)
        {
            chosen.estimate = route::StrategyEstimate {
                strategy: ExecutionStrategy::PartialRead,
                ..chosen.estimate
            };
        }

        Ok(chosen)
    }

    fn retrieve(
        &self,
        engine: &Engine,
        chosen: &Chosen,
        request: &ContextRequest,
    ) -> Result<(Vec<ContextSegment>, Vec<String>), ContextError> {
        if chosen.classified.scope == route::RequestScope::UserProvidedText {
            return Ok((
                segment::from_text(self.encoding, &request.input),
                Vec::new(),
            ));
        }

        match chosen.estimate.strategy {
            ExecutionStrategy::DirectRead => self
                .read_path(engine, chosen.classified.path.as_deref())
                .map(|segments| (segments, chosen.classified.path.iter().cloned().collect())),

            ExecutionStrategy::PartialRead => self.read_symbol(engine, chosen),

            ExecutionStrategy::IndexedRetrieval => self.read_indexed(engine, chosen, request),

            ExecutionStrategy::ProjectMap => {
                let (segments, mut sources) = self.read_map(engine, request, Vec::new())?;
                sources.push(MAP_SOURCE.to_owned());
                Ok((segments, sources))
            }

            ExecutionStrategy::Hybrid => {
                let (mut segments, a) = self.read_indexed(engine, chosen, request)?;
                let (map, b) = self.read_map(engine, request, Vec::new())?;
                let map_included = !map.is_empty();
                segments.extend(map);
                let mut sources = a;
                sources.extend(b);
                if map_included {
                    sources.push(MAP_SOURCE.to_owned());
                }
                Ok((segments, sources))
            }
        }
    }

    fn read_path(
        &self,
        engine: &Engine,
        path: Option<&str>,
    ) -> Result<Vec<ContextSegment>, ContextError> {
        let Some(path) = path else {
            return Ok(Vec::new());
        };

        let view = engine.source(path, None, None)?;
        let symbols = engine
            .store()
            .file(path)?
            .map(|file| engine.store().symbols_of_file(file.id))
            .transpose()?
            .unwrap_or_default();

        Ok(segment::from_source(
            self.encoding,
            path,
            &view.text,
            view.start_line,
            &symbols,
        ))
    }

    fn read_symbol(
        &self,
        engine: &Engine,
        chosen: &Chosen,
    ) -> Result<(Vec<ContextSegment>, Vec<String>), ContextError> {
        let Some(symbol) = chosen.classified.symbol.as_deref() else {
            return Ok((Vec::new(), Vec::new()));
        };

        let hits = engine.search_symbols(symbol, 3)?;
        let Some(hit) = hits.first() else {
            return Ok((Vec::new(), Vec::new()));
        };

        let view = engine.source(&hit.path, Some(hit.start_line), Some(hit.end_line))?;
        let symbols = engine
            .store()
            .file(&hit.path)?
            .map(|file| engine.store().symbols_of_file(file.id))
            .transpose()?
            .unwrap_or_default();

        Ok((
            segment::from_source(
                self.encoding,
                &hit.path,
                &view.text,
                view.start_line,
                &symbols,
            ),
            vec![hit.path.clone()],
        ))
    }

    fn read_indexed(
        &self,
        engine: &Engine,
        chosen: &Chosen,
        request: &ContextRequest,
    ) -> Result<(Vec<ContextSegment>, Vec<String>), ContextError> {
        let query = chosen.classified.tokens.join(" ");
        let hits = if query.is_empty() {
            Vec::new()
        } else {
            engine.search_symbols(&query, 3)?
        };

        if hits.is_empty() {
            // Nothing matched by symbol: a named file is still worth reading.
            if let Some(path) = chosen.classified.path.as_deref() {
                let segments = self.read_path(engine, Some(path))?;
                return Ok((segments, vec![path.to_owned()]));
            }
            return self.read_map(engine, request, Vec::new());
        }

        let mut segments = Vec::new();
        let mut sources = Vec::new();

        for hit in &hits {
            let view = engine.source(&hit.path, Some(hit.start_line), Some(hit.end_line))?;
            let symbols = engine
                .store()
                .file(&hit.path)?
                .map(|file| engine.store().symbols_of_file(file.id))
                .transpose()?
                .unwrap_or_default();

            segments.extend(segment::from_source(
                self.encoding,
                &hit.path,
                &view.text,
                view.start_line,
                &symbols,
            ));

            if !sources.iter().any(|source| source == &hit.path) {
                sources.push(hit.path.clone());
            }
        }

        Ok((segments, sources))
    }

    fn read_map(
        &self,
        engine: &Engine,
        request: &ContextRequest,
        sources: Vec<String>,
    ) -> Result<(Vec<ContextSegment>, Vec<String>), ContextError> {
        let map = engine.map(
            &MapOptions {
                max_tokens: request.max_tokens.min(1_000),
                ..MapOptions::default()
            },
            Some(&request.input),
        )?;

        if map.text.is_empty() {
            return Ok((Vec::new(), sources));
        }

        let text_segment = ContextSegment {
            id: "project-map".to_owned(),
            source_path: None,
            symbol: None,
            start_line: None,
            end_line: None,
            token_count: map.tokens,
            relevance_score: 1.0,
            importance: Importance::High,
            content_hash: blake3::hash(map.text.as_bytes()).to_hex().to_string(),
            kind: SegmentKind::Documentation,
            text: map.text,
        };

        Ok((vec![text_segment], sources))
    }

    /// Scores segments by how many query tokens they contain.
    fn score(segments: &mut [ContextSegment], tokens: &[String]) {
        let total = tokens.len().max(1) as f32;

        for segment in segments {
            let lower = segment.text.to_lowercase();
            let hits = tokens
                .iter()
                .filter(|token| lower.contains(token.as_str()))
                .count() as f32;

            let boost = match segment.importance {
                Importance::Critical => 1.0,
                Importance::High => 0.5,
                Importance::Normal => 0.2,
                Importance::Low => 0.0,
                Importance::Irrelevant => -0.5,
            };

            segment.relevance_score = hits / total + boost;
        }
    }

    /// Stores the prepared context in the cache and returns it unchanged.
    fn finish(
        &mut self,
        engine: &Engine,
        key: &str,
        output: ContextOutput,
    ) -> Result<ContextOutput, ContextError> {
        let encoded = serde_json::to_string(&output)?;
        let generation = engine.store().generation()?;
        // An entry that embeds the project map depends on the whole project, not only on
        // its listed files: record no sources so the generation alone validates it.
        let cache_sources: &[String] = if output.map_included {
            &[]
        } else {
            &output.sources
        };
        self.cache.put(
            engine.store(),
            key,
            &encoded,
            output.final_tokens,
            generation,
            cache_sources,
        )?;
        Ok(output)
    }
}

/// Joins the critical segments of the request: the requirements and constraints stated by
/// the user, which compression must preserve.
fn requirement_text(encoding: Encoding, input: &str) -> String {
    segment::from_text(encoding, input)
        .into_iter()
        .filter(|segment| segment.importance == Importance::Critical)
        .map(|segment| segment.text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl ContextProvider for Distributor {
    fn prepare(
        &mut self,
        engine: &Engine,
        request: &ContextRequest,
    ) -> Result<ContextOutput, ContextError> {
        let chosen = self.choose(engine, request)?;
        let generation = engine.store().generation()?;
        let key = cache_key(
            chosen.estimate.strategy.as_str(),
            &request.input,
            request.max_tokens,
            request.compression.as_str(),
            &crate::vce::cache::CacheFingerprint {
                encoding: self.encoding.as_str(),
                target_ratio: self.target_ratio,
                min_savings: self.min_savings,
                validate_output: self.validate_output,
                allow_expansion: self.allow_expansion,
                prefer_partial: self.prefer_partial,
                avoid_map_for_direct: self.avoid_map_for_direct,
                budget_window: self.budget.window,
                budget_reserve_output: self.budget.reserve_output,
            },
        );

        if let Some((stored, _from_ram)) = self.cache.get(engine.store(), &key, generation)?
            && let Ok(mut cached) = serde_json::from_str::<ContextOutput>(&stored)
        {
            cached.cache_hits = 1;
            cached.cache_misses = 0;
            return Ok(cached);
        }

        let (mut segments, mut sources) = self.retrieve(engine, &chosen, request)?;
        // The map is derived from the whole project, so an entry that embeds it cannot be
        // validated by the listed sources alone: a change anywhere changes the map.
        let map_included = sources.iter().any(|source| source == MAP_SOURCE);
        sources.retain(|source| source != MAP_SOURCE);

        // Retrieval that finds nothing must not produce empty context: the request text
        // itself is then the context worth compressing.
        if segments.is_empty() {
            segments = segment::from_text(self.encoding, &request.input);
        } else if chosen.classified.scope != route::RequestScope::UserProvidedText {
            // Requirements and constraints stated in the request are part of the context
            // no matter what the index returned (`TOOL.md` extension §4.3).
            let critical: Vec<ContextSegment> = segment::from_text(self.encoding, &request.input)
                .into_iter()
                .filter(|segment| segment.importance == Importance::Critical)
                .collect();
            segments.extend(critical);
        }

        Self::score(&mut segments, &chosen.classified.tokens);

        // A whole small file read on purpose needs no provenance headers: they cost more
        // than the payload. Send the raw text when it fits the budget.
        if chosen.estimate.strategy == ExecutionStrategy::DirectRead && !segments.is_empty() {
            let allowance = self.allowance(request.max_tokens);
            let body = compress::assemble_body(&segments);
            let tokens = tokenizer::count(self.encoding, &body);

            if tokens <= allowance {
                let output = ContextOutput {
                    content: body,
                    original_tokens: tokens,
                    final_tokens: tokens,
                    cache_hits: 0,
                    cache_misses: 1,
                    strategy: chosen.estimate.strategy,
                    compression: CompressionResult {
                        original_tokens: tokens,
                        compressed_tokens: tokens,
                        reduction_ratio: 0.0,
                        level: CompressionLevel::None,
                        preserved_segments: segments.len(),
                        removed_segments: 0,
                        validated: true,
                    },
                    segments: segments.len(),
                    sources,
                    map_included: false,
                    validation_missing: Vec::new(),
                    budget_exceeded: false,
                };

                return self.finish(engine, &key, output);
            }
        }

        let assembled = compress::assemble(&segments);
        let original_tokens = tokenizer::count(self.encoding, &assembled);

        let allowance = self.allowance(request.max_tokens);
        let ratio = if original_tokens > allowance && original_tokens > 0 {
            allowance as f64 / original_tokens as f64
        } else if request.compression == CompressionLevel::Aggressive {
            // Aggressive was asked for explicitly: aim for the configured target even
            // when the budget would allow everything.
            self.target_ratio
        } else {
            // Nothing to gain: keep the context as it is rather than "compress" it down.
            1.0
        };

        let mut compressed =
            compress::compress(&segments, request.compression, self.encoding, Some(ratio));

        // Provenance headers are part of the final text but not of the selection, so the
        // first pass can overshoot. Tighten the target until the budget holds.
        if request.compression != CompressionLevel::None {
            let mut attempt_ratio = ratio;
            for _ in 0..3 {
                if compressed.stats.compressed_tokens <= allowance {
                    break;
                }

                let factor = allowance as f64 / compressed.stats.compressed_tokens as f64;
                attempt_ratio *= factor * 0.95;
                let next = compress::compress(
                    &segments,
                    request.compression,
                    self.encoding,
                    Some(attempt_ratio),
                );
                if next.stats.compressed_tokens >= compressed.stats.compressed_tokens {
                    break;
                }
                compressed = next;
            }
        }

        let mut missing = Vec::new();

        if self.validate_output && request.compression != CompressionLevel::None {
            // Only stated requirements are checked: numbers and identifiers inside code
            // that was deliberately dropped are not requirements.
            let requirements = requirement_text(self.encoding, &request.input);

            if !requirements.trim().is_empty() {
                let check = validate::preserve(&requirements, &compressed.text);
                if !check.ok {
                    if self.allow_expansion {
                        let fallback = compress::compress(
                            &segments,
                            CompressionLevel::None,
                            self.encoding,
                            None,
                        );
                        let recheck = validate::preserve(&requirements, &fallback.text);
                        missing = recheck.missing;
                        compressed = fallback;
                        compressed.stats.validated = recheck.ok;
                    } else {
                        missing = check.missing;
                        compressed.stats.validated = false;
                    }
                }
            }
        }

        // Compression that saves almost nothing costs more than it returns.
        if request.compression != CompressionLevel::None
            && compressed.stats.reduction_ratio < self.min_savings as f32
            && !segments.is_empty()
        {
            compressed = compress::compress(&segments, CompressionLevel::None, self.encoding, None);
        }

        // The budget is a hard ceiling; fallbacks above must not silently slip out of it.
        // If a fallback dropped validation, keep the content but report the overflow so the
        // caller can decide (retry with more tokens) instead of trusting an oversized result.
        let final_tokens = compressed.stats.compressed_tokens;
        let exceeds_budget = final_tokens > allowance && !segments.is_empty();

        let output = ContextOutput {
            content: compressed.text.clone(),
            original_tokens,
            final_tokens,
            cache_hits: 0,
            cache_misses: 1,
            strategy: chosen.estimate.strategy,
            compression: compressed.stats,
            segments: compressed.kept.len(),
            sources,
            map_included,
            validation_missing: missing,
            budget_exceeded: exceeds_budget,
        };

        self.finish(engine, &key, output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            ("style.css", ".btn { color: red; }\n"),
        ])
    }

    fn distributor() -> Distributor {
        Distributor::new(Encoding::O200kBase, 1_050_000, Some(131_072))
    }

    #[test]
    fn a_named_file_is_read_directly_without_the_map() {
        let (_dir, engine) = site();
        let mut distributor = distributor();

        let output = distributor
            .prepare(&engine, &ContextRequest::new("read src/auth.rs", 1000))
            .expect("prepare");

        assert_eq!(output.strategy, ExecutionStrategy::DirectRead);
        assert!(output.content.contains("fn authenticate"));
        assert!(
            !output.content.contains("src/main.rs\n"),
            "the map must not be part of a direct read: {}",
            output.content
        );
        assert_eq!(output.sources, vec!["src/auth.rs".to_owned()]);
    }

    #[test]
    fn a_small_file_is_sent_without_segment_headers() {
        let (_dir, engine) = site();
        let mut distributor = distributor();

        let output = distributor
            .prepare(&engine, &ContextRequest::new("read style.css", 1000))
            .expect("prepare");

        assert_eq!(output.strategy, ExecutionStrategy::DirectRead);
        assert!(
            !output.content.contains("--- "),
            "headers must be omitted: {}",
            output.content
        );
        assert!(output.content.contains(".btn"));
        assert_eq!(output.original_tokens, output.final_tokens);
    }

    #[test]
    fn a_named_symbol_returns_only_its_range() {
        let (_dir, engine) = site();
        let mut distributor = distributor();

        let output = distributor
            .prepare(&engine, &ContextRequest::new("explain `logout`", 1000))
            .expect("prepare");

        assert!(output.content.contains("fn logout"));
        assert!(
            !output.content.contains("fn authenticate"),
            "only the range"
        );
    }

    #[test]
    fn a_project_question_uses_the_map() {
        let (dir, engine) = site();
        for index in 0..40 {
            std::fs::write(
                dir.path().join(format!("src/module_{index}.rs")),
                "pub fn run() {}\n",
            )
            .expect("write");
        }
        refresh(engine.store(), dir.path(), &RefreshOptions::default()).expect("refresh");

        let mut distributor = distributor();
        let output = distributor
            .prepare(
                &engine,
                &ContextRequest::new("analyze the architecture of the modules", 1000),
            )
            .expect("prepare");

        assert_eq!(output.strategy, ExecutionStrategy::ProjectMap);
        assert!(output.content.contains("module_"));
    }

    #[test]
    fn a_hybrid_context_that_embeds_the_map_is_generation_bound() {
        let (dir, engine) = site();
        let request = ContextRequest {
            input: "compare src/auth.rs and src/main.rs".to_owned(),
            max_tokens: 2_000,
            compression: CompressionLevel::None,
            strategy: Some(ExecutionStrategy::Hybrid),
        };

        let first = distributor().prepare(&engine, &request).expect("first");
        assert_eq!(first.strategy, ExecutionStrategy::Hybrid);
        assert!(first.map_included, "the map is part of a hybrid answer");

        // A new file changes the map but none of the listed sources.
        std::fs::write(dir.path().join("src/billing.rs"), "pub fn charge() {}\n").expect("write");
        refresh(engine.store(), dir.path(), &RefreshOptions::default()).expect("refresh");

        // A fresh distributor has an empty M0, so any hit would come from stale M1.
        let after = distributor().prepare(&engine, &request).expect("second");

        assert_eq!(
            after.cache_misses, 1,
            "a changed map must not be a cache hit"
        );
        assert!(
            after.content.contains("billing"),
            "the map must be rebuilt with the new file: {}",
            after.content
        );
    }

    #[test]
    fn a_repeated_request_is_served_from_cache() {
        let (_dir, engine) = site();
        let mut distributor = distributor();
        let request = ContextRequest::new("read src/auth.rs", 1000);

        let first = distributor.prepare(&engine, &request).expect("first");
        let second = distributor.prepare(&engine, &request).expect("second");

        assert_eq!(first.cache_misses, 1);
        assert_eq!(second.cache_hits, 1);
        assert_eq!(first.content, second.content);
        assert_eq!(first.final_tokens, second.final_tokens);
    }

    #[test]
    fn a_changed_project_invalidates_the_cache() {
        let (dir, engine) = site();
        let mut distributor = distributor();
        let request = ContextRequest::new("read src/auth.rs", 1000);

        distributor.prepare(&engine, &request).expect("first");

        std::fs::write(
            dir.path().join("src/auth.rs"),
            "pub fn authenticate(token: &str) -> bool {\n    token == \"other\"\n}\n",
        )
        .expect("write");
        refresh(engine.store(), dir.path(), &RefreshOptions::default()).expect("refresh");

        let after = distributor.prepare(&engine, &request).expect("second");

        assert_eq!(after.cache_misses, 1, "the generation changed");
        assert!(after.content.contains("other"));
    }

    #[test]
    fn a_single_file_reindex_invalidates_the_runtime_cache() {
        // `Engine::source` reindexes one changed file without a full project refresh.
        // Before the generation bump that path left the M0 entry matching, so the next
        // identical request served the pre-edit context.
        let (dir, engine) = site();
        let mut distributor = distributor();
        let request = ContextRequest::new("read src/auth.rs", 1000);

        let first = distributor.prepare(&engine, &request).expect("first");
        assert!(first.content.contains("secret"));

        std::fs::write(
            dir.path().join("src/auth.rs"),
            "pub fn authenticate(token: &str) -> bool {\n    token == \"public\"\n}\n\npub fn logout() {}\n",
        )
        .expect("write");

        let view = engine.source("src/auth.rs", None, None).expect("source");
        assert!(view.refreshed, "the changed file must be reindexed");
        assert!(view.text.contains("public"));

        let after = distributor.prepare(&engine, &request).expect("second");

        assert_eq!(after.cache_misses, 1, "a reindexed file must invalidate M0");
        assert!(
            after.content.contains("public"),
            "stale context was served: {}",
            after.content
        );
    }

    #[test]
    fn user_prose_is_segmented_without_touching_the_project() {
        let (_dir, engine) = site();
        let mut distributor = distributor();
        let input =
            "Мы делаем сервис. Нельзя терять данные при перезапуске. Нужно кэшировать сессии.";

        let output = distributor
            .prepare(&engine, &ContextRequest::new(input, 500))
            .expect("prepare");

        assert!(output.sources.is_empty());
        assert!(output.content.contains("Нельзя терять данные"));
        assert!(output.compression.validated);
    }

    #[test]
    fn compression_is_reverted_when_a_constraint_would_be_lost() {
        let (_dir, engine) = site();
        let mut distributor = distributor();
        // One critical line plus a lot of droppable prose.
        let mut input = String::from("The retry limit must be `5` and never 0.\n\n");
        for _ in 0..30 {
            input.push_str("Some narrative that adds little. ");
        }

        let output = distributor
            .prepare(
                &engine,
                &ContextRequest {
                    input,
                    max_tokens: 40,
                    compression: CompressionLevel::Aggressive,
                    strategy: None,
                },
            )
            .expect("prepare");

        assert!(output.content.contains("must be"));
        assert!(output.validation_missing.is_empty());
    }

    #[test]
    fn the_budget_is_respected_when_compression_can_help() {
        let (_dir, engine) = site();
        let mut distributor = distributor();

        let output = distributor
            .prepare(&engine, &ContextRequest::new("read src/auth.rs", 200))
            .expect("prepare");

        assert!(
            output.final_tokens <= 200,
            "final {} > budget 200",
            output.final_tokens
        );
    }

    #[test]
    fn output_round_trips_through_json() {
        let (_dir, engine) = site();
        let mut distributor = distributor();
        let output = distributor
            .prepare(&engine, &ContextRequest::new("read src/auth.rs", 500))
            .expect("prepare");

        let encoded = serde_json::to_string(&output).expect("encode");
        let decoded: ContextOutput = serde_json::from_str(&encoded).expect("decode");

        assert_eq!(decoded.content, output.content);
        assert_eq!(decoded.strategy, output.strategy);
        assert_eq!(decoded.final_tokens, output.final_tokens);
    }
}
