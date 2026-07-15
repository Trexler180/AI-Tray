use crate::models::{ClaudeUsage, DataHealth, DataSource, EstimateConfidence, ModelUsage};
use crate::pricing;
use crate::util::today_str;
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{File, Metadata};
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

#[derive(Clone)]
struct Candidate {
    path: PathBuf,
    key: String,
    size: u64,
    modified_ms: u64,
}

fn modified_ms(metadata: &Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
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

#[derive(Default, Deserialize, Serialize)]
struct Cache {
    files: BTreeMap<String, FileRecord>,
}

fn scan_file(candidate: &Candidate, existing: Option<FileRecord>) -> std::io::Result<FileRecord> {
    if let Some(record) = existing {
        if record.observed_size == candidate.size && record.modified_ms == candidate.modified_ms {
            return Ok(record);
        }
        if candidate.size >= record.processed_offset && candidate.size != record.observed_size {
            return append_file(candidate, record);
        }
    }
    append_file(candidate, FileRecord::default())
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
    let _guard = COLLECT_LOCK.lock().unwrap();
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
    let mut seen_files = HashSet::new();
    let mut scanned = 0u64;
    let mut cached = 0u64;
    let mut skipped = 0u64;
    let mut changed = false;

    for candidate in &candidates {
        seen_files.insert(candidate.key.clone());
        let existing = cache.files.remove(&candidate.key);
        let backup = existing.clone();
        let was_cached = existing.as_ref().is_some_and(|record| {
            record.observed_size == candidate.size && record.modified_ms == candidate.modified_ms
        });
        match scan_file(candidate, existing) {
            Ok(record) => {
                if was_cached {
                    cached += 1;
                } else {
                    scanned += 1;
                    changed = true;
                }
                cache.files.insert(candidate.key.clone(), record);
            }
            Err(_) => {
                skipped += 1;
                if let Some(record) = backup {
                    cache.files.insert(candidate.key.clone(), record);
                }
            }
        }
    }
    let before = cache.files.len();
    cache.files.retain(|key, _| seen_files.contains(key));
    changed |= before != cache.files.len();

    let mut usage = ClaudeUsage {
        available: cache.files.values().any(|record| !record.events.is_empty()),
        history_health: DataHealth {
            source: DataSource::LocalLogs,
            fetched_at: Some(Utc::now().timestamp()),
            attempted_at: Some(Utc::now().timestamp()),
            files_scanned: scanned,
            files_cached: cached,
            files_skipped: skipped,
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
            confidence,
        })
        .collect();
    usage.estimate = pricing::metadata(confidence, unknown_models);
    if persist && changed {
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
        fs::copy(
            Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/claude_assistant.jsonl"
            )),
            root.join("fixture.jsonl"),
        )
        .unwrap();
        let usage = collect_from_roots(std::slice::from_ref(&root), false);
        assert_eq!(usage.tokens_30d, 1_000);
        assert_eq!(usage.token_breakdown.cache_creation, 200);
        assert_eq!(usage.token_breakdown.cache_read, 300);
        assert_eq!(usage.history_health.files_scanned, 1);
        let _ = fs::remove_dir_all(root);
    }
}
