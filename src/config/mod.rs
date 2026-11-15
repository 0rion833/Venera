//! Configuration: JSON config, schema, key resolution (future `venera-config`).
//!
//! Every struct uses `#[serde(deny_unknown_fields)]` — a typo in a field must error,
//! never be silently ignored (`AGENTS.md` §5). Structure changes require a matching update
//! in `schema/config.schema.json`.
//!
//! Load order: `.venera/config.json` in the project, then the user config directory,
//! then the defaults compiled into the binary from `config/default.json`.
//!
//! Prices are RUB per 1M tokens with VAT included, split into a normal tier and an
//! optional long-context tier that applies once the prompt exceeds its threshold.

pub mod keys;
pub mod prices;
pub mod reasoning;
pub mod schema;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use keys::KeySource;
pub use prices::{LongContext, ModelPrice, PriceRates};
pub use reasoning::Reasoning;

/// Defaults shipped inside the binary.
pub const DEFAULT_CONFIG: &str = include_str!("../../config/default.json");

/// Tokenizer encoding used to count tokens for a model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum Encoding {
    /// OpenAI `o200k_base` (GPT-4o/5/6 family).
    #[default]
    #[serde(rename = "o200k_base")]
    O200kBase,
    /// OpenAI `cl100k_base` (GPT-4/3.5 family, legacy).
    #[serde(rename = "cl100k_base")]
    Cl100kBase,
}

impl Encoding {
    /// Wire name of the encoding.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::O200kBase => "o200k_base",
            Self::Cl100kBase => "cl100k_base",
        }
    }
}

/// Top-level configuration document.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    pub vendors: BTreeMap<String, Vendor>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub context: ContextConfig,
}

/// Which vendor and model to use when the CLI does not say otherwise.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// An OpenAI-compatible aggregator (ProxyAPI, Cloud.ru, ...).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Vendor {
    /// Base URL without the chat path, e.g. `https://api.proxyapi.ru/v1`.
    pub base_url: String,
    /// Path appended to `base_url` for chat completions.
    #[serde(default = "default_chat_path")]
    pub chat_path: String,
    /// Where the API key comes from. Absent means "offline only".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<KeySource>,
    pub models: BTreeMap<String, Model>,
}

fn default_chat_path() -> String {
    "/chat/completions".to_owned()
}

impl Vendor {
    /// Full chat-completions URL.
    pub fn chat_url(&self) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), self.chat_path)
    }

    /// Looks a model up by config key, provider id, or the id's last segment.
    pub fn find_model(&self, name: &str) -> Option<(&str, &Model)> {
        if let Some((key, model)) = self.models.iter().find(|(key, model)| {
            key.eq_ignore_ascii_case(name) || model.id.eq_ignore_ascii_case(name)
        }) {
            return Some((key.as_str(), model));
        }

        self.models
            .iter()
            .find(|(_, model)| {
                model
                    .id
                    .rsplit('/')
                    .next()
                    .is_some_and(|tail| tail.eq_ignore_ascii_case(name))
            })
            .map(|(key, model)| (key.as_str(), model))
    }

    /// Known model keys, for error messages.
    pub fn model_keys(&self) -> String {
        self.models.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// A single model with its tokenizer and RUB prices.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// Provider-side id sent in the request, e.g. `openai/gpt-6-luna`.
    pub id: String,
    #[serde(default)]
    pub encoding: Encoding,
    pub context_window: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
    /// Thinking level used when the CLI does not pass `--reasoning`.
    #[serde(default)]
    pub reasoning: Reasoning,
    pub price: ModelPrice,
}

/// Session limits.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Hard cap in RUB; when hit, the agent stops and asks for confirmation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_budget_rub: Option<f64>,
    /// Hard ceiling on one turn's reply, in tokens, thinking included.
    ///
    /// Output is billed at the highest rate in the table (5x input, 50x cache read on
    /// `gpt-6-luna`), so the reply is the part worth capping. `None` leaves the ceiling
    /// to the model's own `max_output`; a value here wins when it is lower.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens_per_turn: Option<u64>,
}

/// Context-engine settings (`TOOL.md` §17).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexConfig {
    /// Files above this size are not indexed, in bytes.
    #[serde(default = "default_max_file_size")]
    pub max_file_size: u64,
    /// Index database, relative to the project root unless absolute.
    #[serde(default = "default_database")]
    pub database: String,
    /// Honour `.gitignore` and `.veneraignore`.
    #[serde(default = "allow_by_default")]
    pub respect_gitignore: bool,
    #[serde(default)]
    pub map: MapDefaults,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            max_file_size: default_max_file_size(),
            database: default_database(),
            respect_gitignore: true,
            map: MapDefaults::default(),
        }
    }
}

