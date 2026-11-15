//! Picks the model for a turn: curated config first, then the fetched catalog.
//!
//! Config models win because they carry cache prices and long-context tiers that the
//! catalog page does not list. Everything else falls back to the catalog, which covers
//! all models the aggregator offers.

use crate::config::prices::{ModelPrice, PriceRates};
use crate::config::{Config, ConfigError, Encoding, Reasoning};
use crate::provider::catalog::{Catalog, CatalogModel};

/// Where the selected model came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Curated in `config/default.json` or the user config.
    Config,
    /// Fetched from the aggregator catalog.
    Catalog,
}

/// A model ready to be priced.
#[derive(Debug, Clone)]
pub struct Selection {
    pub vendor_name: String,
    pub chat_url: String,
    /// Key used in config or the catalog id.
    pub model_key: String,
    /// Provider-side id sent in the request.
    pub model_id: String,
    pub encoding: Encoding,
    pub context_window: Option<u64>,
    /// Default thinking level; `--reasoning` may override it per turn.
    pub reasoning: Reasoning,
    pub price: ModelPrice,
    pub origin: Origin,
}

/// Selection errors.
#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("model `{name}` is priced per {unit}, so token math does not apply")]
    NotTokenPriced { name: String, unit: &'static str },
    #[error("model `{name}` has no input/output price in the catalog")]
    NoPrice { name: String },
    #[error(
        "unknown model `{name}` for vendor `{vendor}`; known: {known}. Run `venera models --refresh` to load the full catalog"
    )]
    UnknownModel {
        name: String,
        vendor: String,
        known: String,
    },
}

/// Resolves `vendor`/`model` against the config and the optional catalog.
pub fn select(
    config: &Config,
    catalog: Option<&Catalog>,
    vendor: Option<&str>,
    model: Option<&str>,
) -> Result<Selection, SelectionError> {
    let vendor_name = vendor
        .or(config.defaults.vendor.as_deref())
        .ok_or(ConfigError::NoDefaultVendor)?;
    let vendor_cfg = config.vendor(vendor_name)?;

    let model_name =
        model
            .or(config.defaults.model.as_deref())
            .ok_or_else(|| ConfigError::NoDefaultModel {
                vendor: vendor_name.to_owned(),
            })?;

    if let Some((key, model_cfg)) = vendor_cfg.find_model(model_name) {
        return Ok(Selection {
            vendor_name: vendor_name.to_owned(),
            chat_url: vendor_cfg.chat_url(),
            model_key: key.to_owned(),
            model_id: model_cfg.id.clone(),
            encoding: model_cfg.encoding,
            context_window: Some(model_cfg.context_window),
            reasoning: model_cfg.reasoning,
            price: model_cfg.price.clone(),
            origin: Origin::Config,
        });
    }

    let catalog = catalog.filter(|catalog| catalog.vendor == vendor_name);
    if let Some(model_cfg) = catalog.and_then(|catalog| catalog.find(model_name)) {
        return from_catalog(vendor_name, vendor_cfg.chat_url(), model_cfg);
    }

    let mut known: Vec<String> = vendor_cfg.models.keys().cloned().collect();
    if let Some(catalog) = catalog {
        known.extend(
            catalog
                .token_models(None)
                .iter()
                .take(8)
                .map(|model| model.id.clone()),
        );
    }

    Err(SelectionError::UnknownModel {
        name: model_name.to_owned(),
        vendor: vendor_name.to_owned(),
        known: known.join(", "),
    })
}

