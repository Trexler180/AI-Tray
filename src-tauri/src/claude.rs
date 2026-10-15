use crate::models::{ClaudeUsage, DataHealth, DataSource, EstimateConfidence, ModelUsage};
use crate::pricing;
use crate::util::today_str;
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::history_cache::{modified_ms, Candidate, ScanRecord};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Mutex;
use walkdir::WalkDir;

const CACHE_VERSION: u32 = 1;
static COLLECT_LOCK: Mutex<()> = Mutex::new(());

pub fn clear_history_cache() -> std::io::Result<()> {
    let _guard = COLLECT_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    crate::history_cache::clear("claude")
}

fn projects_roots() -> Vec<PathBuf> {
    crate::accounts::account_dirs()
        .into_iter()
        .map(|dir| dir.join("projects"))
        .filter(|path| path.is_dir())
        .collect()
}

fn jsonl_files(roots: &[PathBuf], min_mtime: std::time::SystemTime) -> Vec<Candidate> {
    roots
        .iter()
        .flat_map(|root| WalkDir::new(root).into_iter().filter_map(Result::ok))
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            if metadata
                .modified()
                .ok()
                .is_some_and(|time| time < min_mtime)
            {
                return None;
            }
            let path = path.to_path_buf();
            Some(Candidate {
                key: path.to_string_lossy().to_ascii_lowercase(),
                size: metadata.len(),
                modified_ms: modified_ms(&metadata),
                path,
            })
        })
        .collect()
}

#[derive(Clone, Deserialize, Serialize)]
struct ClaudeEvent {
    ts: i64,
    identity: String,
    model: String,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct FileRecord {
    observed_size: u64,
    modified_ms: u64,
    processed_offset: u64,
    events: Vec<ClaudeEvent>,
}

impl ScanRecord for FileRecord {
    fn observed_size(&self) -> u64 {
        self.observed_size
    }
    fn observed_modified_ms(&self) -> u64 {
        self.modified_ms
    }
    fn processed_offset(&self) -> u64 {
        self.processed_offset
    }
    fn prune_before(&mut self, cutoff: i64) -> bool {
        let before = self.events.len();
        self.events.retain(|event| event.ts >= cutoff);
        before != self.events.len()
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Cache {
    files: BTreeMap<String, FileRecord>,
}

fn append_file(candidate: &Candidate, mut record: FileRecord) -> std::io::Result<FileRecord> {
    let mut file = File::open(&candidate.path)?;
    file.seek(SeekFrom::Start(record.processed_offset))?;
    let mut reader = BufReader::new(file);
    let mut offset = record.processed_offset;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if !line.ends_with('\n') {
            break;
        }
        offset += bytes as u64;
        if !line.contains("usage") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value["type"] != "assistant" {
            continue;
        }
        let message = &value["message"];
        let usage = &message["usage"];
        let model = message["model"].as_str().unwrap_or("");
        if usage.is_null() || model.is_empty() || model == "<synthetic>" {
            continue;
        }
        let ts = value["timestamp"]
            .as_str()
            .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
            .map(|timestamp| timestamp.with_timezone(&Utc).timestamp());
        let Some(ts) = ts else {
            continue;
        };
        let id = message["id"].as_str().unwrap_or("");
        let request = value["requestId"].as_str().unwrap_or("");
        let identity = if id.is_empty() && request.is_empty() {
            String::new()
        } else {
            format!("{id}:{request}")
        };
        let event = ClaudeEvent {
            ts,
            identity,
            model: model.to_string(),
            input: usage["input_tokens"].as_u64().unwrap_or(0),
            output: usage["output_tokens"].as_u64().unwrap_or(0),
            cache_write: usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            cache_read: usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
        };
        if event.input + event.output + event.cache_write + event.cache_read > 0 {
            record.events.push(event);
        }
    }
    record.observed_size = candidate.size;
    record.modified_ms = candidate.modified_ms;
    record.processed_offset = offset;
    Ok(record)
}

pub fn collect() -> ClaudeUsage {
    // Poison-tolerant for the same reason as the Codex scanner: one panicked
    // scan must not take Claude history down for the rest of the session.
    let _guard = COLLECT_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let roots = projects_roots();
    if roots.is_empty() {
        return ClaudeUsage::default();
    }
    collect_from_roots(&roots, true)
}

fn collect_from_roots(roots: &[PathBuf], persist: bool) -> ClaudeUsage {
    let cutoff = Utc::now().timestamp() - 30 * 86400;
    let min_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 86400);
    let candidates = jsonl_files(roots, min_mtime);
    let mut cache: Cache = if persist {
        crate::history_cache::load("claude", CACHE_VERSION)
    } else {
        Cache::default()
    };
    let stats = crate::history_cache::rescan(&mut cache.files, &candidates, cutoff, append_file);

