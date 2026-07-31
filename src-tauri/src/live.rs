//! Live usage straight from the same endpoints the official CLIs use.
//!
//! Claude: GET https://api.anthropic.com/api/oauth/usage
//!         Bearer <accessToken>, anthropic-beta: oauth-2025-04-20
//! Codex:  GET https://chatgpt.com/backend-api/wham/usage
//!         Bearer <access_token>, chatgpt-account-id: <account_id>
//!
//! On a 401 we refresh the token (rotating, written back to the cred file via
//! `auth`) and retry once.
//!
//! Every fetch failure falls back to the last good snapshot for a grace
//! period, so one dropped request, a 429, or a token-refresh race with the
//! CLI doesn't flip the UI and the alert pipeline to "logs only".

use crate::auth;
use crate::models::{DataHealth, DataSource, ExtraUsage, Gauge, ModelGauge, QuotaWindow};
use crate::util::{codex_window_meta, codex_window_meta_by_slot, human_until};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// How long a stale snapshot keeps serving while the endpoint is unreachable.
const LIVE_CACHE_SECS: i64 = 15 * 60;

struct Cached<T> {
    fetched_at: i64,
    value: T,
}

#[derive(Clone)]
struct FetchFailure {
    kind: &'static str,
    message: String,
}

impl FetchFailure {
    fn status(status: u16) -> Self {
        let (kind, message) = match status {
            0 => ("network", "Network request failed".to_string()),
            401 | 403 => ("authorization", "Provider rejected the saved credentials".to_string()),
            429 => ("rate_limited", "Provider temporarily rate-limited refreshes".to_string()),
            500..=599 => ("provider", format!("Provider returned HTTP {status}")),
            _ => ("http", format!("Provider returned HTTP {status}")),
        };
        Self { kind, message }
    }

    fn missing_credentials() -> Self {
        Self {
            kind: "missing_credentials",
            message: "No usable provider credentials were found".to_string(),
        }
    }
}

fn live_health(now: i64) -> DataHealth {
    DataHealth {
        source: DataSource::LiveApi,
        fetched_at: Some(now),
        attempted_at: Some(now),
        stale_age_seconds: Some(0),
        ..Default::default()
    }
}

fn failed_health(now: i64, failure: &FetchFailure) -> DataHealth {
    DataHealth {
        source: DataSource::Unavailable,
        attempted_at: Some(now),
        error_kind: Some(failure.kind.to_string()),
        error_message: Some(failure.message.clone()),
        ..Default::default()
    }
}

fn cached_health(now: i64, fetched_at: i64, failure: &FetchFailure) -> DataHealth {
    DataHealth {
        source: DataSource::MemoryCache,
        fetched_at: Some(fetched_at),
        attempted_at: Some(now),
        stale_age_seconds: Some((now - fetched_at).max(0)),
        error_kind: Some(failure.kind.to_string()),
        error_message: Some(failure.message.clone()),
        ..Default::default()
    }
}

/// Per-account Claude snapshots, keyed by config-directory path, so a stale
/// account keeps serving its own last-good gauges independently of the others.
static CLAUDE_CACHE: OnceLock<Mutex<HashMap<String, Cached<ClaudeLive>>>> = OnceLock::new();
static CODEX_CACHE: Mutex<Option<Cached<CodexLive>>> = Mutex::new(None);

fn claude_cache() -> &'static Mutex<HashMap<String, Cached<ClaudeLive>>> {
    CLAUDE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build()
}

/// GET with bearer auth. Returns Ok(json), or Err(status) where 401 signals
/// an expired token and 0 signals a transport error.
fn get_json(url: &str, token: &str, extra: &[(&str, &str)]) -> Result<Value, u16> {
    let mut req = agent()
        .get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json");
    for (k, v) in extra {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(resp) => {
            let body = resp.into_string().map_err(|_| 0u16)?;
            serde_json::from_str(&body).map_err(|_| 0u16)
        }
        Err(ureq::Error::Status(code, _)) => Err(code),
        Err(_) => Err(0),
    }
}