fn from_catalog(
    vendor_name: &str,
    chat_url: String,
    model: &CatalogModel,
) -> Result<Selection, SelectionError> {
    if !model.is_token_priced() {
        return Err(SelectionError::NotTokenPriced {
            name: model.id.clone(),
            unit: model.unit.map_or(
                "an unknown unit",
                crate::provider::catalog::PriceUnit::describe,
            ),
        });
    }

    let price = model.price.ok_or_else(|| SelectionError::NoPrice {
        name: model.id.clone(),
    })?;
    let input = price
        .input
        .or(price.output)
        .ok_or_else(|| SelectionError::NoPrice {
            name: model.id.clone(),
        })?;
    let output = price.output.unwrap_or(input);

    Ok(Selection {
        vendor_name: vendor_name.to_owned(),
        chat_url,
        model_key: model.id.clone(),
        model_id: model.id.clone(),
        encoding: Encoding::default(),
        context_window: model.context_window,
        reasoning: if model.capabilities.reasoning {
            Reasoning::Medium
        } else {
            Reasoning::Off
        },
        price: ModelPrice {
            rates: PriceRates {
                input,
                output,
                cache_read: None,
                cache_write: None,
            },
            long_context: None,
        },
        origin: Origin::Catalog,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::catalog::{Capabilities, CatalogPrice, PriceUnit};

    fn catalog(models: Vec<CatalogModel>) -> Catalog {
        Catalog {
            vendor: crate::provider::catalog::VENDOR.to_owned(),
            source_url: "test".to_owned(),
            fetched_at_unix: 0,
            models,
        }
    }

    fn catalog_model(id: &str, unit: PriceUnit, reasoning: bool) -> CatalogModel {
        CatalogModel {
            id: id.to_owned(),
            name: id.rsplit('/').next().unwrap_or(id).to_owned(),
            context_window: Some(128_000),
            capabilities: Capabilities {
                reasoning,
                vision: false,
                tools: true,
                json: true,
            },
            price: Some(CatalogPrice {
                input: Some(12.0),
                output: Some(34.0),
            }),
            unit: Some(unit),
            long_context: None,
            cache_discount: None,
        }
    }

    #[test]
    fn config_model_wins_and_keeps_cache_prices() -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::builtin()?;
        let selection = select(&config, None, Some("proxyapi"), Some("gpt-6-luna"))?;

        assert_eq!(selection.origin, Origin::Config);
        assert_eq!(selection.model_id, "openai/gpt-6-luna");
        assert_eq!(selection.reasoning, Reasoning::Medium);
        assert_eq!(selection.price.rates.cache_read, Some(3.0));
        assert!(selection.price.long_context.is_some());
        Ok(())
    }

    #[test]
    fn catalog_model_is_selectable_by_id() -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::builtin()?;
        let catalog = catalog(vec![catalog_model(
            "deepseek/deepseek-v4",
            PriceUnit::Tokens,
            true,
        )]);

        let selection = select(
            &config,
            Some(&catalog),
            Some("proxyapi"),
            Some("deepseek/deepseek-v4"),
        )?;

        assert_eq!(selection.origin, Origin::Catalog);
        assert_eq!(selection.model_key, "deepseek/deepseek-v4");
        assert_eq!(selection.reasoning, Reasoning::Medium);
        assert_eq!(selection.price.rates.input, 12.0);
        assert_eq!(selection.price.rates.output, 34.0);
        assert_eq!(selection.context_window, Some(128_000));
        Ok(())
    }

    #[test]
    fn catalog_model_without_reasoning_defaults_to_off() -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::builtin()?;
        let catalog = catalog(vec![catalog_model("x/plain", PriceUnit::Tokens, false)]);

        let selection = select(&config, Some(&catalog), Some("proxyapi"), Some("x/plain"))?;
        assert_eq!(selection.reasoning, Reasoning::Off);
        Ok(())
    }

    #[test]
    fn non_token_priced_model_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::builtin()?;
        let catalog = catalog(vec![catalog_model("x/image", PriceUnit::Chars, false)]);

        let error = select(&config, Some(&catalog), Some("proxyapi"), Some("x/image"))
            .expect_err("per-character model must not be priced with token math");
        assert!(error.to_string().contains("1M characters"), "got: {error}");
        Ok(())
    }

    #[test]
    fn unknown_model_points_at_the_catalog_refresh() -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::builtin()?;
        let error = select(&config, None, Some("proxyapi"), Some("nope"))
            .expect_err("unknown model must fail");
        let message = error.to_string();

        assert!(message.contains("gpt-6-luna"), "got: {message}");
        assert!(message.contains("--refresh"), "got: {message}");
        Ok(())
    }
}
