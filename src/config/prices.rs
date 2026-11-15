//! Pricing model: RUB per 1M tokens, normal and long-context tiers.
//!
//! All prices are RUB per 1M tokens with VAT included, exactly as the aggregator lists
//! them. The long-context tier applies once the prompt exceeds `threshold` tokens
//! (`AGENTS.md` §7: 272000 for Luna, where prices change).
//!
//! Missing cache prices fall back to the input price, so a model without cache
//! discounts is still billed correctly.

use serde::{Deserialize, Serialize};

/// Price per 1M tokens for one tier.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PriceRates {
    /// Uncached prompt tokens.
    pub input: f64,
    /// Completion tokens.
    pub output: f64,
    /// Prompt tokens served from the provider cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Prompt tokens written to the provider cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

impl PriceRates {
    /// Cache-read rate, falling back to the input rate.
    pub fn cache_read_or_input(&self) -> f64 {
        self.cache_read.unwrap_or(self.input)
    }

    /// Cache-write rate, falling back to the input rate.
    pub fn cache_write_or_input(&self) -> f64 {
        self.cache_write.unwrap_or(self.input)
    }

    /// Returns the name of the first non-positive field, if any.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !is_positive(self.input) {
            return Err("input");
        }
        if !is_positive(self.output) {
            return Err("output");
        }
        if self.cache_read.is_some_and(|rate| !is_non_negative(rate)) {
            return Err("cache_read");
        }
        if self.cache_write.is_some_and(|rate| !is_non_negative(rate)) {
            return Err("cache_write");
        }
        Ok(())
    }
}

fn is_positive(rate: f64) -> bool {
    rate.is_finite() && rate > 0.0
}

fn is_non_negative(rate: f64) -> bool {
    rate.is_finite() && rate >= 0.0
}

/// Long-context tier: applies once the prompt is larger than `threshold` tokens.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LongContext {
    pub threshold: u64,
    pub rates: PriceRates,
}

/// Full price table of a model.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    /// Normal tier.
    pub rates: PriceRates,
    /// Optional long-context tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context: Option<LongContext>,
}

impl ModelPrice {
    /// Rates that apply to a prompt of `prompt_tokens` tokens.
    ///
    /// The threshold is exclusive: exactly 272000 tokens is still the normal tier.
    pub fn rates_for(&self, prompt_tokens: u64) -> &PriceRates {
        match &self.long_context {
            Some(long) if prompt_tokens > long.threshold => &long.rates,
            _ => &self.rates,
        }
    }

    /// Long-context threshold, if this model has a second tier.
    pub fn long_context_threshold(&self) -> Option<u64> {
        self.long_context.as_ref().map(|long| long.threshold)
    }

    /// Returns the name of the first non-positive field across both tiers.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.rates.validate()?;
        if let Some(long) = &self.long_context {
            long.rates.validate()?;
        }
        Ok(())
    }
}
