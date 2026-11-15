//! ProxyAPI model catalog: fetch, parse, cache.
//!
//! The aggregator exposes all 509 models on a single server-rendered page,
//! `https://proxyapi.ru/models/browse`. We parse the cards with plain string scanning —
//! no HTML parser dependency, no JavaScript — and cache the result as JSON so every
//! offline command keeps working without network access.
//!
//! The page only lists input/output prices. Cache and long-context tiers are curated in
//! `config/default.json`; when a catalog model is used, cache tokens fall back to the
//! input rate (see `crate::config::prices`).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::provider::registry::{self, CatalogSource};

/// Page that lists every model with its prices.
pub const BROWSE_URL: &str = "https://proxyapi.ru/models/browse";

/// Vendor name the catalog belongs to.
pub const VENDOR: &str = "proxyapi";

/// Billing unit a model is priced in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceUnit {
    /// RUB per 1M tokens.
    Tokens,
    /// RUB per 1M characters.
    Chars,
    /// RUB per second (video).
    Second,
    /// RUB per minute (audio).
    Minute,
}

impl PriceUnit {
    /// Human-readable unit for messages.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Tokens => "1M tokens",
            Self::Chars => "1M characters",
            Self::Second => "second",
            Self::Minute => "minute",
        }
    }

    /// Compact unit for table columns.
    pub fn short(self) -> &'static str {
        match self {
            Self::Tokens => "1M tok",
            Self::Chars => "1M chr",
            Self::Second => "/sec",
            Self::Minute => "/min",
        }
    }
}

/// Capabilities the catalog advertises for a model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// "Рассуждения": the model can spend tokens thinking.
    pub reasoning: bool,
    pub vision: bool,
    /// "Функции": tool calling.
    pub tools: bool,
    pub json: bool,
}

/// Input/output prices as listed on the catalog page.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPrice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
}

/// One model from the catalog.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModel {
    /// Provider id, e.g. `openai/gpt-6-luna`.
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<CatalogPrice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<PriceUnit>,
    /// Second price tier for long prompts, when the catalog lists one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context: Option<CatalogLongContext>,
    /// Fraction taken off the input price for cached prompt tokens (`0.9` = 90%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_discount: Option<f64>,
}

/// A long-context price step from the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogLongContext {
    /// Applies once the prompt exceeds this many tokens.
    pub threshold: u64,
    pub input: f64,
    pub output: f64,
}

/// One value of AITUNNEL's public chat catalog.
#[derive(Debug, Deserialize)]
struct AitunnelEntry {
    #[serde(default, rename = "provider")]
    _provider: Option<String>,
    #[serde(default)]
    prompt_cost: Option<f64>,
    #[serde(default)]
    completion_cost: Option<f64>,
    #[serde(default)]
    context_size: Option<u64>,
    #[serde(default, rename = "max_output")]
    _max_output: Option<u64>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    modalities: Option<AitunnelModalities>,
    #[serde(default)]
    cache_discount: Option<f64>,
    #[serde(default)]
    price_tiers: Option<Vec<AitunnelTier>>,
}

#[derive(Debug, Deserialize)]
struct AitunnelModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AitunnelTier {
    min_prompt_tokens: u64,
    prompt_cost: f64,
    completion_cost: f64,
}

impl CatalogModel {
    /// True when the model can be priced with token math.
    pub fn is_token_priced(&self) -> bool {
        self.unit == Some(PriceUnit::Tokens)
            && self
                .price
                .is_some_and(|price| price.input.is_some() || price.output.is_some())
    }

    /// Short name without the vendor prefix.
    pub fn short_name(&self) -> &str {
        self.id.rsplit('/').next().unwrap_or(&self.id)
    }

    /// Capability tags, for display.
    pub fn tags(&self) -> Vec<&'static str> {
        let mut tags = Vec::new();
        if self.capabilities.reasoning {
            tags.push("reasoning");
        }
        if self.capabilities.vision {
            tags.push("vision");
        }
        if self.capabilities.tools {
            tags.push("tools");
        }
        if self.capabilities.json {
            tags.push("json");
        }
        tags
    }
}

/// Cached catalog document.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub vendor: String,
    pub source_url: String,
    /// Unix seconds; no date crate needed.
    pub fetched_at_unix: u64,
    pub models: Vec<CatalogModel>,
}

