//! Model list for the picker: curated config first, then the ProxyAPI catalog.
//!
//! The catalog only lists input and output prices, so a model chosen from it has no
//! separate cache rates; [`crate::config::prices`] falls back to the input rate, which
//! keeps billing correct rather than optimistic. Models from `config.json` carry their
//! full price table, cache tiers included.

use crate::config::prices::{LongContext, ModelPrice, PriceRates};
use crate::config::{Config, Model};
use crate::provider::catalog::{Catalog, CatalogModel};

/// A model the UI can switch to.
#[derive(Debug, Clone)]
pub struct ModelChoice {
    /// Provider id, e.g. `openai/gpt-6-luna`.
    pub id: String,
    /// Aggregator that serves the model.
    pub provider: String,
    /// Short label for the header and the picker.
    pub label: String,
    /// Price table used for the session counter.
    pub price: ModelPrice,
    /// Context window in tokens; `0` when the catalog does not say.
    pub context_window: u64,
    /// True when the table came from `config.json` with cache rates.
    pub curated: bool,
    /// True when the provider advertises function tools for this model.
    pub tools: bool,
}

impl ModelChoice {
    /// Builds a choice from a curated config entry.
    pub fn from_config(provider: &str, key: &str, model: &Model) -> Self {
        Self {
            id: model.id.clone(),
            provider: provider.to_owned(),
            label: key.to_owned(),
            price: model.price.clone(),
            context_window: model.context_window,
            curated: true,
            tools: true,
        }
    }

    /// Builds a choice from a catalog entry; `None` when it cannot be priced.
    pub fn from_catalog(model: &CatalogModel, provider: &str) -> Option<Self> {
        let price = model.price?;
        let input = price.input?;
        let output = price.output?;

        let cache_read = model
            .cache_discount
            .map(|discount| input * (1.0 - discount).max(0.0));

        Some(Self {
            id: model.id.clone(),
            provider: provider.to_owned(),
            label: model.short_name().to_owned(),
            price: ModelPrice {
                rates: PriceRates {
                    input,
                    output,
                    cache_read,
                    cache_write: None,
                },
                long_context: model.long_context.map(|tier| LongContext {
                    threshold: tier.threshold,
                    rates: PriceRates {
                        input: tier.input,
                        output: tier.output,
                        cache_read: None,
                        cache_write: None,
                    },
                }),
            },
            context_window: model.context_window.unwrap_or(0),
            curated: false,
            tools: model.capabilities.tools,
        })
    }

    /// `30/150 руб. per 1M`.
    pub fn price_label(&self) -> String {
        format!(
            "{}/{} руб./1M",
            number(self.price.rates.input),
            number(self.price.rates.output)
        )
    }

    /// `ctx 1.05M`, or an empty string when the window is unknown.
    pub fn context_label(&self) -> String {
        if self.context_window == 0 {
            String::new()
        } else {
            format!("ctx {}", compact_tokens(self.context_window))
        }
    }
}

/// Every selectable model: curated entries first, then the token-priced catalog.
pub fn choices(config: &Config, catalog: Option<&Catalog>) -> Vec<ModelChoice> {
    let mut choices: Vec<ModelChoice> = Vec::new();

    for (provider, vendor) in &config.vendors {
        for (key, model) in &vendor.models {
            choices.push(ModelChoice::from_config(provider, key, model));
        }
    }

    if let Some(catalog) = catalog {
        for model in catalog.token_models(None) {
            if choices
                .iter()
                .any(|choice| choice.id == model.id && choice.provider == catalog.vendor)
            {
                continue;
            }
            if let Some(choice) = ModelChoice::from_catalog(model, &catalog.vendor) {
                choices.push(choice);
            }
        }
    }

    choices.sort_by(|left, right| {
        right.curated.cmp(&left.curated).then_with(|| {
            left.label
                .to_ascii_lowercase()
                .cmp(&right.label.to_ascii_lowercase())
        })
    });
    choices
}

/// Resolves one id or key against the config, then the catalog.
pub fn resolve(config: &Config, catalog: Option<&Catalog>, name: &str) -> Option<ModelChoice> {
    // The default vendor wins, so a bare `gpt-6-luna` means the configured model even when
    // another vendor happens to list the same tail name.
    if let Ok(resolved) = config.resolve_model(None, Some(name)) {
        return Some(ModelChoice::from_config(
            resolved.vendor_name,
            resolved.model_key,
            resolved.model,
        ));
    }

    for (provider, vendor) in &config.vendors {
        if let Some((key, model)) = vendor.find_model(name) {
            return Some(ModelChoice::from_config(provider, key, model));
        }
    }

    let catalog = catalog?;
    catalog
        .find(name)
        .and_then(|model| ModelChoice::from_catalog(model, &catalog.vendor))
}

