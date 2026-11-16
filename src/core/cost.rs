//! RUB cost counter.
//!
//! Consumes `crate::provider::usage::Usage` plus the price table from
//! `crate::config::prices`, updates the status bar after every turn, and enforces
//! `limits.session_budget_rub`.
//!
//! The math is deliberately trivial and explicit: tokens / 1M * RUB-per-1M-token, per
//! billing bucket, with the long-context tier chosen by prompt size before billing.

use crate::config::ModelPrice;
use crate::provider::usage::Usage;

/// Cost of one turn, split into billing buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostBreakdown {
    pub input_rub: f64,
    pub output_rub: f64,
    pub cache_read_rub: f64,
    pub cache_write_rub: f64,
    /// Prompt tokens the tier was chosen from.
    pub prompt_tokens: u64,
    /// Whether the long-context tier applied.
    pub long_context: bool,
}

impl CostBreakdown {
    /// Sum of all buckets.
    pub fn total_rub(&self) -> f64 {
        self.input_rub + self.output_rub + self.cache_read_rub + self.cache_write_rub
    }
}

/// Prices a turn according to the model's tier for the prompt size.
pub fn turn_cost(price: &ModelPrice, usage: &Usage) -> CostBreakdown {
    let prompt_tokens = usage.prompt_tokens();
    let rates = price.rates_for(prompt_tokens);
    let long_context = price
        .long_context_threshold()
        .is_some_and(|threshold| prompt_tokens > threshold);

    CostBreakdown {
        input_rub: per_million(usage.input, rates.input),
        output_rub: per_million(usage.output, rates.output),
        cache_read_rub: per_million(usage.cache_read, rates.cache_read_or_input()),
        cache_write_rub: per_million(usage.cache_write.unwrap_or(0), rates.cache_write_or_input()),
        prompt_tokens,
        long_context,
    }
}

/// tokens / 1M * RUB per 1M tokens.
fn per_million(tokens: u64, rub_per_million: f64) -> f64 {
    tokens as f64 / 1_000_000.0 * rub_per_million
}

/// Result of charging a turn against the session budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// Still inside the cap.
    Within,
    /// Cap reached or crossed; the agent must ask for confirmation.
    Exceeded,
}

/// Accumulates session spend and checks it against `limits.session_budget_rub`.
#[derive(Debug, Clone, Default)]
pub struct CostCounter {
    spent_rub: f64,
    budget_rub: Option<f64>,
}

impl CostCounter {
    /// Creates a counter with an optional hard cap in RUB.
    pub fn new(budget_rub: Option<f64>) -> Self {
        Self {
            spent_rub: 0.0,
            budget_rub,
        }
    }

    /// Total spent so far.
    pub fn spent_rub(&self) -> f64 {
        self.spent_rub
    }

    /// Remaining budget, if a cap is configured.
    pub fn remaining_rub(&self) -> Option<f64> {
        self.budget_rub
            .map(|budget| (budget - self.spent_rub).max(0.0))
    }

    /// Charges a turn and reports whether the cap was reached.
    pub fn add(&mut self, breakdown: &CostBreakdown) -> Budget {
        self.spent_rub += breakdown.total_rub();
        match self.budget_rub {
            Some(budget) if self.spent_rub >= budget => Budget::Exceeded,
            _ => Budget::Within,
        }
    }
}

