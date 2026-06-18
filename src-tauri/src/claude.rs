use crate::models::ClaudeUsage;
use crate::pricing;
use crate::util::today_str;
use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// The `projects` transcript folder for every configured Claude directory that
/// has one. Cost/tokens are reported machine-wide (the logs carry no account
/// id), so all accounts' transcripts are pooled.
fn projects_roots() -> Vec<PathBuf> {
    crate::accounts::account_dirs()
        .into_iter()
        .map(|dir| dir.join("projects"))
        .filter(|p| p.is_dir())
        .collect()
}

/// All .jsonl transcripts modified after `min_mtime`. Files untouched for
/// longer than the reporting window can't contain entries inside it (they are
/// append-only), so skipping them keeps refreshes cheap as history grows.
fn jsonl_files(root: &Path, min_mtime: std::time::SystemTime) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| x == "jsonl")
                .unwrap_or(false)
        })
        .filter(|e| {
            e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| t >= min_mtime)
                .unwrap_or(true)
        })
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// One assistant turn's billable tokens.
struct Entry {
    ts: i64,
    tokens: u64,
    cost: f64,
}

pub fn collect() -> ClaudeUsage {
    let roots = projects_roots();
    if roots.is_empty() {
        return ClaudeUsage::default();
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let now = Utc::now().timestamp();
    let cutoff_30d = now - 30 * 86400;
    // One extra day of slack so local-timezone edges never drop entries.
    let min_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 86400);

    let files = roots.iter().flat_map(|root| jsonl_files(root, min_mtime));
    for path in files {
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if !line.contains("usage") {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if v["type"] != "assistant" {
                continue;
            }
            let msg = &v["message"];
            let usage = &msg["usage"];
            if usage.is_null() {
                continue;
            }
            let model = msg["model"].as_str().unwrap_or("");
            if model.is_empty() || model == "<synthetic>" {
                continue;
            }

            // Dedupe on message id + request id (ccusage approach). Entries
            // with neither id can't be distinguished, so never drop them.
            let id = msg["id"].as_str().unwrap_or("");
            let req = v["requestId"].as_str().unwrap_or("");
            if !(id.is_empty() && req.is_empty()) && !seen.insert(format!("{id}:{req}")) {
                continue;
            }

            let input = usage["input_tokens"].as_u64().unwrap_or(0);
            let output = usage["output_tokens"].as_u64().unwrap_or(0);
            let cache_write = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            let tokens = input + output + cache_write + cache_read;
            if tokens == 0 {
                continue;
            }

            let ts = match v["timestamp"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            {
                Some(d) => d.with_timezone(&Utc).timestamp(),
                None => continue,
            };

            let cost = pricing::claude_rates(model).cost(input, output, cache_write, cache_read);
            entries.push(Entry { ts, tokens, cost });
        }
    }

    let mut usage = ClaudeUsage {
        available: !entries.is_empty(),
        ..Default::default()
    };

    let today = today_str();
    let mut per_day: BTreeMap<String, (u64, f64)> = BTreeMap::new();

    for e in &entries {
        if e.ts < cutoff_30d {
            continue;
        }
        let date = DateTime::<Utc>::from_timestamp(e.ts, 0)
            .map(|d| d.with_timezone(&Local).format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let slot = per_day.entry(date.clone()).or_insert((0, 0.0));
        slot.0 += e.tokens;
        slot.1 += e.cost;

        usage.tokens_30d += e.tokens;
        usage.cost_30d += e.cost;
        if date == today {
            usage.tokens_today += e.tokens;
            usage.cost_today += e.cost;
        }
    }

    usage.daily = crate::util::fill_daily(&per_day, 30);

    usage
}
