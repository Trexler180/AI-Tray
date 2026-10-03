//! Rate-limit resets you can spend on demand, for both providers.
//!
//! Codex "free rate-limit reset" credits clear your Codex 5h/weekly limit.
//! They're exposed by a backend endpoint the desktop app uses:
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
//!
//! Claude banked resets are the CLI's `cedar_ember` program, handled the way
//! T3 Code does it: the grants ride along on the oauth usage response (`live`
//! asks for them with `cedar_ember=1`, so they cost no extra request), and one
//! is claimed with
//!
//! - POST https://api.anthropic.com/api/organizations/{org}/reset_rate_limits
//!   `{program, grant_id, request_id}` — `request_id` is an idempotency key, so
//!   a claim that went unanswered is retried as the *same* claim.
//!
//! The organization comes from the CLI's account record (`.claude.json`) for
//! that config directory.

use crate::auth::{self, RefreshError};
use crate::http::{agent, get_json_retrying};
use crate::models::{ClaudeResets, CodexResets, ResetCredit};
use crate::util::human_until;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

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

// ---------------- Claude ----------------

const CLAUDE_API: &str = "https://api.anthropic.com";
const CLAUDE_PROGRAM: &str = "cedar_ember";
/// Used when no installed CLI version can be found.
const CLAUDE_CLI_FALLBACK_VERSION: &str = "2.1.288";

/// The CLI identifies itself on these endpoints, and so do T3 Code and this
/// app. The version is the newest native install, read once.
pub fn claude_user_agent() -> &'static str {
    static UA: OnceLock<String> = OnceLock::new();
    UA.get_or_init(|| {
        let version = installed_claude_version()
            .unwrap_or_else(|| CLAUDE_CLI_FALLBACK_VERSION.to_string());
        format!("claude-cli/{version} (external, cli)")
    })
}

/// Newest version folder of the native installer (`~/.local/share/claude/versions`).
fn installed_claude_version() -> Option<String> {
    let dir = dirs::home_dir()?.join(".local/share/claude/versions");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| {
            let parts: Vec<u64> = name.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
            (parts.len() == 3).then_some((parts, name))
        })
        .max()
        .map(|(_, name)| name)
}

fn valid_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The `cedar_ember` block of an oauth usage response. None when the account
/// isn't eligible (or the block is missing or malformed); otherwise the count
/// of claimable resets and the grant to claim next. Grants that are paused, not
/// usable now, or past `ends_at` can't be claimed and don't count — and nothing
/// counts unless the server names one of the live grants as next.
pub fn parse_claude_resets(block: &Value, now: i64) -> Option<ClaudeResets> {
    if !block["eligible"].as_bool()? {
        return None;
    }
    let live: Vec<(&str, u64, Option<i64>)> = block["grants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|grant| {
            let id = grant["id"].as_str().filter(|id| valid_grant_id(id))?;
            let left = grant["resets_left"].as_u64()?;
            let ends_at = match &grant["ends_at"] {
                Value::Null => None,
                // A deadline that doesn't parse can't be shown to be in the
                // future, so the grant is treated as expired.
                value => Some(rfc3339_unix(value)?),
            };
            let paused = grant["paused"].as_bool().unwrap_or(false);
            let usable_now = grant["usable_now"].as_bool().unwrap_or(false);
            (!paused && usable_now && ends_at.is_none_or(|end| end > now))
                .then_some((id, left, ends_at))
        })
        .collect();
    let next_id = block["next_grant_id"].as_str();
    let Some(&(id, _, ends_at)) = live.iter().find(|(id, _, _)| Some(*id) == next_id) else {
        return Some(ClaudeResets::default());
    };
    Some(ClaudeResets {
        available_count: live.iter().map(|(_, left, _)| left).sum(),
        next_credit_id: Some(id.to_string()),
        next_expires_at: ends_at,
        next_expires_in: ends_at.map(human_until),
    })
}

/// A cached reset block made ready to serve: the countdown recomputed, and
/// nothing left to claim once the next grant's deadline has passed.
pub fn serve_cached_claude_resets(resets: &ClaudeResets, now: i64) -> ClaudeResets {
    if resets.next_expires_at.is_some_and(|end| end <= now) {
        return ClaudeResets::default();
    }
    let mut resets = resets.clone();
    resets.next_expires_in = resets.next_expires_at.map(human_until);
    resets
}

/// Why a claim didn't go through. The wording is T3 Code's.
#[derive(Debug, PartialEq, Eq)]
enum ClaimError {
    MalformedCredit,
    UnknownAccount,
    LoginUnreadable,
    AccountUnreadable,
    SignedOut,
    RateLimited,
    CoolingDown,
    Unconfirmed,
    RequestFailed,
}

