//! Codex "free rate-limit reset" credits.
//!
//! These one-time credits clear your Codex 5h/weekly limit. They're exposed by
//! a backend endpoint the desktop app uses:
//!
//! - GET  https://chatgpt.com/backend-api/wham/rate-limit-reset-credits
//!   (read-only — lists credits with hidden granted_at/expires_at dates)
//! - POST https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume
//!   (spends one credit — only ever called from an explicit user action)
//!
//! Both are authed with the same rotating Codex OAuth token the rest of the app
//! manages (`auth::refresh_codex_creds_after_rejection`). On a 401 we refresh
//! once and retry.
//!
//! A successful fetch is cached briefly so one dropped request doesn't blank the
//! UI or make the alert pipeline think every credit vanished.

use crate::auth::{self, RefreshError};
use crate::http::{agent, get_json_retrying};
use crate::models::{CodexResets, ResetCredit};
use crate::util::human_until;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::sync::{Mutex, MutexGuard};

const RESETS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CONSUME_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";

/// How long a stale snapshot keeps serving while the endpoint is unreachable.
const CACHE_SECS: i64 = 15 * 60;

struct Cached {
    fetched_at: i64,
    value: CodexResets,
}

static RESETS_CACHE: Mutex<Option<Cached>> = Mutex::new(None);

fn cache() -> MutexGuard<'static, Option<Cached>> {
    RESETS_CACHE.lock().unwrap_or_else(|error| error.into_inner())
}

/// The headers the Codex desktop app attaches to wham reset-credit calls,
/// beyond Authorization.
fn reset_headers(account: &str) -> [(&'static str, &str); 7] {
    [
        ("Accept", "application/json"),
        ("Content-Type", "application/json"),
        ("OAI-Language", "en"),
        ("X-OpenAI-Attach-Auth", "1"),
        ("X-OpenAI-Attach-Integrity-State", "1"),
        ("originator", "Codex Desktop"),
        ("chatgpt-account-id", account),
    ]
}

fn get_retrying(token: &str, account: &str) -> Result<Value, u16> {
    get_json_retrying(RESETS_URL, token, &reset_headers(account))
}

fn rfc3339_unix(v: &Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|d| d.timestamp())
}

fn parse_credit(v: &Value) -> ResetCredit {
    let expires_at = rfc3339_unix(&v["expires_at"]);
    ResetCredit {
        id: v["id"].as_str().unwrap_or_default().to_string(),
        status: v["status"].as_str().unwrap_or_default().to_string(),
        title: v["title"].as_str().map(|s| s.to_string()),
        description: v["description"].as_str().map(|s| s.to_string()),
        granted_at: rfc3339_unix(&v["granted_at"]),
        expires_at,
        expires_in: expires_at.map(human_until),
        redeem_started_at: rfc3339_unix(&v["redeem_started_at"]),
        redeemed_at: rfc3339_unix(&v["redeemed_at"]),
    }
}

/// Parse the full response. Any well-formed JSON object counts as success —
/// an empty `credits` array is the legitimate "you have no resets" state, not
/// a failure.
fn parse(v: &Value) -> Option<CodexResets> {
    if !v.is_object() {
        return None;
    }
    let credits = v["credits"]
        .as_array()
        .map(|arr| arr.iter().map(parse_credit).collect())
        .unwrap_or_default();
    let available_count = v["available_count"].as_u64().unwrap_or(0);
    Some(CodexResets {
        available: true,
        available_count,
        credits,
    })
}

/// Fetch reset credits, refreshing the token once on a 401/403.
fn fetch() -> Option<CodexResets> {
    let (token, account) = auth::codex_token()?;
    match get_retrying(&token, &account) {
        Ok(v) => parse(&v),
        Err(401) | Err(403) => {
            let (fresh, acc) = auth::refresh_codex_creds_after_rejection(&token).ok()?;
            get_retrying(&fresh, &acc).ok().and_then(|v| parse(&v))
        }
        Err(_) => None,
    }
}