fn default_max_file_size() -> u64 {
    1_048_576
}

fn default_database() -> String {
    ".venera/index.db".to_owned()
}

fn allow_by_default() -> bool {
    true
}

/// Defaults for the rendered project map.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MapDefaults {
    /// Token budget when `--max-tokens` is not given.
    #[serde(default = "default_map_tokens")]
    pub default_token_budget: usize,
    /// Symbols shown per file; `0` means unlimited.
    #[serde(default = "default_symbols_per_file")]
    pub max_symbols_per_file: usize,
}

impl Default for MapDefaults {
    fn default() -> Self {
        Self {
            default_token_budget: default_map_tokens(),
            max_symbols_per_file: default_symbols_per_file(),
        }
    }
}

fn default_map_tokens() -> usize {
    1000
}

fn default_symbols_per_file() -> usize {
    40
}

/// Context optimisation settings (`TOOL.md` extension §11).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    /// Master switch: with `false` the distributor skips compression and caching.
    #[serde(default = "allow_by_default")]
    pub enabled: bool,
    /// `adaptive` picks a strategy by cost; any strategy name forces one.
    #[serde(default = "default_strategy")]
    pub default_strategy: String,
    #[serde(default)]
    pub compression: CompressionConfig,
    #[serde(default)]
    pub cache: ContextCacheConfig,
    #[serde(default)]
    pub distribution: DistributionConfig,
    #[serde(default)]
    pub session: SessionConfig,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_strategy: default_strategy(),
            compression: CompressionConfig::default(),
            cache: ContextCacheConfig::default(),
            distribution: DistributionConfig::default(),
            session: SessionConfig::default(),
        }
    }
}

fn default_strategy() -> String {
    "adaptive".to_owned()
}

/// Compression settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompressionConfig {
    /// Whether to compress at all.
    #[serde(default = "allow_by_default")]
    pub enabled: bool,
    /// `none`, `light`, `balanced`, `aggressive`, or `semantic`.
    #[serde(default = "default_level")]
    pub default_level: String,
    /// Share of the original the result should aim for.
    #[serde(default = "default_target_ratio")]
    pub target_retained_ratio: f64,
    /// Below this saving, keep the original: compression must be worth its own cost.
    #[serde(default = "default_min_savings")]
    pub minimum_savings_ratio: f64,
    /// Never drop critical segments, whatever the level.
    #[serde(default = "allow_by_default")]
    pub preserve_critical: bool,
    /// Run the preservation check and fall back when it fails.
    #[serde(default = "allow_by_default")]
    pub validate_output: bool,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_level: default_level(),
            target_retained_ratio: default_target_ratio(),
            minimum_savings_ratio: default_min_savings(),
            preserve_critical: true,
            validate_output: true,
        }
    }
}

fn default_level() -> String {
    "balanced".to_owned()
}

fn default_target_ratio() -> f64 {
    0.6
}

fn default_min_savings() -> f64 {
    0.1
}

/// Cache settings for the context levels.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCacheConfig {
    /// Whether the persistent working cache (M1) is used.
    #[serde(default = "allow_by_default")]
    pub enabled: bool,
    /// Entries kept in RAM (M0); `0` disables the runtime level.
    #[serde(default = "default_m0_entries")]
    pub m0_max_entries: usize,
    /// Seconds an M1 entry stays valid; `0` means no expiry.
    #[serde(default = "default_m1_ttl")]
    pub m1_ttl_seconds: i64,
}

impl Default for ContextCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            m0_max_entries: default_m0_entries(),
            m1_ttl_seconds: default_m1_ttl(),
        }
    }
}

fn default_m0_entries() -> usize {
    256
}

fn default_m1_ttl() -> i64 {
    86_400
}

/// Distribution rules.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DistributionConfig {
    /// Prefer a symbol range over a whole file when both are sufficient.
    #[serde(default = "allow_by_default")]
    pub prefer_partial_source: bool,
    /// Never spend the map budget when the request names a file directly.
    #[serde(default = "allow_by_default")]
    pub avoid_map_for_direct_tasks: bool,
    /// Allow restoring a larger context when validation fails.
    #[serde(default = "allow_by_default")]
    pub allow_context_expansion: bool,
}

impl Default for DistributionConfig {
    fn default() -> Self {
        Self {
            prefer_partial_source: true,
            avoid_map_for_direct_tasks: true,
            allow_context_expansion: true,
        }
    }
}

