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

/// Whether what a prefix entry left over is only a dated snapshot of the model
/// the pattern names — `claude-opus-4-5` + `-20251101`, `gpt-5.5-` +
/// `2026-04-23` — rather than a different model in the same family, like
/// `claude-opus-4` + `-1-20250805`.
fn is_snapshot_suffix(rest: &str) -> bool {
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let digits = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    match rest.split('-').collect::<Vec<_>>().as_slice() {
        [date] => digits(date, 8),
        [year, month, day] => digits(year, 4) && digits(month, 2) && digits(day, 2),
        _ => false,
    }
}

fn matched_rates(provider: &str, model: &str) -> (Rates, EstimateConfidence, Option<String>) {
    let model = model.to_ascii_lowercase();
    for entry in &catalog().entries {
        if entry.provider != provider {
            continue;
        }
        // A prefix entry is an exact one for the model it names and that
        // model's dated snapshots, and a family price for anything else under
        // it. Every Claude entry is a prefix, so without this no Claude
        // estimate could ever be high confidence.
        let exact = match entry.match_kind.as_str() {
            "exact" => (model == entry.pattern).then_some(true),
            "prefix" => model
                .strip_prefix(entry.pattern.as_str())
                .map(|rest| rest.is_empty() || is_snapshot_suffix(rest)),
            _ => None,
        };
        if let Some(exact) = exact {
            let confidence = if exact {
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

/// Past its review date on `today` — or with a review date that doesn't
/// parse, which can't be shown not to have passed.
fn stale_on(review_after: &str, today: NaiveDate) -> bool {
    NaiveDate::parse_from_str(review_after, "%Y-%m-%d").map_or(true, |date| today > date)
}

fn catalog_stale() -> bool {
    stale_on(&catalog().review_after, Utc::now().date_naive())
}

/// A confidence as it should be reported. A catalog past its review date can't
/// vouch for any of its rates, however exactly the model matched, so the
/// figure drops to low until the catalog is reviewed again.
pub fn reported_confidence(confidence: EstimateConfidence) -> EstimateConfidence {
    if catalog_stale() {
        EstimateConfidence::Low
    } else {
        confidence
    }
}

pub fn metadata(confidence: EstimateConfidence, unknown: BTreeSet<String>) -> EstimateMetadata {
    let c = catalog();
    let stale = catalog_stale();
    EstimateMetadata {
        confidence: if stale {
            EstimateConfidence::Low
        } else {
            confidence
        },
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

    /// Output price per million tokens a model id resolves to, for asserting
    /// which catalog entry won.
    fn claude_output_rate(model: &str) -> (f64, EstimateConfidence) {
        let priced = claude_value(model, 0, 1_000_000, 0, 0);
        (priced.cost, priced.confidence)
    }

    /// Entries are matched first-to-last, so a newer model whose id starts with
    /// an older family's prefix has to be listed ahead of it. These are the
    /// cases that went wrong when a newer id was missing from the catalog.
    #[test]
    fn newer_claude_models_are_not_caught_by_older_prefixes() {
        // Not the $75 Opus 4 / 4.1 rate that `claude-opus-4` carries.
        assert_eq!(claude_output_rate("claude-opus-4-7").0, 25.0);
        assert_eq!(claude_output_rate("claude-opus-4-5-20251101").0, 25.0);
        assert_eq!(claude_output_rate("claude-opus-4-1-20250805").0, 75.0);
        // Opus 5.5 is its own price, not Opus 5's.
        assert_eq!(claude_output_rate("claude-opus-5-5").0, 20.0);
        assert_eq!(claude_output_rate("claude-opus-5").0, 25.0);
        assert_ne!(claude_output_rate("claude-opus-5").1, EstimateConfidence::Low);
    }

    #[test]
    fn fable_5_1_cache_reads_use_its_own_rate() {
        let priced = claude_value("claude-fable-5-1", 0, 0, 0, 1_000_000);
        assert!((priced.cost - 0.25).abs() < 1e-9);
        let older = claude_value("claude-fable-5", 0, 0, 0, 1_000_000);
        assert!((older.cost - 1.0).abs() < 1e-9);
    }

    /// The README's "High — the model has an exact current catalog entry":
    /// the model a prefix names, bare or as a dated snapshot, is that entry.
    /// A different model that only shares the family prefix is not.
    #[test]
    fn a_prefix_entry_is_exact_for_its_own_model_and_snapshots() {
        assert_eq!(claude_output_rate("claude-opus-5-5").1, EstimateConfidence::High);
        // Its own entry, not Sonnet 5's family price.
        assert_eq!(
            claude_output_rate("claude-sonnet-5-5"),
            (10.0, EstimateConfidence::High)
        );
        assert_eq!(
            claude_output_rate("claude-haiku-4-5-20251001").1,
            EstimateConfidence::High
        );
        assert_eq!(claude_output_rate("claude-opus-4-20250514").1, EstimateConfidence::High);
        // Opus 4.1 under the Opus 4 entry is a family price.
        assert_eq!(
            claude_output_rate("claude-opus-4-1-20250805").1,
            EstimateConfidence::Medium
        );
        assert_eq!(
            codex_value("gpt-5.5-2026-04-23", 1, 0, 0).confidence,
            EstimateConfidence::High
        );
        assert_eq!(
            codex_value("gpt-5.5-codex", 1, 0, 0).confidence,
            EstimateConfidence::Medium
        );
    }

    #[test]
    fn snapshot_suffixes_are_dates_and_nothing_else() {
        assert!(is_snapshot_suffix("-20251101"));
        assert!(is_snapshot_suffix("2026-04-23"));
        assert!(!is_snapshot_suffix("-1-20250805"));
        assert!(!is_snapshot_suffix("-5"));
        assert!(!is_snapshot_suffix("codex"));
        assert!(!is_snapshot_suffix("-2025110"));
    }

    /// Fixed dates, so this keeps passing after the bundled catalog's own
    /// review date goes by.
    #[test]
    fn a_catalog_past_its_review_date_is_stale() {
        let day = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert!(!stale_on("2026-11-30", day("2026-11-30")));
        assert!(stale_on("2026-11-30", day("2026-12-01")));
        assert!(stale_on("not a date", day("2026-01-01")));
    }

    #[test]
    fn unknown_models_are_explicitly_low_confidence() {
        let priced = codex_value("future-model", 1, 0, 0);
        assert_eq!(priced.confidence, EstimateConfidence::Low);
        assert_eq!(priced.unknown_model.as_deref(), Some("future-model"));
    }
}