impl Catalog {
    /// Parses the browse page into a catalog.
    pub fn from_browse_html(html: &str, fetched_at_unix: u64) -> Self {
        Self {
            vendor: VENDOR.to_owned(),
            source_url: BROWSE_URL.to_owned(),
            fetched_at_unix,
            models: parse_browse(html),
        }
    }

    /// Parses AITUNNEL's public chat catalog: a map of model id to parameters.
    pub fn from_aitunnel_json(text: &str, fetched_at_unix: u64) -> Result<Self, CatalogError> {
        let entries: std::collections::BTreeMap<String, AitunnelEntry> =
            serde_json::from_str(text).map_err(CatalogError::Invalid)?;

        let models = entries
            .into_iter()
            .map(|(id, entry)| {
                let price = match (entry.prompt_cost, entry.completion_cost) {
                    (None, None) => None,
                    (input, output) => Some(CatalogPrice { input, output }),
                };
                let long_context = entry
                    .price_tiers
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter(|tier| tier.min_prompt_tokens > 0)
                    .min_by_key(|tier| tier.min_prompt_tokens)
                    .map(|tier| CatalogLongContext {
                        threshold: tier.min_prompt_tokens,
                        input: tier.prompt_cost,
                        output: tier.completion_cost,
                    });

                CatalogModel {
                    id: id.clone(),
                    name: entry.description.unwrap_or_else(|| id.clone()),
                    context_window: entry.context_size,
                    capabilities: Capabilities {
                        reasoning: false,
                        vision: entry.modalities.as_ref().is_some_and(|modalities| {
                            modalities.input.iter().any(|kind| kind == "image")
                        }),
                        tools: true,
                        json: false,
                    },
                    price,
                    unit: Some(PriceUnit::Tokens),
                    long_context,
                    cache_discount: entry
                        .cache_discount
                        .filter(|discount| *discount > 0.0 && *discount < 1.0),
                }
            })
            .collect();

        let vendor = registry::get("aitunnel").map_or("aitunnel", |provider| provider.id);
        Ok(Self {
            vendor: vendor.to_owned(),
            source_url: "https://api.aitunnel.ru/public/aitunnel/models/chat".to_owned(),
            fetched_at_unix,
            models,
        })
    }

    /// Finds a model by id, short name, or display name (case-insensitive).
    pub fn find(&self, name: &str) -> Option<&CatalogModel> {
        self.models
            .iter()
            .find(|model| {
                model.id.eq_ignore_ascii_case(name) || model.name.eq_ignore_ascii_case(name)
            })
            .or_else(|| {
                self.models
                    .iter()
                    .find(|model| model.short_name().eq_ignore_ascii_case(name))
            })
    }

    /// Models usable for token math, optionally filtered by a substring.
    pub fn token_models(&self, filter: Option<&str>) -> Vec<&CatalogModel> {
        let needle = filter.map(str::to_ascii_lowercase);
        self.models
            .iter()
            .filter(|model| model.is_token_priced())
            .filter(|model| match &needle {
                Some(needle) => {
                    model.id.to_ascii_lowercase().contains(needle)
                        || model.name.to_ascii_lowercase().contains(needle)
                }
                None => true,
            })
            .collect()
    }

    /// Default cache location: `<data-local>/venera/catalog/<provider>.json`.
    pub fn cache_path() -> Option<PathBuf> {
        Self::cache_path_for(VENDOR)
    }

    /// Cache location of one provider's catalog.
    pub fn cache_path_for(vendor: &str) -> Option<PathBuf> {
        directories::BaseDirs::new().map(|dirs| {
            dirs.data_local_dir()
                .join("venera")
                .join("catalog")
                .join(format!("{vendor}.json"))
        })
    }

    /// Reads the cached catalog, if any.
    pub fn load_cached() -> Result<Option<Self>, CatalogError> {
        Self::load_cached_for(VENDOR)
    }

    /// Reads one provider's cached catalog.
    pub fn load_cached_for(vendor: &str) -> Result<Option<Self>, CatalogError> {
        let Some(path) = Self::cache_path_for(vendor) else {
            return Ok(None);
        };
        Self::load_from(&path)
    }