/// Recompute the human "expires_in" countdown on a cached snapshot so a served
/// stale value still shows an accurate time remaining, and drop credits whose
/// expiry has already passed (the cached `available` state is then stale).
fn refreshed_cache(value: &CodexResets) -> CodexResets {
    let now = Utc::now().timestamp();
    let credits: Vec<ResetCredit> = value
        .credits
        .iter()
        .filter(|c| c.expires_at.map(|e| e > now).unwrap_or(true))
        .map(|c| {
            let mut c = c.clone();
            c.expires_in = c.expires_at.map(human_until);
            c
        })
        .collect();
    let available_count = credits
        .iter()
        .filter(|c| c.status == "available")
        .count() as u64;
    CodexResets {
        available: true,
        available_count,
        credits,
    }
}

/// Live reset credits, serving the last-good snapshot within a grace window
/// when the endpoint is briefly unreachable.
pub fn codex_resets() -> Option<CodexResets> {
    let fetched = fetch();
    let now = Utc::now().timestamp();
    let mut cache = cache();
    match fetched {
        Some(value) => {
            *cache = Some(Cached {
                fetched_at: now,
                value: value.clone(),
            });
            Some(value)
        }
        None => {
            let cached = cache.as_ref()?;
            if now - cached.fetched_at > CACHE_SECS {
                return None;
            }
            Some(refreshed_cache(&cached.value))
        }
    }
}

/// Consume (redeem) a reset credit. Spends the credit — only call this in
/// response to an explicit user action. Refreshes the token once on a 401/403.
pub fn consume_codex_reset(credit_id: &str) -> Result<(), String> {
    if credit_id.is_empty() {
        return Err("missing credit id".to_string());
    }
    let (token, account) =
        auth::codex_token().ok_or_else(|| "Codex auth unavailable".to_string())?;
    let redeem_request_id = uuid::Uuid::new_v4().to_string();
    let body = serde_json::json!({
        "credit_id": credit_id,
        "redeem_request_id": redeem_request_id,
    });

    let post = |token: &str, account: &str| -> Result<(), u16> {
        let mut req = agent()
            .post(CONSUME_URL)
            .set("Authorization", &format!("Bearer {token}"));
        for (k, v) in reset_headers(account) {
            req = req.set(k, v);
        }
        match req.send_json(body.clone()) {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, _)) => Err(code),
            Err(_) => Err(0),
        }
    };

    let result = match post(&token, &account) {
        Ok(()) => Ok(()),
        Err(401) | Err(403) => {
            let (fresh, acc) =
                auth::refresh_codex_creds_after_rejection(&token).map_err(|error| match error {
                    RefreshError::SaveFailed(error) => {
                        format!("token refreshed but could not be saved: {error}")
                    }
                    _ => "token refresh failed".to_string(),
                })?;
            post(&fresh, &acc).map_err(|code| format!("consume failed (HTTP {code})"))
        }
        Err(code) => Err(format!("consume failed (HTTP {code})")),
    };
    // Invalidate the cache so the next fetch reflects the spent credit.
    if result.is_ok() {
        *cache() = None;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_full_response() {
        let v = json!({
            "available_count": 1,
            "credits": [{
                "id": "RateLimitResetCredit_abc",
                "status": "available",
                "title": "One free rate limit reset",
                "description": "Thanks!",
                "granted_at": "2026-06-18T00:36:49.561200Z",
                "expires_at": "2026-07-18T00:36:49.561200Z",
                "redeem_started_at": null,
                "redeemed_at": null
            }]
        });
        let r = parse(&v).expect("parse");
        assert!(r.available);
        assert_eq!(r.available_count, 1);
        assert_eq!(r.credits.len(), 1);
        let c = &r.credits[0];
        assert_eq!(c.id, "RateLimitResetCredit_abc");
        assert_eq!(c.status, "available");
        assert!(c.granted_at.is_some());
        assert!(c.expires_at.is_some());
        assert!(c.expires_in.is_some());
        assert!(c.redeemed_at.is_none());
    }

    #[test]
    fn empty_credits_is_success_not_failure() {
        let v = json!({ "available_count": 0, "credits": [] });
        let r = parse(&v).expect("parse");
        assert!(r.available);
        assert_eq!(r.available_count, 0);
        assert!(r.credits.is_empty());
    }

    #[test]
    fn non_object_is_failure() {
        assert!(parse(&json!("nope")).is_none());
        assert!(parse(&json!(null)).is_none());
    }
}
