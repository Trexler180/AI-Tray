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
use crate::models::Gauge;
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

// ---------------- Claude ----------------

#[derive(Clone)]
pub struct ClaudeLive {
    pub five_hour: Option<Gauge>,
    pub seven_day: Option<Gauge>,
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

fn parse_claude(v: &Value) -> Option<ClaudeLive> {
    let live = ClaudeLive {
        five_hour: claude_window(&v["five_hour"], 300),
        seven_day: claude_window(&v["seven_day"], 10080),
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
            let (five_hour, seven_day) = served
                .map(|l| (l.five_hour, l.seven_day))
                .unwrap_or((None, None));
            ClaudeAccountLive {
                id: acct.id,
                label: acct.label,
                subscription_type: acct.subscription_type,
                active: !acct.removable,
                removable: acct.removable,
                live,
                five_hour,
                seven_day,
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