    /// Reads a catalog from an explicit path.
    pub fn load_from(path: &Path) -> Result<Option<Self>, CatalogError> {
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path).map_err(|source| CatalogError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|source| CatalogError::Parse {
                path: path.to_path_buf(),
                source,
            })
    }

    /// Writes the catalog to the default cache location and returns the path.
    pub fn save_cached(&self) -> Result<PathBuf, CatalogError> {
        let path = Self::cache_path_for(&self.vendor).ok_or(CatalogError::NoDataDir)?;
        self.save_to(&path)?;
        Ok(path)
    }

    /// Writes the catalog to an explicit path.
    pub fn save_to(&self, path: &Path) -> Result<(), CatalogError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| CatalogError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = serde_json::to_string_pretty(self).map_err(CatalogError::Serialize)?;
        std::fs::write(path, text).map_err(|source| CatalogError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Fetches the browse page and parses it. Requires network access.
    pub async fn fetch(client: &reqwest::Client) -> Result<Self, CatalogError> {
        Self::fetch_for(VENDOR, client).await
    }

    /// Fetches and parses the catalog published by `provider_id`.
    pub async fn fetch_for(
        provider_id: &str,
        client: &reqwest::Client,
    ) -> Result<Self, CatalogError> {
        let Some(provider) = registry::get(provider_id) else {
            return Err(CatalogError::UnknownProvider(provider_id.to_owned()));
        };

        let response = client
            .get(provider.models_url)
            .header(
                reqwest::header::USER_AGENT,
                concat!("venera/", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(CatalogError::Status(status.as_u16()));
        }

        let text = response.text().await?;
        let catalog = match provider.catalog {
            CatalogSource::ProxyapiBrowse => Self::from_browse_html(&text, unix_now()),
            CatalogSource::AitunnelPublic => Self::from_aitunnel_json(&text, unix_now())?,
        };

        if catalog.models.is_empty() {
            return Err(CatalogError::Empty);
        }

        Ok(catalog)
    }
}

/// Catalog errors.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("catalog request returned HTTP {0}")]
    Status(u16),
    #[error("unknown provider `{0}`")]
    UnknownProvider(String),
    #[error("invalid catalog JSON: {0}")]
    Invalid(#[source] serde_json::Error),
    #[error("catalog page contained no models (layout changed?)")]
    Empty,
    #[error("failed to read catalog {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write catalog {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid catalog {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize catalog: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("no user data directory is available for the catalog cache")]
    NoDataDir,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Parses every `<article>` card on the browse page.
pub fn parse_browse(html: &str) -> Vec<CatalogModel> {
    html.split("<article")
        .skip(1)
        .filter_map(|block| parse_card(block.split("</article>").next().unwrap_or_default()))
        .collect()
}

fn parse_card(block: &str) -> Option<CatalogModel> {
    let id = block.split("href=\"/models/").nth(1)?.split('"').next()?;
    if id.split('/').count() != 2 {
        return None;
    }

    let name = attribute(block, "aria-label=\"")
        .or_else(|| attribute(block, "title=\""))
        .unwrap_or_else(|| id.rsplit('/').next().unwrap_or(id).to_owned());

    let text = strip_html(block);
    let input = price_after(&text, "Ввод");
    let output = price_after(&text, "Вывод");

    Some(CatalogModel {
        id: id.to_owned(),
        name,
        context_window: context_window(&text),
        capabilities: Capabilities {
            reasoning: block.contains(">Рассуждения<"),
            vision: block.contains(">Vision<"),
            tools: block.contains(">Функции<"),
            json: block.contains(">JSON<"),
        },
        price: match (input, output) {
            (None, None) => first_rub_price(&text).map(|value| CatalogPrice {
                input: Some(value),
                output: None,
            }),
            (input, output) => Some(CatalogPrice { input, output }),
        },
        unit: unit_of(&text),
        long_context: None,
        cache_discount: None,
    })
}

fn attribute(html: &str, prefix: &str) -> Option<String> {
    let rest = html.split(prefix).nth(1)?;
    let value = rest.split('"').next()?;
    Some(unescape(value))
}

fn unit_of(text: &str) -> Option<PriceUnit> {
    if text.contains("за 1М токенов") {
        return Some(PriceUnit::Tokens);
    }
    if text.contains("за 1М символов") {
        return Some(PriceUnit::Chars);
    }
    if text.contains("за 1 минуту") {
        return Some(PriceUnit::Minute);
    }
    if text.contains("за 1 секунду") {
        return Some(PriceUnit::Second);
    }
    None
}

/// Price before the first `₽` sign, for cards without "Ввод"/"Вывод" labels.
fn first_rub_price(text: &str) -> Option<f64> {
    let rub = text.find('₽')?;
    let before = &text[..rub];
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, ch)| {
            ch.is_ascii_digit() || matches!(ch, ',' | '.' | ' ' | '\u{A0}' | '\u{202F}')
        })
        .last()
        .map(|(index, _)| index)?;

    first_number(&before[start..])
}

