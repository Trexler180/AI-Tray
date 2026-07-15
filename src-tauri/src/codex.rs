use crate::models::{CodexUsage, EstimateConfidence, Gauge, ModelUsage};
use crate::pricing;
use crate::util::{human_until, today_str};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

fn sessions_root() -> Option<PathBuf> {
    let mut p = dirs::home_dir()?;
    p.push(".codex");
    p.push("sessions");
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// All rollout-*.jsonl session files with their modified time.
fn session_files(root: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let is_rollout = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
            .unwrap_or(false);
        if !is_rollout {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            out.push((path.to_path_buf(), mtime));
        }
    }
    out
}

/// Extract YYYY-MM-DD from a filename like rollout-2026-06-01T20-55-53-<uuid>.jsonl
fn date_from_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("rollout-")?;
    // rest = 2026-06-01T...
    let date = rest.get(0..10)?;
    if date.len() == 10 && &date[4..5] == "-" {
        Some(date.to_string())
    } else {
        None
    }
}

struct LastCount {
    input: u64,
    cached: u64,
    output: u64,
    reasoning_output: u64,
    model: String,
    rate_limits: Option<Value>,
    plan_type: Option<String>,
    ts: Option<i64>,
}

/// Scan one session file, returning the final token_count snapshot (cumulative).
fn last_token_count(path: &Path) -> Option<LastCount> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut found: Option<LastCount> = None;
    let mut current_model = String::new();
    for line in reader.lines().map_while(Result::ok) {
        if !line.contains("token_count") && !line.contains("turn_context") {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let payload = &v["payload"];
        if v["type"] == "turn_context" {
            if let Some(model) = payload["model"].as_str() {
                current_model = model.to_string();
            }
            continue;
        }
        if payload["type"] != "token_count" {
            continue;
        }
        let total = &payload["info"]["total_token_usage"];
        let ts = v["timestamp"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc).timestamp());
        let rl = payload.get("rate_limits").cloned().filter(|x| !x.is_null());
        let plan = rl
            .as_ref()
            .and_then(|r| r["plan_type"].as_str())
            .map(|s| s.to_string());
        found = Some(LastCount {
            input: total["input_tokens"].as_u64().unwrap_or(0),
            cached: total["cached_input_tokens"].as_u64().unwrap_or(0),
            output: total["output_tokens"].as_u64().unwrap_or(0),
            reasoning_output: total["reasoning_output_tokens"].as_u64().unwrap_or(0),
            model: if current_model.is_empty() {
                "unknown".to_string()
            } else {
                current_model.clone()
            },
            rate_limits: rl,
            plan_type: plan,
            ts,
        });
    }
    found
}

fn gauge_from(window: &Value) -> Option<Gauge> {
    if window.is_null() {
        return None;
    }
    let used = window["used_percent"].as_f64()?;
    let win_min = window["window_minutes"].as_i64().unwrap_or(0);
    let resets_at = window["resets_at"].as_i64();
    Some(Gauge {
        used_percent: used,
        window_minutes: win_min,
        resets_at,
        resets_in: resets_at.map(human_until),
    })
}

pub fn collect() -> CodexUsage {
    let root = match sessions_root() {
        Some(r) => r,
        None => return CodexUsage::default(),
    };

    let mut files = session_files(&root);
    if files.is_empty() {
        return CodexUsage::default();
    }
    // newest first by mtime
    files.sort_by(|a, b| b.1.cmp(&a.1));

    let mut usage = CodexUsage {
        available: true,
        ..Default::default()
    };

    // Live gauges come from the most recently touched session.
    if let Some(latest) = last_token_count(&files[0].0) {
        usage.plan_type = latest.plan_type;
        usage.updated_at = latest.ts;
        if let Some(rl) = &latest.rate_limits {
            usage.primary = gauge_from(&rl["primary"]);
            usage.secondary = gauge_from(&rl["secondary"]);
            usage.credits = rl["credits"].as_f64();
        }
    }

    // API-equivalent value history: sum each session's final cumulative totals
    // per day. Cached input is a subset of input, not an additional category.
    let today = today_str();
    let cutoff = Utc::now().timestamp() - 30 * 86400;
    // Skip files untouched for >31 days without opening them — they can't
    // contribute to the 30-day window, and rescanning all history every
    // refresh gets expensive.
    let min_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 86400);
    let mut per_day: BTreeMap<String, (u64, f64)> = BTreeMap::new();
    let mut per_model: BTreeMap<String, (u64, f64, EstimateConfidence)> = BTreeMap::new();
    let mut confidence = EstimateConfidence::High;
    let mut unknown_models = BTreeSet::new();

    for (path, mtime) in &files {
        if *mtime < min_mtime {
            continue;
        }
        let date = match date_from_name(path) {
            Some(d) => d,
            None => continue,
        };
        // Skip files older than 30 days for the chart, cheaply via date string.
        if let Some(lc) = last_token_count(path) {
            // Only count within window if its timestamp is recent enough.
            if let Some(ts) = lc.ts {
                if ts < cutoff {
                    continue;
                }
            }
            let tokens = lc.input + lc.output;
            let priced = pricing::codex_value(&lc.model, lc.input, lc.cached, lc.output);
            let cost = priced.cost;
            confidence = confidence.max(priced.confidence);
            if let Some(model) = priced.unknown_model {
                unknown_models.insert(model);
            }
            let e = per_day.entry(date.clone()).or_insert((0, 0.0));
            e.0 += tokens;
            e.1 += cost;

            usage.tokens_30d += tokens;
            usage.cost_30d += cost;
            usage.token_breakdown.input += lc.input;
            usage.token_breakdown.cached_input += lc.cached;
            usage.token_breakdown.output += lc.output;
            usage.token_breakdown.reasoning_output += lc.reasoning_output;
            let model = per_model
                .entry(lc.model)
                .or_insert((0, 0.0, EstimateConfidence::High));
            model.0 += tokens;
            model.1 += cost;
            model.2 = model.2.max(priced.confidence);
            if date == today {
                usage.tokens_today += tokens;
                usage.cost_today += cost;
            }
        }
    }

    usage.daily = crate::util::fill_daily(&per_day, 30);
    usage.models = per_model
        .into_iter()
        .map(|(model, (tokens, value, confidence))| ModelUsage {
            model,
            tokens,
            value,
            confidence,
        })
        .collect();
    usage.estimate = pricing::metadata(confidence, unknown_models);

    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_from_rollout_name() {
        let p = Path::new("rollout-2026-06-01T20-55-53-abcd1234.jsonl");
        assert_eq!(date_from_name(p), Some("2026-06-01".to_string()));
        assert_eq!(date_from_name(Path::new("other.jsonl")), None);
        assert_eq!(date_from_name(Path::new("rollout-x.jsonl")), None);
    }

    #[test]
    fn fixture_keeps_model_and_codex_token_semantics() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/codex_token_count.jsonl"
        ));
        let count = last_token_count(path).expect("fixture token count");
        assert_eq!(count.model, "gpt-5.5");
        assert_eq!(count.input, 1_500);
        assert_eq!(count.cached, 900);
        assert_eq!(count.output, 300);
        assert_eq!(count.input + count.output, 1_800);
    }
}