/// Session lifecycle settings (`prirote.md` §14).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// Drop temporary task context when the task ends.
    #[serde(default = "allow_by_default")]
    pub cleanup_after_task: bool,
    /// Synchronize the index after every task.
    #[serde(default = "allow_by_default")]
    pub sync_after_task: bool,
    /// Re-index only changed files instead of the whole project.
    #[serde(default = "allow_by_default")]
    pub incremental_sync: bool,
    /// Keep valid persistent cache entries when the generation advances.
    #[serde(default = "allow_by_default")]
    pub preserve_persistent_cache: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            cleanup_after_task: true,
            sync_after_task: true,
            incremental_sync: true,
            preserve_persistent_cache: true,
        }
    }
}

/// Where the loaded config came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// `.venera/config.json` inside the project.
    Project(PathBuf),
    /// User-level config directory.
    User(PathBuf),
    /// Compiled-in defaults.
    Builtin,
}

impl ConfigSource {
    /// Human-readable description that never contains secrets.
    pub fn describe(&self) -> String {
        match self {
            Self::Project(path) => format!("project file {}", path.display()),
            Self::User(path) => format!("user file {}", path.display()),
            Self::Builtin => "built-in defaults (compiled in)".to_owned(),
        }
    }
}

/// A config together with the source it was read from.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub source: ConfigSource,
}