/// The id used when nothing says otherwise.
pub fn default_id(config: &Config) -> Option<String> {
    config
        .resolve_model(None, None)
        .ok()
        .map(|resolved| resolved.model.id.clone())
}

/// A zero-priced table, used only so the UI has something to display.
pub fn unpriced() -> ModelPrice {
    ModelPrice {
        rates: PriceRates {
            input: 0.0,
            output: 0.0,
            cache_read: None,
            cache_write: None,
        },
        long_context: None,
    }
}

fn number(value: f64) -> String {
    if (value.fract()).abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// Short form for narrow places: `1.05M`, `272K`, `512`.
pub fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::catalog::Catalog;

    const CARD: &str = r#"
<article><a aria-label="GPT-6 Luna" href="/models/openai/gpt-6-luna"></a>
<h3 title="GPT-6 Luna">GPT-6 Luna</h3>
<span>контекст <!-- -->1,05M</span>
<div>Функции</div>
<span>Ввод<!-- --> <span>30<!-- --> руб.<!-- --> за 1М токенов</span></span>
<span> • <!-- -->Вывод<!-- --> <span>150<!-- --> руб.<!-- --> за 1М токенов</span></span>
</article>"#;

    const CARD_SOL: &str = r#"
<article><a aria-label="GPT-6 Sol" href="/models/openai/gpt-6-sol"></a>
<h3 title="GPT-6 Sol">GPT-6 Sol</h3>
<span>контекст <!-- -->1,05M</span>
<span>Ввод<!-- --> <span>420<!-- --> руб.<!-- --> за 1М токенов</span></span>
<span> • <!-- -->Вывод<!-- --> <span>2100<!-- --> руб.<!-- --> за 1М токенов</span></span>
</article>"#;

    fn config() -> Config {
        serde_json::from_str(crate::config::DEFAULT_CONFIG).expect("builtin config")
    }

    #[test]
    fn curated_models_come_first_and_catalog_models_fill_in() {
        let catalog = Catalog::from_browse_html(&format!("{CARD}{CARD_SOL}"), 0);
        let list = choices(&config(), Some(&catalog));

        assert_eq!(list[0].label, "gpt-6-luna");
        assert!(list[0].curated);
        assert_eq!(list[0].context_window, 1_050_000);

        // `sol` is already configured, so the catalog copy must not duplicate it.
        let sol: Vec<&ModelChoice> = list
            .iter()
            .filter(|choice| choice.id.ends_with("sol"))
            .collect();
        assert_eq!(sol.len(), 1);
    }

    #[test]
    fn catalog_models_without_both_prices_are_skipped() {
        let card = r#"
<article><a aria-label="Half" href="/models/x/half"></a>
<span>контекст 8K</span>
<span>Ввод <span>10 руб. за 1М токенов</span></span>
</article>"#;
        let catalog = Catalog::from_browse_html(card, 0);
        let list = choices(&config(), Some(&catalog));

        assert!(list.iter().all(|choice| !choice.id.ends_with("half")));
    }

    #[test]
    fn resolves_by_key_id_and_short_name() {
        let catalog = Catalog::from_browse_html(CARD, 0);

        assert_eq!(
            resolve(&config(), Some(&catalog), "gpt-6-luna").map(|choice| choice.id),
            Some("openai/gpt-6-luna".to_owned())
        );
        assert_eq!(
            resolve(&config(), Some(&catalog), "openai/gpt-6-luna").map(|choice| choice.id),
            Some("openai/gpt-6-luna".to_owned())
        );
        assert!(resolve(&config(), Some(&catalog), "definitely-not-a-model").is_none());
    }

    #[test]
    fn catalog_prices_have_no_cache_tier() {
        let card = r#"
<article><a aria-label="Extra" href="/models/x/extra"></a>
<span>контекст 128K</span>
<span>Ввод <span>10 руб. за 1М токенов</span></span>
<span> • Вывод <span>20 руб. за 1М токенов</span></span>
</article>"#;
        let catalog = Catalog::from_browse_html(card, 0);
        let choice = resolve(&config(), Some(&catalog), "x/extra").expect("catalog choice");

        assert!(!choice.curated);
        assert!(!choice.tools, "a card without Функции has no tool support");
        assert_eq!(choice.price.rates.input, 10.0);
        assert_eq!(choice.price.rates.cache_read, None);
        assert_eq!(choice.context_window, 128_000);
    }

    #[test]
    fn labels_are_compact() {
        let catalog = Catalog::from_browse_html(CARD, 0);
        let choice = resolve(&config(), Some(&catalog), "gpt-6-luna").expect("choice");

        assert_eq!(choice.price_label(), "30/150 руб./1M");
        assert_eq!(choice.context_label(), "ctx 1.05M");
    }
}