fn price_after(text: &str, label: &str) -> Option<f64> {
    let index = text.find(label)?;
    let window: String = text[index + label.len()..].chars().take(40).collect();
    first_number(&window)
}

fn first_number(text: &str) -> Option<f64> {
    let mut digits = String::new();
    let mut started = false;

    for ch in text.chars() {
        match ch {
            '0'..='9' => {
                started = true;
                digits.push(ch);
            }
            ',' | '.' if started => digits.push('.'),
            ' ' | '\u{A0}' | '\u{202F}' if started => {}
            _ if started => break,
            _ => {}
        }
    }

    digits.parse().ok()
}

fn context_window(text: &str) -> Option<u64> {
    let index = text.find("контекст")?;
    let rest = text[index + "контекст".len()..].trim_start();

    let mut digits = String::new();
    let mut suffix = None;

    for ch in rest.chars() {
        match ch {
            '0'..='9' => digits.push(ch),
            ',' | '.' if !digits.is_empty() => digits.push('.'),
            'M' | 'm' | 'М' | 'м' if !digits.is_empty() => {
                suffix = Some(1_000_000.0);
                break;
            }
            'K' | 'k' | 'К' | 'к' if !digits.is_empty() => {
                suffix = Some(1_000.0);
                break;
            }
            _ => break,
        }
    }

    let value: f64 = digits.parse().ok()?;
    let value = value * suffix.unwrap_or(1.0);
    Some(value.round() as u64)
}

fn strip_html(html: &str) -> String {
    let without_comments = strip_comments(html);
    let mut out = String::with_capacity(without_comments.len());
    let mut in_tag = false;

    for ch in without_comments.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

    unescape(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }

    out.push_str(rest);
    out
}

