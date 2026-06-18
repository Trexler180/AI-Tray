//! OAuth token handling shared with the official CLIs.
//!
//! Access tokens expire after a few hours. Both providers use ROTATING refresh
//! tokens: every refresh returns a new refresh token and invalidates the old
//! one. So after refreshing we MUST write the new tokens back into the same
//! credential files the CLIs read (`~/.claude/.credentials.json`,
//! `~/.codex/auth.json`) — otherwise the next CLI refresh fails and the user is
//! forced to log in again. Writes are atomic (temp file + rename).

use chrono::Utc;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_REFRESH_SKEW_MS: i64 = 2 * 60 * 1000;

static CLAUDE_REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(8))
        .build()
}

pub fn claude_creds_path() -> Option<PathBuf> {
    let mut p = dirs::home_dir()?;
    p.push(".claude");
    p.push(".credentials.json");
    Some(p)
}

pub fn codex_auth_path() -> Option<PathBuf> {
    let mut p = dirs::home_dir()?;
    p.push(".codex");
    p.push("auth.json");
    Some(p)
}

/// Read and parse a JSON file. The CLIs rewrite these credential files while
/// we read them, so a read can hit a Windows sharing violation or land on a
/// half-written file; brief retries beat a spurious "logged out".
pub fn read_json(path: &Path) -> Option<Value> {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(60));
        }
        match std::fs::read_to_string(path) {
            Ok(body) => {
                if let Ok(value) = serde_json::from_str(&body) {
                    return Some(value);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => {}
        }
    }
    None
}

/// Atomic write: serialize to a sibling temp file then rename over the target.
pub fn write_json_atomic(path: &Path, value: &Value) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp-aiusage");
    let body = serde_json::to_string_pretty(value)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

// ---------------- Claude ----------------

fn claude_refresh_lock() -> &'static Mutex<()> {
    CLAUDE_REFRESH_LOCK.get_or_init(|| Mutex::new(()))
}

fn claude_oauth(creds: &Value) -> Option<&Value> {
    creds.get("claudeAiOauth")
}

fn claude_access_from(creds: &Value) -> Option<String> {
    claude_oauth(creds)?
        .get("accessToken")?
        .as_str()
        .map(|s| s.to_string())
}

fn claude_token_expiring(creds: &Value) -> bool {
    let Some(expires_at) = claude_oauth(creds)
        .and_then(|oauth| oauth.get("expiresAt"))
        .and_then(|v| v.as_i64())
    else {
        return false;
    };
    expires_at <= Utc::now().timestamp_millis() + TOKEN_REFRESH_SKEW_MS
}

/// Exchange the Claude refresh token for fresh tokens. Transient failures
/// (network, 5xx, 429) get one retry; a 4xx means the refresh token itself
/// was rejected, where retrying can't help.
fn refresh_claude(refresh_token: &str) -> Option<Value> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLAUDE_CLIENT_ID,
    });
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(800));
        }
        match agent()
            .post(CLAUDE_TOKEN_URL)
            .set("Content-Type", "application/json")
            .send_json(body.clone())
        {
            Ok(resp) => return serde_json::from_str(&resp.into_string().ok()?).ok(),
            Err(ureq::Error::Status(code, _)) if (400..500).contains(&code) && code != 429 => {
                return None;
            }
            Err(_) => {}
        }
    }
    None
}

/// Tokens returned by a successful refresh exchange.
pub struct RefreshedTokens {
    pub access_token: String,
    /// Present only when the provider rotated the refresh token (it always does).
    pub refresh_token: Option<String>,
    /// Unix milliseconds, when known.
    pub expires_at: Option<i64>,
}

/// True when an access token with this expiry (unix ms) is close enough to
/// expiry that we should refresh it preemptively.
pub fn claude_token_is_expiring(expires_at_ms: i64) -> bool {
    expires_at_ms <= Utc::now().timestamp_millis() + TOKEN_REFRESH_SKEW_MS
}