/// A vendor/model pair resolved from CLI arguments and config defaults.
#[derive(Debug, Clone, Copy)]
pub struct Resolved<'a> {
    pub vendor_name: &'a str,
    pub vendor: &'a Vendor,
    pub model_key: &'a str,
    pub model: &'a Model,
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid built-in config: {0}")]
    Builtin(#[source] serde_json::Error),
    #[error("unknown vendor `{name}`; known vendors: {known}")]
    UnknownVendor { name: String, known: String },
    #[error("unknown model `{name}` in vendor `{vendor}`; known models: {known}")]
    UnknownModel {
        name: String,
        vendor: String,
        known: String,
    },
    #[error("no vendor given and `defaults.vendor` is not set")]
    NoDefaultVendor,
    #[error("no model given and `defaults.model` is not set for vendor `{vendor}`")]
    NoDefaultModel { vendor: String },
    #[error("vendor `{vendor}` has no models")]
    EmptyVendor { vendor: String },
    #[error("config value `{field}` must be positive")]
    InvalidSetting { field: &'static str },
    #[error("config value `{field}` must not be empty")]
    EmptySetting { field: &'static str },
    #[error("config value `{field}` is `{value}`; expected one of: {allowed}")]
    InvalidName {
        field: &'static str,
        value: String,
        allowed: &'static str,
    },
    #[error("config value `{field}` must be between {min} and {max}")]
    OutOfRange {
        field: &'static str,
        min: f64,
        max: f64,
    },
    #[error("config value `{field}` must be zero or positive")]
    NegativeSetting { field: &'static str },
    #[error("vendor `{vendor}` model `{model}` has a non-positive price for `{field}`")]
    NonPositivePrice {
        vendor: String,
        model: String,
        field: &'static str,
    },
    #[error(
        "vendor `{vendor}` model `{model}`: long-context threshold {threshold} must be below the context window {context_window}"
    )]
    ThresholdOutsideWindow {
        vendor: String,
        model: String,
        threshold: u64,
        context_window: u64,
    },
}

impl Config {
    /// Parses the compiled-in defaults without touching the filesystem.
    pub fn builtin() -> Result<Self, ConfigError> {
        serde_json::from_str(DEFAULT_CONFIG).map_err(ConfigError::Builtin)
    }

    /// Loads the first config found: project file, user file, then built-in defaults.
    pub fn load() -> Result<LoadedConfig, ConfigError> {
        let project = PathBuf::from(".venera").join("config.json");
        if project.is_file() {
            return Ok(LoadedConfig {
                config: Self::read(&project)?,
                source: ConfigSource::Project(project),
            });
        }

        if let Some(dirs) = directories::ProjectDirs::from("", "", "venera") {
            let user = dirs.config_dir().join("config.json");
            if user.is_file() {
                return Ok(LoadedConfig {
                    config: Self::read(&user)?,
                    source: ConfigSource::User(user),
                });
            }
        }

        Ok(LoadedConfig {
            config: Self::builtin()?,
            source: ConfigSource::Builtin,
        })
    }

    fn read(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Checks that every vendor and model is usable.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.index.max_file_size == 0 {
            return Err(ConfigError::InvalidSetting {
                field: "index.max_file_size",
            });
        }

        if self.index.map.default_token_budget == 0 {
            return Err(ConfigError::InvalidSetting {
                field: "index.map.default_token_budget",
            });
        }

        if self.index.database.trim().is_empty() {
            return Err(ConfigError::EmptySetting {
                field: "index.database",
            });
        }

        // `Some(0)` would make every turn reply nothing; treat it as a typo, not a budget.
        if self.limits.max_output_tokens_per_turn == Some(0) {
            return Err(ConfigError::InvalidSetting {
                field: "limits.max_output_tokens_per_turn",
            });
        }

        self.validate_context()?;

        for (vendor_name, vendor) in &self.vendors {
            if vendor.models.is_empty() {
                return Err(ConfigError::EmptyVendor {
                    vendor: vendor_name.clone(),
                });
            }

            for (model_key, model) in &vendor.models {
                model
                    .price
                    .validate()
                    .map_err(|field| ConfigError::NonPositivePrice {
                        vendor: vendor_name.clone(),
                        model: model_key.clone(),
                        field,
                    })?;

                if let Some(threshold) = model.price.long_context_threshold()
                    && threshold >= model.context_window
                {
                    return Err(ConfigError::ThresholdOutsideWindow {
                        vendor: vendor_name.clone(),
                        model: model_key.clone(),
                        threshold,
                        context_window: model.context_window,
                    });
                }
            }
        }

        Ok(())
    }

    /// Checks the context-optimisation section.
    fn validate_context(&self) -> Result<(), ConfigError> {
        const STRATEGIES: &[&str] = &[
            "adaptive",
            "direct-read",
            "partial-read",
            "indexed-retrieval",
            "project-map",
            "hybrid",
        ];
        const LEVELS: &[&str] = &["none", "light", "balanced", "aggressive", "semantic"];

        if !STRATEGIES.contains(&self.context.default_strategy.as_str()) {
            return Err(ConfigError::InvalidName {
                field: "context.default_strategy",
                value: self.context.default_strategy.clone(),
                allowed: "adaptive, direct-read, partial-read, indexed-retrieval, project-map, hybrid",
            });
        }

        let level = &self.context.compression.default_level;
        if !LEVELS.contains(&level.as_str()) {
            return Err(ConfigError::InvalidName {
                field: "context.compression.default_level",
                value: level.clone(),
                allowed: "none, light, balanced, aggressive, semantic",
            });
        }

        let target = self.context.compression.target_retained_ratio;
        if !target.is_finite() || !(0.1..=1.0).contains(&target) {
            return Err(ConfigError::OutOfRange {
                field: "context.compression.target_retained_ratio",
                min: 0.1,
                max: 1.0,
            });
        }

        let savings = self.context.compression.minimum_savings_ratio;
        if !savings.is_finite() || !(0.0..=1.0).contains(&savings) {
            return Err(ConfigError::OutOfRange {
                field: "context.compression.minimum_savings_ratio",
                min: 0.0,
                max: 1.0,
            });
        }

        if self.context.cache.m1_ttl_seconds < 0 {
            return Err(ConfigError::NegativeSetting {
                field: "context.cache.m1_ttl_seconds",
            });
        }

        Ok(())
    }

    /// Looks a vendor up by name.
    pub fn vendor(&self, name: &str) -> Result<&Vendor, ConfigError> {
        self.vendors
            .get(name)
            .ok_or_else(|| ConfigError::UnknownVendor {
                name: name.to_owned(),
                known: self.vendors.keys().cloned().collect::<Vec<_>>().join(", "),
            })
    }

    /// Resolves a vendor/model pair, falling back to `defaults`.
    pub fn resolve_model<'a>(
        &'a self,
        vendor: Option<&'a str>,
        model: Option<&'a str>,
    ) -> Result<Resolved<'a>, ConfigError> {
        let vendor_name = vendor
            .or(self.defaults.vendor.as_deref())
            .ok_or(ConfigError::NoDefaultVendor)?;
        let vendor_cfg = self.vendor(vendor_name)?;

        let model_name = model.or(self.defaults.model.as_deref()).ok_or_else(|| {
            ConfigError::NoDefaultModel {
                vendor: vendor_name.to_owned(),
            }
        })?;

        let (model_key, model_cfg) =
            vendor_cfg
                .find_model(model_name)
                .ok_or_else(|| ConfigError::UnknownModel {
                    name: model_name.to_owned(),
                    vendor: vendor_name.to_owned(),
                    known: vendor_cfg.model_keys(),
                })?;

        Ok(Resolved {
            vendor_name,
            vendor: vendor_cfg,
            model_key,
            model: model_cfg,
        })
    }
}