    let mut usage = ClaudeUsage {
        available: cache.files.values().any(|record| !record.events.is_empty()),
        history_health: DataHealth {
            source: DataSource::LocalLogs,
            fetched_at: Some(Utc::now().timestamp()),
            attempted_at: Some(Utc::now().timestamp()),
            files_scanned: stats.scanned,
            files_cached: stats.cached,
            files_skipped: stats.skipped,
            ..Default::default()
        },
        ..Default::default()
    };
    let today = today_str();
    let mut seen_events = HashSet::new();
    let mut per_day: BTreeMap<String, (u64, f64)> = BTreeMap::new();
    let mut per_model: BTreeMap<String, (u64, f64, EstimateConfidence)> = BTreeMap::new();
    let mut confidence = EstimateConfidence::High;
    let mut unknown_models = BTreeSet::new();

    for event in cache.files.values().flat_map(|record| &record.events) {
        if event.ts < cutoff {
            continue;
        }
        if !event.identity.is_empty() && !seen_events.insert(event.identity.clone()) {
            continue;
        }
        let tokens = event.input + event.output + event.cache_write + event.cache_read;
        let priced = pricing::claude_value(
            &event.model,
            event.input,
            event.output,
            event.cache_write,
            event.cache_read,
        );
        confidence = confidence.max(priced.confidence);
        if let Some(model) = priced.unknown_model {
            unknown_models.insert(model);
        }
        let date = DateTime::<Utc>::from_timestamp(event.ts, 0)
            .map(|date| date.with_timezone(&Local).format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let day = per_day.entry(date.clone()).or_insert((0, 0.0));
        day.0 += tokens;
        day.1 += priced.cost;
        usage.tokens_30d += tokens;
        usage.cost_30d += priced.cost;
        usage.token_breakdown.input += event.input;
        usage.token_breakdown.output += event.output;
        usage.token_breakdown.cache_creation += event.cache_write;
        usage.token_breakdown.cache_read += event.cache_read;
        let model =
            per_model
                .entry(event.model.clone())
                .or_insert((0, 0.0, EstimateConfidence::High));
        model.0 += tokens;
        model.1 += priced.cost;
        model.2 = model.2.max(priced.confidence);
        if date == today {
            usage.tokens_today += tokens;
            usage.cost_today += priced.cost;
        }
    }

    usage.daily = crate::util::fill_daily(&per_day, 30);
    usage.models = per_model
        .into_iter()
        .map(|(model, (tokens, value, confidence))| ModelUsage {
            model,
            tokens,
            value,
            confidence: pricing::reported_confidence(confidence),
        })
        .collect();
    usage.estimate = pricing::metadata(confidence, unknown_models);
    if persist && stats.changed {
        if let Err(error) = crate::history_cache::save("claude", CACHE_VERSION, &cache) {
            usage.history_health.error_kind = Some("cache_write".to_string());
            usage.history_health.error_message =
                Some(format!("Could not save scan cache: {error}"));
        }
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    #[test]
    fn fixture_scans_and_preserves_separate_cache_categories() {
        let root = std::env::temp_dir().join(format!("ai-usage-claude-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        // Re-dated on the way in: the assertions below read a 30-day window, so
        // the fixture's own date has to stay inside it.
        crate::util::copy_fixture_dated(
            Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/claude_assistant.jsonl"
            )),
            &root.join("fixture.jsonl"),
            &[("2026-07-15", 1)],
        );
        let usage = collect_from_roots(std::slice::from_ref(&root), false);
        assert_eq!(usage.tokens_30d, 1_000);
        assert_eq!(usage.token_breakdown.cache_creation, 200);
        assert_eq!(usage.token_breakdown.cache_read, 300);
        assert_eq!(usage.history_health.files_scanned, 1);
        let _ = fs::remove_dir_all(root);
    }
}