/// Like get_json, but retries transport errors and 5xx with backoff
/// (3 attempts total). Auth errors and 429 pass straight through.
fn get_json_retrying(url: &str, token: &str, extra: &[(&str, &str)]) -> Result<Value, u16> {
    let mut delay_ms = 500;
    for _ in 0..2 {
        match get_json(url, token, extra) {
            Err(0) | Err(500) | Err(502) | Err(503) | Err(504) => {
                std::thread::sleep(Duration::from_millis(delay_ms));
                delay_ms *= 3;
            }
            other => return other,
        }
    }
    get_json(url, token, extra)
}

fn gauge_from_pct(used: f64, resets_at_unix: Option<i64>, window_minutes: i64) -> Gauge {
    Gauge {
        used_percent: used,
        window_minutes,
        resets_at: resets_at_unix,
        resets_in: resets_at_unix.map(human_until),
    }
}

/// Prepare a cached gauge for serving: drop it once its reset time has passed
/// (the cached used_percent is then known to be wrong) and recompute the
/// human countdown.
fn serve_cached_gauge(gauge: &Option<Gauge>) -> Option<Gauge> {
    let gauge = gauge.as_ref()?;
    if let Some(resets_at) = gauge.resets_at {
        if resets_at <= Utc::now().timestamp() {
            return None;
        }
    }
    let mut gauge = gauge.clone();
    gauge.resets_in = gauge.resets_at.map(human_until);
    Some(gauge)
}

/// `serve_cached_gauge` for a model-scoped gauge, keeping its model name.
fn serve_cached_model_gauge(mg: &Option<ModelGauge>) -> Option<ModelGauge> {
    let mg = mg.as_ref()?;
    let gauge = serve_cached_gauge(&Some(mg.gauge.clone()))?;
    Some(ModelGauge {
        model: mg.model.clone(),
        gauge,
    })
}

fn serve_cached_quota(quota: &QuotaWindow) -> Option<QuotaWindow> {
    let mut quota = quota.clone();
    quota.gauge = serve_cached_gauge(&Some(quota.gauge))?;
    Some(quota)
}

// ---------------- Claude ----------------

#[derive(Clone)]
pub struct ClaudeLive {
    pub five_hour: Option<Gauge>,
    pub seven_day: Option<Gauge>,
    pub seven_day_model: Option<ModelGauge>,
    pub quotas: Vec<QuotaWindow>,
    pub extra_usage: Option<ExtraUsage>,
}

/// One account's live gauges, ready for the UI/alerts layer.
pub struct ClaudeAccountLive {
    pub id: String,
    pub label: String,
    pub subscription_type: Option<String>,
    /// The built-in `~/.claude` account.
    pub active: bool,
    /// A user-added directory (removable); false for the built-in `~/.claude`.
    pub removable: bool,
    /// True when these gauges are fresh or within the stale-serve grace window.
    pub live: bool,
    pub five_hour: Option<Gauge>,
    pub seven_day: Option<Gauge>,
    pub seven_day_model: Option<ModelGauge>,
    pub quotas: Vec<QuotaWindow>,
    pub extra_usage: Option<ExtraUsage>,
    pub health: DataHealth,
}

fn rfc3339_unix(v: &Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|d| d.timestamp())
}

fn claude_window(node: &Value, window_minutes: i64) -> Option<Gauge> {
    if node.is_null() {
        return None;
    }
    let used = node["utilization"].as_f64()?;
    Some(gauge_from_pct(
        used,
        rfc3339_unix(&node["resets_at"]),
        window_minutes,
    ))
}

fn scope_name(scope: &Value, key: &str) -> Option<String> {
    let value = &scope[key];
    value["display_name"]
        .as_str()
        .or_else(|| value["id"].as_str())
        .or_else(|| value.as_str())
        .map(str::to_string)
}

fn quota_id(kind: &str, model: Option<&str>, surface: Option<&str>) -> String {
    let clean = |s: &str| {
        s.to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    };
    match (model, surface) {
        (Some(model), Some(surface)) => format!("{kind}:{}:{}", clean(model), clean(surface)),
        (Some(model), None) => format!("{kind}:{}", clean(model)),
        (None, Some(surface)) => format!("{kind}:{}", clean(surface)),
        (None, None) => kind.to_string(),
    }
}

