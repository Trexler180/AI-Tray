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
use crate::models::{Gauge, ModelGauge};
use crate::util::human_until;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// How long a stale snapshot keeps serving while the endpoint is unreachable.
const LIVE_CACHE_SECS: i64 = 15 * 60;

struct Cached<T> {
    fetched_at: i64,
    value: T,
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

// ---------------- Claude ----------------

#[derive(Clone)]
pub struct ClaudeLive {
    pub five_hour: Option<Gauge>,
    pub seven_day: Option<Gauge>,
    pub seven_day_model: Option<ModelGauge>,
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

/// The model-scoped weekly window (e.g. Fable-only) from the `limits` array.
/// The legacy `seven_day_<model>` top-level fields are null on current
/// responses; the `weekly_scoped` entry in `limits` is where this lives now.
fn claude_model_weekly(v: &Value) -> Option<ModelGauge> {
    v["limits"].as_array()?.iter().find_map(|l| {
        if l["kind"] != "weekly_scoped" {
            return None;
        }
        let model = l["scope"]["model"]["display_name"].as_str()?;
        let used = l["percent"].as_f64()?;
        Some(ModelGauge {
            model: model.to_string(),
            gauge: gauge_from_pct(used, rfc3339_unix(&l["resets_at"]), 10080),
        })
    })
}

fn parse_claude(v: &Value) -> Option<ClaudeLive> {
    let live = ClaudeLive {
        five_hour: claude_window(&v["five_hour"], 300),
        seven_day: claude_window(&v["seven_day"], 10080),
        seven_day_model: claude_model_weekly(v),
    };
    // A response with no recognizable window is a failure, not "no limits";
    // treating it as success would poison the cache with an empty snapshot.
    (live.five_hour.is_some() || live.seven_day.is_some()).then_some(live)
}

/// Fetch one account's usage with a given access token, refreshing once via
/// `refresh` (which returns a fresh token) if the endpoint rejects it.
fn fetch_claude_for(
    token: Option<String>,
    refresh: impl FnOnce(&str) -> Option<String>,
) -> Option<ClaudeLive> {
    const URL: &str = "https://api.anthropic.com/api/oauth/usage";
    const HDR: [(&str, &str); 1] = [("anthropic-beta", "oauth-2025-04-20")];

    let token = token?;
    match get_json_retrying(URL, &token, &HDR) {
        Ok(v) => parse_claude(&v),
        Err(401) | Err(403) => {
            let fresh = refresh(&token)?;
            get_json_retrying(URL, &fresh, &HDR)
                .ok()
                .and_then(|v| parse_claude(&v))
        }
        Err(_) => None,
    }
}

/// Cache the fetched snapshot for `org`, or serve its last-good one within the
/// grace window when the fetch failed.
fn claude_cached(org: &str, fetched: Option<ClaudeLive>) -> Option<ClaudeLive> {
    let now = Utc::now().timestamp();
    let mut cache = claude_cache().lock().unwrap();
    match fetched {
        Some(live) => {
            cache.insert(
                org.to_string(),
                Cached {
                    fetched_at: now,
                    value: live.clone(),
                },
            );
            Some(live)
        }
        None => {
            let cached = cache.get(org)?;
            if now - cached.fetched_at > LIVE_CACHE_SECS {
                return None;
            }
            let live = ClaudeLive {
                five_hour: serve_cached_gauge(&cached.value.five_hour),
                seven_day: serve_cached_gauge(&cached.value.seven_day),
                seven_day_model: serve_cached_model_gauge(&cached.value.seven_day_model),
            };
            (live.five_hour.is_some() || live.seven_day.is_some()).then_some(live)
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
            let served = claude_cached(&acct.id, fetched);
            let live = served.is_some();
            let (five_hour, seven_day, seven_day_model) = served
                .map(|l| (l.five_hour, l.seven_day, l.seven_day_model))
                .unwrap_or((None, None, None));
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
    let live = CodexLive {
        plan_type: v["plan_type"].as_str().map(|s| s.to_string()),
        primary: codex_window(&rl["primary_window"]),
        secondary: codex_window(&rl["secondary_window"]),
    };
    (live.primary.is_some() || live.secondary.is_some()).then_some(live)
}

fn fetch_codex_live() -> Option<CodexLive> {
    const URL: &str = "https://chatgpt.com/backend-api/wham/usage";

    let auth_json = auth::read_json(&auth::codex_auth_path()?)?;
    let token = auth_json["tokens"]["access_token"].as_str()?.to_string();
    let account = auth_json["tokens"]["account_id"]
        .as_str()
        .unwrap_or("")
        .to_string();

    match get_json_retrying(URL, &token, &[("chatgpt-account-id", account.as_str())]) {
        Ok(v) => parse_codex(&v),
        Err(401) | Err(403) => {
            let (fresh, acc) = auth::refresh_codex_creds()?;
            get_json_retrying(URL, &fresh, &[("chatgpt-account-id", acc.as_str())])
                .ok()
                .and_then(|v| parse_codex(&v))
        }
        Err(_) => None,
    }
}

pub fn codex_live() -> Option<CodexLive> {
    let fetched = fetch_codex_live();
    let now = Utc::now().timestamp();
    let mut cache = CODEX_CACHE.lock().unwrap();
    match fetched {
        Some(live) => {
            *cache = Some(Cached {
                fetched_at: now,
                value: live.clone(),
            });
            Some(live)
        }
        None => {
            let cached = cache.as_ref()?;
            if now - cached.fetched_at > LIVE_CACHE_SECS {
                return None;
            }
            let live = CodexLive {
                plan_type: cached.value.plan_type.clone(),
                primary: serve_cached_gauge(&cached.value.primary),
                secondary: serve_cached_gauge(&cached.value.secondary),
            };
            (live.primary.is_some() || live.secondary.is_some()).then_some(live)
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
}
