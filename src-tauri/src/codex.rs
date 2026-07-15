use crate::models::{CodexUsage, DataHealth, DataSource, EstimateConfidence, Gauge, ModelUsage};
use crate::pricing;
use crate::util::{human_until, today_str};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use walkdir::WalkDir;

const CACHE_VERSION: u32 = 1;
static COLLECT_LOCK: Mutex<()> = Mutex::new(());

fn codex_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        if root.is_dir() {
            return Some(root);
        }
    }
    let root = dirs::home_dir()?.join(".codex");
    root.is_dir().then_some(root)
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

fn rollout_files(root: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    for folder in ["sessions", "archived_sessions"] {
        let folder = root.join(folder);
        if !folder.is_dir() {
            continue;
        }
        for entry in WalkDir::new(folder).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            out.push(Candidate {
                path: path.to_path_buf(),
                key: name.to_string(),
                size: metadata.len(),
                modified_ms: modified_ms(&metadata),
            });
        }
    }
    out
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
struct Counts {
    input: u64,
    cached: u64,
    output: u64,
    reasoning_output: u64,
}

impl Counts {
    fn from_json(value: &Value) -> Self {
        Self {
            input: value["input_tokens"].as_u64().unwrap_or(0),
            cached: value["cached_input_tokens"].as_u64().unwrap_or(0),
            output: value["output_tokens"].as_u64().unwrap_or(0),
            reasoning_output: value["reasoning_output_tokens"].as_u64().unwrap_or(0),
        }
    }

    fn delta_from(self, previous: Self) -> Self {
        Self {
            input: self.input.saturating_sub(previous.input),
            cached: self.cached.saturating_sub(previous.cached),
            output: self.output.saturating_sub(previous.output),
            reasoning_output: self
                .reasoning_output
                .saturating_sub(previous.reasoning_output),
        }
    }