/// Formats a RUB amount for display: adaptive precision, comma decimals, `руб.` suffix.
pub fn format_rub(value: f64) -> String {
    let magnitude = value.abs();
    let decimals = if magnitude >= 1.0 {
        2
    } else if magnitude >= 0.01 {
        4
    } else {
        6
    };

    let mut text = format!("{value:.decimals$}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }

    match text.split_once('.') {
        Some((integer, fraction)) => format!("{},{fraction} руб.", group_digits(integer)),
        None => format!("{} руб.", group_digits(&text)),
    }
}

/// Formats a RUB amount with two decimals and thin-space thousands separators.
pub fn format_money(value: f64) -> String {
    let text = format!("{value:.2}");
    match text.split_once('.') {
        Some((integer, fraction)) => format!("{},{}", group_digits(integer), fraction),
        None => group_digits(&text),
    }
}

/// Formats a price per 1M tokens, always with two decimals.
pub fn format_rate(value: f64) -> String {
    format!("{} руб./1M", format_money(value))
}

/// Formats a token count with thin-space thousands separators.
pub fn format_tokens(tokens: u64) -> String {
    group_digits(&tokens.to_string())
}

/// Groups decimal digits in threes with thin spaces, e.g. `1050000` -> `1 050 000`.
fn group_digits(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push('\u{202F}');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::prices::{LongContext, ModelPrice, PriceRates};

    fn luna() -> ModelPrice {
        ModelPrice {
            rates: PriceRates {
                input: 30.0,
                output: 150.0,
                cache_read: Some(3.0),
                cache_write: Some(37.5),
            },
            long_context: Some(LongContext {
                threshold: 272_000,
                rates: PriceRates {
                    input: 60.0,
                    output: 225.0,
                    cache_read: Some(6.0),
                    cache_write: Some(75.0),
                },
            }),
        }
    }

    #[test]
    fn bills_input_tokens_at_the_normal_rate_below_the_threshold() {
        let usage = Usage {
            input: 100_000,
            ..Usage::default()
        };
        assert_eq!(turn_cost(&luna(), &usage).total_rub(), 3.0);
    }

    #[test]
    fn bills_one_million_input_tokens_at_the_long_context_rate() {
        let usage = Usage {
            input: 1_000_000,
            ..Usage::default()
        };
        let cost = turn_cost(&luna(), &usage);

        assert!(cost.long_context);
        assert_eq!(cost.total_rub(), 60.0);
    }

    #[test]
    fn switches_tiers_strictly_above_the_threshold() {
        let normal = Usage {
            input: 272_000,
            ..Usage::default()
        };
        let long = Usage {
            input: 272_001,
            ..Usage::default()
        };

        let normal_cost = turn_cost(&luna(), &normal);
        let long_cost = turn_cost(&luna(), &long);

        assert!(!normal_cost.long_context);
        assert!(long_cost.long_context);
        assert_eq!(normal_cost.total_rub(), 272_000.0 / 1_000_000.0 * 30.0);
        assert_eq!(long_cost.total_rub(), 272_001.0 / 1_000_000.0 * 60.0);
    }

    #[test]
    fn bills_cache_buckets_at_their_own_rates() {
        let usage = Usage {
            cache_read: 100_000,
            cache_write: Some(100_000),
            ..Usage::default()
        };
        let cost = turn_cost(&luna(), &usage);

        assert!(!cost.long_context);
        assert!((cost.cache_read_rub - 0.3).abs() < 1e-9);
        assert!((cost.cache_write_rub - 3.75).abs() < 1e-9);
        assert!((cost.total_rub() - 4.05).abs() < 1e-9);
    }

    #[test]
    fn falls_back_to_input_rate_without_cache_prices() {
        let price = ModelPrice {
            rates: PriceRates {
                input: 10.0,
                output: 20.0,
                cache_read: None,
                cache_write: None,
            },
            long_context: None,
        };
        let usage = Usage {
            cache_read: 1_000_000,
            cache_write: Some(1_000_000),
            ..Usage::default()
        };

        assert_eq!(turn_cost(&price, &usage).total_rub(), 20.0);
    }

    #[test]
    fn counter_reports_budget_exhaustion() {
        let mut counter = CostCounter::new(Some(2.0));
        let usage = Usage {
            output: 10_000,
            ..Usage::default()
        };
        let cost = turn_cost(&luna(), &usage);

        assert_eq!(counter.add(&cost), Budget::Within);
        assert_eq!(counter.add(&cost), Budget::Exceeded);
        assert_eq!(counter.remaining_rub(), Some(0.0));
    }

    #[test]
    fn formats_rub_with_adaptive_precision() {
        assert_eq!(format_rub(30.0), "30 руб.");
        assert_eq!(format_rub(0.00009), "0,00009 руб.");
        assert_eq!(format_rub(1.234), "1,23 руб.");
        assert_eq!(format_rub(0.0), "0 руб.");
    }

    #[test]
    fn formats_rates_and_token_counts() {
        assert_eq!(format_rate(37.5), "37,50 руб./1M");
        assert_eq!(format_rate(30.0), "30,00 руб./1M");
        assert_eq!(format_rate(2100.0), "2\u{202F}100,00 руб./1M");
        assert_eq!(format_money(2.74), "2,74");
        assert_eq!(format_tokens(1_050_000), "1\u{202F}050\u{202F}000");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_rub(1050.0), "1\u{202F}050 руб.");
    }
}