fn claude_quotas(v: &Value) -> Vec<QuotaWindow> {
    let mut quotas = Vec::new();
    if let Some(limits) = v["limits"].as_array() {
        for limit in limits {
            let Some(used) = limit["percent"].as_f64() else {
                continue;
            };
            let kind = limit["kind"].as_str().unwrap_or("quota");
            let group = limit["group"].as_str().unwrap_or(kind);
            let model = scope_name(&limit["scope"], "model");
            let surface = scope_name(&limit["scope"], "surface");
            let label = if let Some(model) = &model {
                format!("Weekly ({model})")
            } else if let Some(surface) = &surface {
                format!("Weekly ({surface})")
            } else if group == "session" || kind == "session" {
                "Session (5h)".to_string()
            } else if group == "weekly" || kind.contains("weekly") {
                "Weekly".to_string()
            } else {
                kind.replace('_', " ")
            };
            let window_minutes = if group == "session" || kind == "session" {
                300
            } else if group == "weekly" || kind.contains("weekly") {
                10080
            } else {
                0
            };
            quotas.push(QuotaWindow {
                id: quota_id(kind, model.as_deref(), surface.as_deref()),
                label,
                kind: kind.to_string(),
                group: group.to_string(),
                scope_model: model,
                scope_surface: surface,
                gauge: gauge_from_pct(used, rfc3339_unix(&limit["resets_at"]), window_minutes),
            });
        }
    }
    quotas
}

/// The `extra_usage` block (usage credits). Amounts arrive in cents and are
/// converted to dollars here, matching what the UI displays.
fn parse_extra_usage(v: &Value) -> Option<ExtraUsage> {
    let node = &v["extra_usage"];
    if !node.is_object() {
        return None;
    }
    Some(ExtraUsage {
        is_enabled: node["is_enabled"].as_bool().unwrap_or(false),
        monthly_limit: node["monthly_limit"].as_f64().map(|cents| cents / 100.0),
        used_credits: node["used_credits"].as_f64().map(|cents| cents / 100.0),
        utilization: node["utilization"].as_f64(),
        currency: node["currency"].as_str().map(str::to_string),
    })
}

fn parse_claude(v: &Value) -> Option<ClaudeLive> {
    let five_hour = claude_window(&v["five_hour"], 300);
    let seven_day = claude_window(&v["seven_day"], 10080);
    let mut quotas = claude_quotas(v);
    if quotas.is_empty() {
        if let Some(gauge) = &five_hour {
            quotas.push(QuotaWindow {
                id: "session".to_string(),
                label: "Session (5h)".to_string(),
                kind: "session".to_string(),
                group: "session".to_string(),
                gauge: gauge.clone(),
                ..Default::default()
            });
        }
        if let Some(gauge) = &seven_day {
            quotas.push(QuotaWindow {
                id: "weekly_all".to_string(),
                label: "Weekly".to_string(),
                kind: "weekly_all".to_string(),
                group: "weekly".to_string(),
                gauge: gauge.clone(),
                ..Default::default()
            });
        }
    }
    let seven_day_model = quotas.iter().find_map(|quota| {
        quota.scope_model.as_ref().map(|model| ModelGauge {
            model: model.clone(),
            gauge: quota.gauge.clone(),
        })
    });
    let live = ClaudeLive {
        five_hour,
        seven_day,
        seven_day_model,
        quotas,
        extra_usage: parse_extra_usage(v),
    };
    // A response with no recognizable window is a failure, not "no limits";
    // treating it as success would poison the cache with an empty snapshot.
    (live.five_hour.is_some() || live.seven_day.is_some() || !live.quotas.is_empty())
        .then_some(live)
}

/// Fetch one account's usage with a given access token, refreshing once via
/// `refresh` (which returns a fresh token) if the endpoint rejects it.
fn fetch_claude_for(
    token: Option<String>,
    refresh: impl FnOnce(&str) -> Option<String>,
) -> Result<ClaudeLive, FetchFailure> {
    const URL: &str = "https://api.anthropic.com/api/oauth/usage";
    const HDR: [(&str, &str); 1] = [("anthropic-beta", "oauth-2025-04-20")];

    let token = token.ok_or_else(FetchFailure::missing_credentials)?;
    match get_json_retrying(URL, &token, &HDR) {
        Ok(v) => parse_claude(&v).ok_or_else(|| FetchFailure {
            kind: "decode",
            message: "Provider response did not contain recognized quota windows".to_string(),
        }),
        Err(401) | Err(403) => {
            let fresh = refresh(&token).ok_or_else(|| FetchFailure::status(401))?;
            get_json_retrying(URL, &fresh, &HDR)
                .map_err(FetchFailure::status)
                .and_then(|v| parse_claude(&v).ok_or_else(|| FetchFailure {
                    kind: "decode",
                    message: "Provider response did not contain recognized quota windows".to_string(),
                }))
        }
        Err(status) => Err(FetchFailure::status(status)),
    }
}