    fn is_empty(self) -> bool {
        self.input == 0 && self.cached == 0 && self.output == 0
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct TokenEvent {
    ts: i64,
    model: String,
    counts: Counts,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct FileRecord {
    path: String,
    observed_size: u64,
    modified_ms: u64,
    processed_offset: u64,
    current_model: String,
    last_total: Counts,
    rate_limits: Option<Value>,
    plan_type: Option<String>,
    last_ts: Option<i64>,
    events: Vec<TokenEvent>,
}

#[derive(Default, Deserialize, Serialize)]
struct Cache {
    files: BTreeMap<String, FileRecord>,
}

fn gauge_from(window: &Value) -> Option<Gauge> {
    if window.is_null() {
        return None;
    }
    let used = window["used_percent"].as_f64()?;
    let resets_at = window["resets_at"].as_i64();
    Some(Gauge {
        used_percent: used,
        window_minutes: window["window_minutes"].as_i64().unwrap_or(0),
        resets_at,
        resets_in: resets_at.map(human_until),
    })
}

fn scan_file(candidate: &Candidate, existing: Option<FileRecord>) -> std::io::Result<FileRecord> {
    if let Some(mut record) = existing {
        if record.observed_size == candidate.size && record.modified_ms == candidate.modified_ms {
            record.path = candidate.path.to_string_lossy().to_string();
            return Ok(record);
        }
        if candidate.size >= record.processed_offset && candidate.size != record.observed_size {
            record.path = candidate.path.to_string_lossy().to_string();
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
        if !line.contains("token_count")
            && !line.contains("turn_context")
            && !line.contains("session_meta")
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value["type"] == "turn_context" {
            if let Some(model) = value["payload"]["model"].as_str() {
                record.current_model = model.to_string();
            }
            continue;
        }
        let payload = &value["payload"];
        if payload["type"] != "token_count" {
            continue;
        }
        let info = &payload["info"];
        let total = Counts::from_json(&info["total_token_usage"]);
        let last = Counts::from_json(&info["last_token_usage"]);
        let counts = if last.is_empty() {
            total.delta_from(record.last_total)
        } else {
            last
        };
        record.last_total = total;
        record.rate_limits = payload
            .get("rate_limits")
            .cloned()
            .filter(|value| !value.is_null());
        record.plan_type = record
            .rate_limits
            .as_ref()
            .and_then(|limits| limits["plan_type"].as_str())
            .map(str::to_string);
        let ts = value["timestamp"]
            .as_str()
            .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
            .map(|timestamp| timestamp.with_timezone(&Utc).timestamp());
        record.last_ts = ts.or(record.last_ts);
        if !counts.is_empty() {
            if let Some(ts) = ts {
                record.events.push(TokenEvent {
                    ts,
                    model: if record.current_model.is_empty() {
                        "unknown".to_string()
                    } else {
                        record.current_model.clone()
                    },
                    counts,
                });
            }
        }
    }
    record.path = candidate.path.to_string_lossy().to_string();
    record.observed_size = candidate.size;
    record.modified_ms = candidate.modified_ms;
    record.processed_offset = offset;
    Ok(record)
}

pub fn collect() -> CodexUsage {
    let _guard = COLLECT_LOCK.lock().unwrap();
    let Some(root) = codex_root() else {
        return CodexUsage::default();
    };
    collect_from_root(&root, true)
}

fn collect_from_root(root: &Path, persist: bool) -> CodexUsage {
    let candidates = rollout_files(root);
    if candidates.is_empty() {
        return CodexUsage::default();
    }
    let mut cache: Cache = if persist {
        crate::history_cache::load("codex", CACHE_VERSION)
    } else {
        Cache::default()
    };
    let mut seen = HashSet::new();
    let mut scanned = 0u64;
    let mut cached = 0u64;
    let mut skipped = 0u64;
    let mut changed = false;

    for candidate in &candidates {
        seen.insert(candidate.key.clone());
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
    cache.files.retain(|key, _| seen.contains(key));
    changed |= before != cache.files.len();

    let mut usage = CodexUsage {
        available: true,
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

    if let Some(latest) = cache.files.values().max_by_key(|record| record.modified_ms) {
        usage.plan_type = latest.plan_type.clone();
        usage.updated_at = latest.last_ts;
        if let Some(limits) = &latest.rate_limits {
            usage.primary = gauge_from(&limits["primary"]);
            usage.secondary = gauge_from(&limits["secondary"]);
            usage.credits = limits["credits"].as_f64();
        }
    }

    let cutoff = Utc::now().timestamp() - 30 * 86400;
    let today = today_str();
    let mut per_day: BTreeMap<String, (u64, f64)> = BTreeMap::new();
    let mut per_model: BTreeMap<String, (u64, f64, EstimateConfidence)> = BTreeMap::new();
    let mut confidence = EstimateConfidence::High;
    let mut unknown_models = BTreeSet::new();

    for event in cache.files.values().flat_map(|record| &record.events) {
        if event.ts < cutoff {
            continue;
        }
        let tokens = event.counts.input + event.counts.output;
        let priced = pricing::codex_value(
            &event.model,
            event.counts.input,
            event.counts.cached,
            event.counts.output,
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
        usage.token_breakdown.input += event.counts.input;
        usage.token_breakdown.cached_input += event.counts.cached;
        usage.token_breakdown.output += event.counts.output;
        usage.token_breakdown.reasoning_output += event.counts.reasoning_output;
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
        if let Err(error) = crate::history_cache::save("codex", CACHE_VERSION, &cache) {
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

    #[test]
    fn fixture_uses_event_dates_and_cached_subset_semantics() {
        let root = std::env::temp_dir().join(format!("ai-usage-codex-{}", uuid::Uuid::new_v4()));
        let sessions = root.join("sessions/2026/07/15");
        fs::create_dir_all(&sessions).unwrap();
        fs::copy(
            Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/codex_token_count.jsonl"
            )),
            sessions.join("rollout-fixture.jsonl"),
        )
        .unwrap();
        let usage = collect_from_root(&root, false);
        assert_eq!(usage.tokens_30d, 1_800);
        assert_eq!(usage.token_breakdown.cached_input, 900);
        assert_eq!(usage.models[0].model, "gpt-5.5");
        assert_eq!(usage.history_health.files_scanned, 1);
        let used_days: Vec<_> = usage.daily.iter().filter(|day| day.tokens > 0).collect();
        assert_eq!(used_days.len(), 2);
        assert_ne!(used_days[0].date, used_days[1].date);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn active_and_archived_folders_are_both_discovered() {
        let root = std::env::temp_dir().join(format!("ai-usage-codex-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::create_dir_all(root.join("archived_sessions")).unwrap();
        let fixture = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/codex_token_count.jsonl"
        ));
        fs::copy(fixture, root.join("sessions/rollout-one.jsonl")).unwrap();
        fs::copy(fixture, root.join("archived_sessions/rollout-two.jsonl")).unwrap();
        assert_eq!(rollout_files(&root).len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unchanged_files_reuse_cache_and_growth_reads_only_the_append() {
        use std::io::Write;

        let root = std::env::temp_dir().join(format!("ai-usage-codex-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("sessions")).unwrap();
        let path = root.join("sessions/rollout-cache.jsonl");
        fs::copy(
            Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/codex_token_count.jsonl"
            )),
            &path,
        )
        .unwrap();
        let first_candidate = rollout_files(&root).remove(0);
        let first = scan_file(&first_candidate, None).unwrap();
        let first_offset = first.processed_offset;
        let warm = scan_file(&first_candidate, Some(first)).unwrap();
        assert_eq!(warm.events.len(), 2);
        assert_eq!(warm.processed_offset, first_offset);

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{\"timestamp\":\"2026-07-15T00:00:03Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":1600,\"cached_input_tokens\":950,\"output_tokens\":320}},\"last_token_usage\":{{\"input_tokens\":100,\"cached_input_tokens\":50,\"output_tokens\":20}}}}}}}}")
            .unwrap();
        drop(file);
        let grown_candidate = rollout_files(&root).remove(0);
        let grown = scan_file(&grown_candidate, Some(warm)).unwrap();
        assert_eq!(grown.events.len(), 3);
        assert!(grown.processed_offset > first_offset);
        assert_eq!(grown.events[2].counts.input, 100);
        let _ = fs::remove_dir_all(root);
    }
}