fn unescape(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD_LUNA: &str = r#"
<article class="card"><a aria-label="GPT-6 Luna" href="/models/openai/gpt-6-luna"></a>
<div><h3 title="GPT-6 Luna">GPT-6 Luna</h3>
<p title="openai/gpt-6-luna"><span>openai</span>/<!-- -->gpt-6-luna</p></div>
<span>контекст <!-- -->1,05M</span>
<div>Рассуждения</div><div>Vision</div><div>Функции</div><div>JSON</div>
<span>Ввод<!-- --> <span>30<!-- --> ₽<!-- --> за 1М токенов</span></span>
<span> • <!-- -->Вывод<!-- --> <span>150<!-- --> ₽<!-- --> за 1М токенов</span></span>
</article>"#;

    const CARD_VIDEO: &str = r#"
<article class="card"><a aria-label="Grok Imagine" href="/models/x-ai/grok-imagine-video-1.5-lite"></a>
<h3 title="Grok Imagine">Grok Imagine</h3>
<div>Vision</div>
<span>Видео 480p<!-- --> <span>2,74<!-- --> ₽<!-- --> за 1 секунду</span></span>
</article>"#;

    #[test]
    fn parses_a_token_priced_card() {
        let models = parse_browse(CARD_LUNA);
        assert_eq!(models.len(), 1);

        let model = &models[0];
        assert_eq!(model.id, "openai/gpt-6-luna");
        assert_eq!(model.name, "GPT-6 Luna");
        assert_eq!(model.context_window, Some(1_050_000));
        assert_eq!(model.unit, Some(PriceUnit::Tokens));
        assert!(model.is_token_priced());
        assert!(model.capabilities.reasoning);
        assert!(model.capabilities.tools);

        let price = model.price.expect("price");
        assert_eq!(price.input, Some(30.0));
        assert_eq!(price.output, Some(150.0));
    }

    #[test]
    fn parses_a_per_second_card_as_not_token_priced() {
        let models = parse_browse(CARD_VIDEO);
        assert_eq!(models.len(), 1);

        let model = &models[0];
        assert_eq!(model.unit, Some(PriceUnit::Second));
        assert!(!model.is_token_priced());
        assert_eq!(model.price.expect("price").input, Some(2.74));
    }

    #[test]
    fn parses_thousands_and_decimal_commas() {
        assert_eq!(first_number("1 263,16 ₽ за 1М"), Some(1263.16));
        assert_eq!(first_number("30 ₽ за 1М"), Some(30.0));
        assert_eq!(first_number("нет числа"), None);
    }

    #[test]
    fn parses_context_suffixes() {
        assert_eq!(context_window("контекст 262K"), Some(262_000));
        assert_eq!(context_window("контекст 1,02M"), Some(1_020_000));
        assert_eq!(context_window("контекст 128K"), Some(128_000));
        assert_eq!(context_window("без ограничения"), None);
    }

    #[test]
    fn finds_models_by_id_short_name_and_display_name() {
        let catalog = Catalog::from_browse_html(CARD_LUNA, 0);

        assert!(catalog.find("openai/gpt-6-luna").is_some());
        assert!(catalog.find("gpt-6-luna").is_some());
        assert!(catalog.find("GPT-6 Luna").is_some());
        assert!(catalog.find("nope").is_none());
    }

    #[test]
    fn filters_token_priced_models() {
        let html = format!("{CARD_LUNA}{CARD_VIDEO}");
        let catalog = Catalog::from_browse_html(&html, 0);

        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.token_models(None).len(), 1);
        assert_eq!(catalog.token_models(Some("luna")).len(), 1);
        assert_eq!(catalog.token_models(Some("grok")).len(), 0);
    }

    #[test]
    fn cache_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("catalog").join("proxyapi.json");
        let catalog = Catalog::from_browse_html(CARD_LUNA, 42);

        catalog.save_to(&path)?;
        let loaded = Catalog::load_from(&path)?.expect("catalog must load");

        assert_eq!(loaded, catalog);
        Ok(())
    }

    const AITUNNEL: &str = r#"{
        "claude-sonnet-4.6": {
            "provider": "anthropic",
            "prompt_cost": 600,
            "completion_cost": 3000,
            "context_size": 1000000,
            "max_output": 64000,
            "description": "Надёжная модель для кода",
            "modalities": { "input": ["text", "image"], "output": ["text"] },
            "cache_discount": 0.9
        },
        "qwen3.7-flash": {
            "prompt_cost": 6,
            "completion_cost": 26,
            "context_size": 262144,
            "price_tiers": [
                { "min_prompt_tokens": 0, "prompt_cost": 6, "completion_cost": 26 },
                { "min_prompt_tokens": 32000, "prompt_cost": 20, "completion_cost": 80 }
            ]
        }
    }"#;

    #[test]
    fn parses_the_aitunnel_public_catalog() -> Result<(), CatalogError> {
        let catalog = Catalog::from_aitunnel_json(AITUNNEL, 7)?;
        assert_eq!(catalog.vendor, "aitunnel");
        assert_eq!(catalog.models.len(), 2);

        let claude = catalog.find("claude-sonnet-4.6").expect("claude");
        assert_eq!(claude.context_window, Some(1_000_000));
        assert!(claude.capabilities.vision);
        assert_eq!(claude.price.expect("price").input, Some(600.0));
        assert_eq!(claude.price.expect("price").output, Some(3000.0));
        assert_eq!(claude.cache_discount, Some(0.9));
        assert!(claude.long_context.is_none());

        let qwen = catalog.find("qwen3.7-flash").expect("qwen");
        let tier = qwen.long_context.expect("long-context tier");
        assert_eq!(tier.threshold, 32_000);
        assert_eq!(tier.input, 20.0);
        assert_eq!(tier.output, 80.0);
        Ok(())
    }

    #[test]
    fn rejects_broken_aitunnel_json() {
        assert!(Catalog::from_aitunnel_json("[]", 0).is_err());
    }
}