/// Cache the fetched snapshot for `org`, or serve its last-good one within the
/// grace window when the fetch failed.
fn claude_cached(
    org: &str,
    fetched: Result<ClaudeLive, FetchFailure>,
) -> (Option<ClaudeLive>, DataHealth) {
    let now = Utc::now().timestamp();
    let mut cache = claude_cache().lock().unwrap();
    match fetched {
        Ok(live) => {
            cache.insert(
                org.to_string(),
                Cached {
                    fetched_at: now,
                    value: live.clone(),
                },
            );
            (Some(live), live_health(now))
        }
        Err(failure) => {
            let Some(cached) = cache.get(org) else {
                return (None, failed_health(now, &failure));
            };
            if now - cached.fetched_at > LIVE_CACHE_SECS {
                return (None, failed_health(now, &failure));
            }
            let live = ClaudeLive {
                five_hour: serve_cached_gauge(&cached.value.five_hour),
                seven_day: serve_cached_gauge(&cached.value.seven_day),
                seven_day_model: serve_cached_model_gauge(&cached.value.seven_day_model),
                quotas: cached
                    .value
                    .quotas
                    .iter()
                    .filter_map(serve_cached_quota)
                    .collect(),
                // Credit spend has no reset boundary inside the grace window,
                // so the cached snapshot serves as-is.
                extra_usage: cached.value.extra_usage.clone(),
            };
            let value = (live.five_hour.is_some()
                || live.seven_day.is_some()
                || !live.quotas.is_empty())
            .then_some(live);
            (value, cached_health(now, cached.fetched_at, &failure))
        }
    }
}

/// Live gauges for every known Claude account.
///
/// Each account is a config directory: its `.credentials.json` is read, used to
/// fetch the live gauges, and (on a 401) refreshed in place in that same file —
/// so the CLI pointed at that directory keeps working with the rotated token.
pub fn claude_live_accounts() -> Vec<ClaudeAccountLive> {
    let mut out: Vec<ClaudeAccountLive> = crate::accounts::list()
        .into_iter()
        .map(|acct| {
            let creds = crate::accounts::creds_file(&acct.dir);
            let fetched = fetch_claude_for(auth::claude_access_token_at(&creds), |t| {
                auth::refresh_claude_creds_after_rejection_at(&creds, t)
            });
            let (served, health) = claude_cached(&acct.id, fetched);
            let live = served.is_some();
            let (five_hour, seven_day, seven_day_model, quotas, extra_usage) = served
                .map(|l| {
                    (
                        l.five_hour,
                        l.seven_day,
                        l.seven_day_model,
                        l.quotas,
                        l.extra_usage,
                    )
                })
                .unwrap_or((None, None, None, Vec::new(), None));
            ClaudeAccountLive {
                id: acct.id,
                label: acct.label,
                subscription_type: acct.subscription_type,
                active: !acct.removable,
                removable: acct.removable,
                live,
                five_hour,
                seven_day,
                seven_day_model,
                quotas,
                extra_usage,
                health,
            }
        })
        .collect();

    // Default account first, then alphabetically, so the UI order is stable.
    out.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.label.cmp(&b.label)));
    out
}

// ---------------- Codex ----------------

#[derive(Clone)]
pub struct CodexLive {
    pub plan_type: Option<String>,
    pub primary: Option<Gauge>,
    pub secondary: Option<Gauge>,
    pub quotas: Vec<QuotaWindow>,
}