/// Exchange a Claude refresh token for fresh tokens WITHOUT touching any
/// credential file. Used by the multi-account store to keep accounts the CLI
/// isn't currently tracking alive on its own. Returns None on a hard rejection
/// (the refresh token was invalidated) or a network failure.
pub fn refresh_claude_tokens(refresh_token: &str) -> Option<RefreshedTokens> {
    let tok = refresh_claude(refresh_token)?;
    Some(RefreshedTokens {
        access_token: tok["access_token"].as_str()?.to_string(),
        refresh_token: tok["refresh_token"].as_str().map(|s| s.to_string()),
        expires_at: tok["expires_in"]
            .as_i64()
            .map(|exp| Utc::now().timestamp_millis() + exp * 1000),
    })
}

fn refresh_claude_creds_locked(path: &Path, mut creds: Value) -> Option<String> {
    let rt = claude_oauth(&creds)?
        .get("refreshToken")?
        .as_str()?
        .to_string();
    let tok = refresh_claude(&rt)?;

    let access = tok["access_token"].as_str()?.to_string();
    let oauth = creds.get_mut("claudeAiOauth")?;
    oauth["accessToken"] = Value::from(access.clone());
    if let Some(new_rt) = tok["refresh_token"].as_str() {
        oauth["refreshToken"] = Value::from(new_rt);
    }
    if let Some(exp) = tok["expires_in"].as_i64() {
        let expires_at = Utc::now().timestamp_millis() + exp * 1000;
        oauth["expiresAt"] = Value::from(expires_at);
    }
    write_json_atomic(path, &creds).ok()?;
    Some(access)
}

/// Read a Claude access token, preemptively refreshing it when it is about to expire.
pub fn claude_access_token() -> Option<String> {
    let path = claude_creds_path()?;
    let creds = read_json(&path)?;
    if !claude_token_expiring(&creds) {
        return claude_access_from(&creds);
    }

    let _guard = claude_refresh_lock().lock().ok()?;
    let creds = read_json(&path)?;
    if !claude_token_expiring(&creds) {
        return claude_access_from(&creds);
    }
    refresh_claude_creds_locked(&path, creds).or_else(|| {
        read_json(&path)
            .as_ref()
            .and_then(|creds| claude_access_from(creds))
    })
}

/// Refresh after the usage endpoint rejected a token.
///
/// Claude refresh tokens rotate. If another refresh finished while this call
/// was waiting, use that newer access token instead of spending the fresh
/// refresh token again.
pub fn refresh_claude_creds_after_rejection(rejected_access: &str) -> Option<String> {
    let _guard = claude_refresh_lock().lock().ok()?;
    let path = claude_creds_path()?;
    let creds = read_json(&path)?;
    let current_access = claude_access_from(&creds)?;
    if current_access != rejected_access {
        return Some(current_access);
    }
    refresh_claude_creds_locked(&path, creds).or_else(|| {
        read_json(&path).and_then(|creds| {
            let current_access = claude_access_from(&creds)?;
            (current_access != rejected_access).then_some(current_access)
        })
    })
}

// ---------------- Codex ----------------

fn refresh_codex(refresh_token: &str) -> Option<Value> {
    // OpenAI's token endpoint expects form-urlencoded (matches the CLI).
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(800));
        }
        match agent().post(CODEX_TOKEN_URL).send_form(&[
            ("client_id", CODEX_CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", "openid profile email"),
        ]) {
            Ok(resp) => return serde_json::from_str(&resp.into_string().ok()?).ok(),
            Err(ureq::Error::Status(code, _)) if (400..500).contains(&code) && code != 429 => {
                return None;
            }
            Err(_) => {}
        }
    }
    None
}

/// Refresh the Codex credential file in place, returning (access_token, account_id).
pub fn refresh_codex_creds() -> Option<(String, String)> {
    let path = codex_auth_path()?;
    let mut auth = read_json(&path)?;
    let rt = auth["tokens"]["refresh_token"].as_str()?.to_string();
    let tok = refresh_codex(&rt)?;

    let access = tok["access_token"].as_str()?.to_string();
    let account = auth["tokens"]["account_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let tokens = auth.get_mut("tokens")?;
    tokens["access_token"] = Value::from(access.clone());
    if let Some(id) = tok["id_token"].as_str() {
        tokens["id_token"] = Value::from(id);
    }
    if let Some(new_rt) = tok["refresh_token"].as_str() {
        tokens["refresh_token"] = Value::from(new_rt);
    }
    auth["last_refresh"] = Value::from(Utc::now().to_rfc3339());
    let _ = write_json_atomic(&path, &auth);
    Some((access, account))
}
