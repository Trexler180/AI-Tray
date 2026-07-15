//! Model-aware API-equivalent pricing.
//!
//! Rates are bundled deliberately: R1 never downloads or silently mutates
//! pricing. `pricing_catalog.json` records the review date and official source
//! for every rate. Unknown models use the provider fallback and are marked low
//! confidence instead of being silently treated as a known model.

use crate::models::{EstimateConfidence, EstimateMetadata};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::OnceLock;

#[derive(Clone, Copy, Deserialize)]
struct Rates {
    input: f64,
    output: f64,
    #[serde(default)]
    cache_write: f64,
    #[serde(default)]
    cache_read: f64,
}

impl Rates {
    fn cost(&self, input: u64, output: u64, cache_write: u64, cache_read: u64) -> f64 {
        (input as f64 * self.input
            + output as f64 * self.output
            + cache_write as f64 * self.cache_write
            + cache_read as f64 * self.cache_read)
            / 1_000_000.0
    }
}

#[derive(Deserialize)]
struct CatalogEntry {
    provider: String,
    pattern: String,
    match_kind: String,
    rates: Rates,
}

#[derive(Deserialize)]
struct Fallbacks {
    codex: Rates,
    claude: Rates,
}

#[derive(Deserialize)]
struct Catalog {
    catalog_version: String,
    reviewed_at: String,
    review_after: String,
    fallbacks: Fallbacks,
    entries: Vec<CatalogEntry>,
}

#[derive(Clone)]
pub struct PricedUsage {
    pub cost: f64,
    pub confidence: EstimateConfidence,
    pub unknown_model: Option<String>,
}

static CATALOG: OnceLock<Catalog> = OnceLock::new();

fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("pricing_catalog.json"))
            .expect("bundled pricing catalog must be valid")
    })
}

fn matched_rates(provider: &str, model: &str) -> (Rates, EstimateConfidence, Option<String>) {
    let model = model.to_ascii_lowercase();
    for entry in &catalog().entries {
        if entry.provider != provider {
            continue;
        }
        let matched = match entry.match_kind.as_str() {
            "exact" => model == entry.pattern,
            "prefix" => model.starts_with(&entry.pattern),
            _ => false,
        };
        if matched {
            let confidence = if entry.match_kind == "exact" {
                EstimateConfidence::High
            } else {
                EstimateConfidence::Medium
            };
            return (entry.rates, confidence, None);
        }
    }

    let fallback = if provider == "claude" {
        catalog().fallbacks.claude
    } else {
        catalog().fallbacks.codex
    };
    (fallback, EstimateConfidence::Low, Some(model))
}

/// Codex reports cached input as a subset of input. Only the uncached portion
/// receives the normal input rate.
pub fn codex_value(model: &str, input: u64, cached_input: u64, output: u64) -> PricedUsage {
    let (rates, confidence, unknown_model) = matched_rates("codex", model);
    let uncached = input.saturating_sub(cached_input);
    PricedUsage {
        cost: rates.cost(uncached, output, 0, cached_input),
        confidence,
        unknown_model,
    }
}

/// Claude reports input, cache creation and cache read as separate categories.
pub fn claude_value(
    model: &str,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
) -> PricedUsage {
    let (rates, confidence, unknown_model) = matched_rates("claude", model);
    PricedUsage {
        cost: rates.cost(input, output, cache_write, cache_read),
        confidence,
        unknown_model,
    }
}

pub fn metadata(confidence: EstimateConfidence, unknown: BTreeSet<String>) -> EstimateMetadata {
    let c = catalog();
    let stale = NaiveDate::parse_from_str(&c.review_after, "%Y-%m-%d")
        .map(|date| Utc::now().date_naive() > date)
        .unwrap_or(true);
    EstimateMetadata {
        confidence,
        catalog_version: c.catalog_version.clone(),
        pricing_reviewed_at: c.reviewed_at.clone(),
        pricing_stale: stale,
        unknown_models: unknown.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_cached_input_is_not_charged_twice() {
        let priced = codex_value("gpt-5.5", 1_000_000, 600_000, 100_000);
        // 400k uncached * $5 + 600k cached * $0.50 + 100k output * $30.
        assert!((priced.cost - 5.3).abs() < 0.000001);
        assert_eq!(priced.confidence, EstimateConfidence::High);
    }

    #[test]
    fn unknown_models_are_explicitly_low_confidence() {
        let priced = codex_value("future-model", 1, 0, 0);
        assert_eq!(priced.confidence, EstimateConfidence::Low);
        assert_eq!(priced.unknown_model.as_deref(), Some("future-model"));
    }
}