fn codex_window(node: &Value) -> Option<Gauge> {
    if node.is_null() {
        return None;
    }
    let used = node["used_percent"].as_f64()?;
    let window_minutes = node["limit_window_seconds"].as_i64().unwrap_or(0) / 60;
    Some(gauge_from_pct(
        used,
        node["reset_at"].as_i64(),
        window_minutes,
    ))
}

fn parse_codex(v: &Value) -> Option<CodexLive> {
    let rl = &v["rate_limit"];
    let mut quotas = Vec::new();
    let mut used_ids = HashSet::new();
    if let Some(windows) = rl.as_object() {
        for (key, node) in windows {
            if !key.ends_with("_window") {
                continue;
            }
            let Some(gauge) = codex_window(node) else {
                continue;
            };
            let slot = key.trim_end_matches("_window");
            // Named from the window's own length, not its slot: Codex moves
            // limits between slots when it turns one on or off.
            let (mut id, label, group) = codex_window_meta(gauge.window_minutes)
                .unwrap_or_else(|| codex_window_meta_by_slot(slot));
            if !used_ids.insert(id.clone()) {
                id = format!("{id}:{slot}");
                used_ids.insert(id.clone());
            }
            quotas.push(QuotaWindow {
                id,
                label,
                kind: key.clone(),
                group: group.to_string(),
                gauge,
                ..Default::default()
            });
        }
    }
    let by_group = |group: &str| {
        quotas
            .iter()
            .find(|quota| quota.group == group)
            .map(|quota| quota.gauge.clone())
    };
    let live = CodexLive {
        plan_type: v["plan_type"].as_str().map(|s| s.to_string()),
        primary: by_group("session"),
        secondary: by_group("weekly"),
        quotas,
    };
    (live.primary.is_some() || live.secondary.is_some() || !live.quotas.is_empty())
        .then_some(live)
}

fn fetch_codex_live() -> Result<CodexLive, FetchFailure> {
    const URL: &str = "https://chatgpt.com/backend-api/wham/usage";

    let path = auth::codex_auth_path().ok_or_else(FetchFailure::missing_credentials)?;
    let auth_json = auth::read_json(&path).ok_or_else(FetchFailure::missing_credentials)?;
    let token = auth_json["tokens"]["access_token"]
        .as_str()
        .ok_or_else(FetchFailure::missing_credentials)?
        .to_string();
    let account = auth_json["tokens"]["account_id"]
        .as_str()
        .unwrap_or("")
        .to_string();

    match get_json_retrying(URL, &token, &[("chatgpt-account-id", account.as_str())]) {
        Ok(v) => parse_codex(&v).ok_or_else(|| FetchFailure {
            kind: "decode",
            message: "Provider response did not contain recognized quota windows".to_string(),
        }),
        Err(401) | Err(403) => {
            let (fresh, acc) =
                auth::refresh_codex_creds().ok_or_else(|| FetchFailure::status(401))?;
            get_json_retrying(URL, &fresh, &[("chatgpt-account-id", acc.as_str())])
                .map_err(FetchFailure::status)
                .and_then(|v| parse_codex(&v).ok_or_else(|| FetchFailure {
                    kind: "decode",
                    message: "Provider response did not contain recognized quota windows".to_string(),
                }))
        }
        Err(status) => Err(FetchFailure::status(status)),
    }
}