impl ClaimError {
    fn message(&self) -> &'static str {
        match self {
            Self::MalformedCredit => "Claude returned a malformed reset credit.",
            Self::UnknownAccount => "That Claude account is no longer tracked.",
            Self::LoginUnreadable => "Claude could not read its login.",
            Self::AccountUnreadable => "Claude could not read its account.",
            Self::SignedOut => "Sign in to Claude again to redeem resets.",
            Self::RateLimited => "Claude is rate limiting resets. Try again soon.",
            Self::CoolingDown => "Claude resets are cooling down. Try again later.",
            Self::Unconfirmed => {
                "Claude could not confirm the reset. If you are still limited in a moment, try again."
            }
            Self::RequestFailed => "Claude could not redeem the reset.",
        }
    }

    /// Every failure except these two is final: Claude answered, or nothing
    /// was sent. An unanswered or unconfirmed claim keeps its request id, so
    /// trying again is the same claim rather than a second one.
    fn settled(&self) -> bool {
        !matches!(self, Self::RequestFailed | Self::Unconfirmed)
    }
}

/// Per-account pending request id. Held across the claim, so two clicks on the
/// same account can't race, and kept after an unsettled failure for the retry.
type ClaimSlot = Arc<Mutex<Option<String>>>;
static CLAIMS: OnceLock<Mutex<HashMap<String, ClaimSlot>>> = OnceLock::new();

fn claim_slot(account_id: &str) -> ClaimSlot {
    CLAIMS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry(account_id.to_string())
        .or_default()
        .clone()
}

/// Where the CLI keeps the account record for a config directory: beside its
/// settings for an explicit `CLAUDE_CONFIG_DIR`, in the home directory for the
/// default `~/.claude`.
fn claude_account_config_path(dir: &std::path::Path, is_default: bool) -> Option<PathBuf> {
    if is_default {
        Some(dirs::home_dir()?.join(".claude.json"))
    } else {
        Some(dir.join(".claude.json"))
    }
}

/// POST one claim. Ok(result) is the server's verdict string.
fn post_claim(token: &str, org: &str, grant_id: &str, request_id: &str) -> Result<String, u16> {
    let url = format!(
        "{CLAUDE_API}/api/organizations/{}/reset_rate_limits",
        org.trim()
    );
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(25))
        .build()
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("User-Agent", claude_user_agent())
        .send_json(serde_json::json!({
            "program": CLAUDE_PROGRAM,
            "grant_id": grant_id,
            "request_id": request_id,
        }));
    match response {
        Ok(response) => {
            let body: Value = serde_json::from_str(&response.into_string().map_err(|_| 0u16)?)
                .map_err(|_| 0u16)?;
            body["result"].as_str().map(str::to_string).ok_or(0)
        }
        Err(ureq::Error::Status(code, _)) => Err(code),
        Err(_) => Err(0),
    }
}

/// The server's verdict as the outcome the panel reports. `reset` is the only
/// one that changed anything; the rest leave the grant where it was.
fn claim_outcome(result: &str) -> Result<&'static str, ClaimError> {
    match result {
        "reset" => Ok("reset"),
        "not_limited" => Ok("nothingToReset"),
        "already_used" => Ok("alreadyRedeemed"),
        "ineligible" => Ok("noCredit"),
        "cooldown" => Err(ClaimError::CoolingDown),
        "unavailable" => Err(ClaimError::Unconfirmed),
        _ => Err(ClaimError::RequestFailed),
    }
}

fn claim_claude_reset(
    account_id: &str,
    grant_id: &str,
    request_id: &str,
) -> Result<&'static str, ClaimError> {
    let account = crate::accounts::list()
        .into_iter()
        .find(|account| account.id == account_id)
        .ok_or(ClaimError::UnknownAccount)?;
    let creds = crate::accounts::creds_file(&account.dir);
    if creds.is_file() && auth::read_json(&creds).is_none() {
        return Err(ClaimError::LoginUnreadable);
    }
    let token = auth::claude_access_token_at(&creds).ok_or(ClaimError::SignedOut)?;
    let config = claude_account_config_path(&account.dir, !account.removable)
        .ok_or(ClaimError::AccountUnreadable)?;
    let org = match auth::read_json(&config) {
        Some(record) => record["oauthAccount"]["organizationUuid"]
            .as_str()
            .map(|org| org.trim().to_string())
            .filter(|org| !org.is_empty()),
        None if config.exists() => return Err(ClaimError::AccountUnreadable),
        None => None,
    }
    .ok_or(ClaimError::SignedOut)?;

    let verdict = match post_claim(&token, &org, grant_id, request_id) {
        Err(401) | Err(403) => {
            // Same request id, so this is still the one claim.
            let fresh = auth::refresh_claude_creds_after_rejection_at(&creds, &token)
                .ok_or(ClaimError::SignedOut)?;
            post_claim(&fresh, &org, grant_id, request_id)
        }
        other => other,
    };
    match verdict {
        Ok(result) => claim_outcome(&result),
        Err(429) => Err(ClaimError::RateLimited),
        Err(401) | Err(403) => Err(ClaimError::SignedOut),
        Err(_) => Err(ClaimError::RequestFailed),
    }
}