pub fn codex_live() -> (Option<CodexLive>, DataHealth) {
    let fetched = fetch_codex_live();
    let now = Utc::now().timestamp();
    let mut cache = CODEX_CACHE.lock().unwrap();
    match fetched {
        Ok(live) => {
            *cache = Some(Cached {
                fetched_at: now,
                value: live.clone(),
            });
            (Some(live), live_health(now))
        }
        Err(failure) => {
            let Some(cached) = cache.as_ref() else {
                return (None, failed_health(now, &failure));
            };
            if now - cached.fetched_at > LIVE_CACHE_SECS {
                return (None, failed_health(now, &failure));
            }
            let live = CodexLive {
                plan_type: cached.value.plan_type.clone(),
                primary: serve_cached_gauge(&cached.value.primary),
                secondary: serve_cached_gauge(&cached.value.secondary),
                quotas: cached
                    .value
                    .quotas
                    .iter()
                    .filter_map(serve_cached_quota)
                    .collect(),
            };
            let value = (live.primary.is_some()
                || live.secondary.is_some()
                || !live.quotas.is_empty())
            .then_some(live);
            (value, cached_health(now, cached.fetched_at, &failure))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape observed from the live /api/oauth/usage endpoint: the top-level
    /// `seven_day_<model>` fields are null and the model-scoped weekly window
    /// lives in `limits` as a `weekly_scoped` entry.
    #[test]
    fn parses_model_scoped_weekly_from_limits() {
        let v: Value = serde_json::from_str(
            r#"{
              "five_hour": {"utilization": 12.0, "resets_at": "2026-07-05T18:59:59+00:00"},
              "seven_day": {"utilization": 55.0, "resets_at": "2026-07-09T02:59:59+00:00"},
              "seven_day_opus": null,
              "limits": [
                {"kind": "session", "group": "session", "percent": 12,
                 "resets_at": "2026-07-05T18:59:59+00:00", "scope": null},
                {"kind": "weekly_all", "group": "weekly", "percent": 55,
                 "resets_at": "2026-07-09T02:59:59+00:00", "scope": null},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 68,
                 "resets_at": "2026-07-09T02:59:59+00:00",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}}
              ]
            }"#,
        )
        .unwrap();

        let live = parse_claude(&v).expect("parses");
        assert_eq!(live.five_hour.as_ref().unwrap().used_percent, 12.0);
        assert_eq!(live.seven_day.as_ref().unwrap().used_percent, 55.0);
        let mg = live.seven_day_model.expect("model-scoped weekly present");
        assert_eq!(mg.model, "Fable");
        assert_eq!(mg.gauge.used_percent, 68.0);
        assert_eq!(mg.gauge.window_minutes, 10080);
        assert!(mg.gauge.resets_at.is_some());
        assert_eq!(live.quotas.len(), 3);
    }

    /// A plan without a model-scoped limit (no `limits`, or none scoped) still
    /// parses; the extra gauge is simply absent.
    #[test]
    fn missing_scoped_limit_is_none() {
        let v: Value = serde_json::from_str(
            r#"{"five_hour": {"utilization": 1.0, "resets_at": "2026-07-05T18:59:59+00:00"},
                "seven_day": {"utilization": 2.0, "resets_at": "2026-07-09T02:59:59+00:00"}}"#,
        )
        .unwrap();
        let live = parse_claude(&v).expect("parses");
        assert!(live.seven_day_model.is_none());
    }

    /// `extra_usage` amounts arrive in cents; the parsed struct carries
    /// dollars. A missing block parses to None, and a disabled block still
    /// surfaces so the UI can say credits exist but are switched off.
    #[test]
    fn parses_extra_usage_cents_to_dollars() {
        let v: Value = serde_json::from_str(
            r#"{
              "five_hour": {"utilization": 12.0, "resets_at": "2026-07-05T18:59:59+00:00"},
              "extra_usage": {"is_enabled": true, "monthly_limit": 2000,
                              "used_credits": 680, "utilization": 34.0, "currency": "USD"}
            }"#,
        )
        .unwrap();
        let live = parse_claude(&v).expect("parses");
        let extra = live.extra_usage.expect("extra usage present");
        assert!(extra.is_enabled);
        assert_eq!(extra.monthly_limit, Some(20.0));
        assert_eq!(extra.used_credits, Some(6.8));
        assert_eq!(extra.utilization, Some(34.0));
        assert_eq!(extra.currency.as_deref(), Some("USD"));

        let v: Value = serde_json::from_str(
            r#"{"five_hour": {"utilization": 1.0, "resets_at": "2026-07-05T18:59:59+00:00"},
                "extra_usage": {"is_enabled": false, "monthly_limit": null,
                                "used_credits": null, "utilization": null}}"#,
        )
        .unwrap();
        let extra = parse_claude(&v).unwrap().extra_usage.expect("block present");
        assert!(!extra.is_enabled);
        assert_eq!(extra.monthly_limit, None);

        let v: Value = serde_json::from_str(
            r#"{"five_hour": {"utilization": 1.0, "resets_at": "2026-07-05T18:59:59+00:00"}}"#,
        )
        .unwrap();
        assert!(parse_claude(&v).unwrap().extra_usage.is_none());
    }

    /// Both windows on: named by their own lengths, not their slots.
    #[test]
    fn parses_both_codex_windows() {
        let v: Value = serde_json::from_str(
            r#"{"plan_type": "pro", "rate_limit": {
                 "primary_window": {"used_percent": 12.0, "limit_window_seconds": 18000,
                                    "reset_at": 1784077200},
                 "secondary_window": {"used_percent": 44.0, "limit_window_seconds": 604800,
                                      "reset_at": 1784592000}}}"#,
        )
        .unwrap();
        let live = parse_codex(&v).expect("parses");
        assert_eq!(live.quotas.len(), 2);
        let labels: Vec<_> = live.quotas.iter().map(|q| q.label.as_str()).collect();
        assert!(labels.contains(&"Session (5h)"));
        assert!(labels.contains(&"Weekly"));
        assert_eq!(live.primary.as_ref().unwrap().used_percent, 12.0);
        assert_eq!(live.secondary.as_ref().unwrap().used_percent, 44.0);
    }

    /// The 5h limit switched off: the weekly window is the only one reported
    /// and it arrives in the *primary* slot. It must still read "Weekly", and
    /// must not leave a phantom session gauge behind.
    #[test]
    fn lone_weekly_window_in_the_primary_slot_is_labelled_weekly() {
        let v: Value = serde_json::from_str(
            r#"{"plan_type": "pro", "rate_limit": {
                 "primary_window": {"used_percent": 61.0, "limit_window_seconds": 604800,
                                    "reset_at": 1784592000},
                 "secondary_window": null}}"#,
        )
        .unwrap();
        let live = parse_codex(&v).expect("parses");
        assert_eq!(live.quotas.len(), 1);
        assert_eq!(live.quotas[0].id, "weekly");
        assert_eq!(live.quotas[0].label, "Weekly");
        assert_eq!(live.quotas[0].group, "weekly");
        assert!(live.primary.is_none());
        assert_eq!(live.secondary.as_ref().unwrap().used_percent, 61.0);
    }

    /// No duration reported: fall back to the slot, and don't invent an hour
    /// count for the session bar.
    #[test]
    fn codex_window_without_a_duration_falls_back_to_its_slot() {
        let v: Value = serde_json::from_str(
            r#"{"rate_limit": {"primary_window": {"used_percent": 5.0, "reset_at": 1784077200}}}"#,
        )
        .unwrap();
        let live = parse_codex(&v).expect("parses");
        assert_eq!(live.quotas[0].label, "Session");
        assert_eq!(live.quotas[0].group, "session");
    }

    /// Two windows of the same length would collide on one id, which alert
    /// state and display prefs key off; the slot disambiguates them.
    #[test]
    fn same_length_codex_windows_keep_distinct_ids() {
        let v: Value = serde_json::from_str(
            r#"{"rate_limit": {
                 "primary_window": {"used_percent": 1.0, "limit_window_seconds": 604800},
                 "secondary_window": {"used_percent": 2.0, "limit_window_seconds": 604800}}}"#,
        )
        .unwrap();
        let live = parse_codex(&v).expect("parses");
        let ids: std::collections::HashSet<_> = live.quotas.iter().map(|q| &q.id).collect();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn cached_health_keeps_failure_and_age() {
        let health = cached_health(1_120, 1_000, &FetchFailure::status(429));
        assert_eq!(health.source, DataSource::MemoryCache);
        assert_eq!(health.stale_age_seconds, Some(120));
        assert_eq!(health.error_kind.as_deref(), Some("rate_limited"));
        assert_eq!(health.fetched_at, Some(1_000));
        assert_eq!(health.attempted_at, Some(1_120));
    }

    #[test]
    fn parses_every_model_and_surface_scoped_limit() {
        let value: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/claude_usage.json"
        ))
        .unwrap();
        let live = parse_claude(&value).expect("fixture parses");
        assert_eq!(live.quotas.len(), 4);
        let ids: std::collections::HashSet<_> =
            live.quotas.iter().map(|quota| &quota.id).collect();
        assert_eq!(ids.len(), 4);
        assert!(live
            .quotas
            .iter()
            .any(|quota| quota.scope_surface.as_deref() == Some("Cowork")));
    }
}