/// Claim a banked Claude reset for one account. Spends the reset — only call
/// this in response to an explicit user action. Returns the outcome:
/// `reset`, `nothingToReset`, `alreadyRedeemed` or `noCredit`.
pub fn consume_claude_reset(account_id: &str, grant_id: &str) -> Result<&'static str, String> {
    if !valid_grant_id(grant_id) {
        return Err(ClaimError::MalformedCredit.message().to_string());
    }
    let slot = claim_slot(account_id);
    let mut pending = slot.lock().unwrap_or_else(|error| error.into_inner());
    let request_id = pending
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    let result = claim_claude_reset(account_id, grant_id, &request_id);
    if result.as_ref().err().is_none_or(ClaimError::settled) {
        *pending = None;
    }
    drop(pending);
    // Whatever happened, the account's limits and grants are worth asking
    // about again now rather than after the usual gap.
    crate::live::force_claude_refetch(account_id);
    result.map_err(|error| error.message().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_790_000_000; // 2026-09-21

    #[test]
    fn claude_resets_count_live_grants_when_next_is_live() {
        let block = json!({
            "eligible": true,
            "next_grant_id": "tenure",
            "grants": [
                {"id": "tenure", "resets_left": 2, "ends_at": "2026-10-30T00:00:00Z", "usable_now": true},
                {"id": "other", "resets_left": 1, "ends_at": null, "usable_now": true},
                {"id": "paused", "resets_left": 5, "paused": true, "usable_now": true},
                {"id": "not_now", "resets_left": 5, "usable_now": false},
                {"id": "expired", "resets_left": 5, "ends_at": "2026-01-01T00:00:00Z", "usable_now": true},
                {"id": "BAD ID", "resets_left": 5, "usable_now": true}
            ]
        });
        let r = parse_claude_resets(&block, NOW).expect("eligible");
        assert_eq!(r.available_count, 3);
        assert_eq!(r.next_credit_id.as_deref(), Some("tenure"));
        assert!(r.next_expires_at.is_some());
        assert!(r.next_expires_in.is_some());
    }

    #[test]
    fn claude_resets_without_a_live_next_grant_count_nothing() {
        let block = json!({
            "eligible": true,
            "next_grant_id": "paused",
            "grants": [
                {"id": "paused", "resets_left": 1, "paused": true, "usable_now": true},
                {"id": "other", "resets_left": 1, "usable_now": true}
            ]
        });
        let r = parse_claude_resets(&block, NOW).expect("eligible");
        assert_eq!(r.available_count, 0);
        assert!(r.next_credit_id.is_none());
    }

    #[test]
    fn ineligible_or_missing_claude_block_is_none() {
        assert!(parse_claude_resets(&json!({"eligible": false}), NOW).is_none());
        assert!(parse_claude_resets(&Value::Null, NOW).is_none());
        assert!(parse_claude_resets(&json!({"grants": []}), NOW).is_none());
    }

    #[test]
    fn cached_claude_resets_go_empty_once_the_next_grant_expires() {
        let resets = ClaudeResets {
            available_count: 1,
            next_credit_id: Some("tenure".into()),
            next_expires_at: Some(NOW + 60),
            next_expires_in: None,
        };
        assert_eq!(serve_cached_claude_resets(&resets, NOW).available_count, 1);
        assert!(serve_cached_claude_resets(&resets, NOW).next_expires_in.is_some());
        assert_eq!(serve_cached_claude_resets(&resets, NOW + 61).available_count, 0);
    }

    #[test]
    fn claim_verdicts_map_to_outcomes() {
        assert_eq!(claim_outcome("reset"), Ok("reset"));
        assert_eq!(claim_outcome("not_limited"), Ok("nothingToReset"));
        assert_eq!(claim_outcome("already_used"), Ok("alreadyRedeemed"));
        assert_eq!(claim_outcome("ineligible"), Ok("noCredit"));
        assert_eq!(claim_outcome("cooldown"), Err(ClaimError::CoolingDown));
        assert_eq!(claim_outcome("unavailable"), Err(ClaimError::Unconfirmed));
        assert_eq!(claim_outcome("???"), Err(ClaimError::RequestFailed));
        // Only an unanswered or unconfirmed claim keeps its request id.
        assert!(!ClaimError::Unconfirmed.settled());
        assert!(!ClaimError::RequestFailed.settled());
        assert!(ClaimError::CoolingDown.settled());
    }

    #[test]
    fn grant_ids_are_checked_before_anything_is_sent() {
        assert!(valid_grant_id("tenure"));
        assert!(valid_grant_id("grant_01-x"));
        assert!(!valid_grant_id(""));
        assert!(!valid_grant_id("Upper"));
        assert!(!valid_grant_id(&"a".repeat(41)));
        assert!(consume_claude_reset("nobody", "../x").is_err());
    }

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
