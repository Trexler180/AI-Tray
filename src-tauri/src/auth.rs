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
static CODEX_REFRESH_LOCK: Mutex<()> = Mutex::new(());

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(8))
        .build()
}

pub fn codex_auth_path() -> Option<PathBuf> {
    Some(crate::util::codex_home()?.join("auth.json"))
}

/// Why a token refresh produced no usable token.
#[derive(Debug)]
pub enum RefreshError {
    /// No credentials file, or no refresh token in it.
    Missing,
    /// The provider refused the refresh token, or never answered.
    Rejected,
    /// The provider issued new tokens but they could not be written back. The
    /// old refresh token is already spent at this point, so the CLI sharing
    /// this file will be signed out — which is worth saying out loud rather
    /// than reporting as an ordinary authorization failure.
    SaveFailed(std::io::Error),
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
///
/// Retried briefly, for the same reason `read_json` is: the CLIs hold these
/// files open while they read them, and the rename over the target can hit a
/// sharing violation. A refreshed token that fails to land is a spent refresh
/// token, so a few more milliseconds here are well spent.
pub fn write_json_atomic(path: &Path, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(value)?;
    let mut attempt = 0;
    loop {
        match crate::util::write_atomic(path, body.as_bytes()) {
            Ok(()) => return Ok(()),
            Err(_) if attempt < 3 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error),
        }
    }
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

/// Read a Claude access token from a specific credentials file, preemptively
/// refreshing it (in place, in that same file) when it is about to expire.
pub fn claude_access_token_at(path: &Path) -> Option<String> {
    let creds = read_json(path)?;
    if !claude_token_expiring(&creds) {
        return claude_access_from(&creds);
    }

    let _guard = claude_refresh_lock().lock().ok()?;
    let creds = read_json(path)?;
    if !claude_token_expiring(&creds) {
        return claude_access_from(&creds);
    }
    refresh_claude_creds_locked(path, creds)
        .or_else(|| read_json(path).as_ref().and_then(claude_access_from))
}

/// Refresh a specific credentials file after the usage endpoint rejected its
/// token.
///
/// Claude refresh tokens rotate. If another refresh finished while this call
/// was waiting, use that newer access token instead of spending the fresh
/// refresh token again.
pub fn refresh_claude_creds_after_rejection_at(
    path: &Path,
    rejected_access: &str,
) -> Option<String> {
    let _guard = claude_refresh_lock().lock().ok()?;
    let creds = read_json(path)?;
    let current_access = claude_access_from(&creds)?;
    if current_access != rejected_access {
        return Some(current_access);
    }
    refresh_claude_creds_locked(path, creds).or_else(|| {
        read_json(path).and_then(|creds| {
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

/// The access token and account id currently saved in `auth.json`.
pub fn codex_token() -> Option<(String, String)> {
    let auth = read_json(&codex_auth_path()?)?;
    codex_tokens_from(&auth)
}

fn codex_tokens_from(auth: &Value) -> Option<(String, String)> {
    let access = auth["tokens"]["access_token"].as_str()?.to_string();
    let account = auth["tokens"]["account_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    Some((access, account))
}

/// Refresh the Codex credential file in place after an endpoint rejected
/// `rejected_access`, returning (access_token, account_id).
///
/// Codex refresh tokens rotate, and the usage and reset-credit fetches run in
/// parallel on the same token — so they are rejected at the same moment and
/// would both spend the same refresh token. Serialising here and re-reading
/// under the lock means the second caller finds the first one's fresh token
/// and uses it instead of spending the (now dead) refresh token again.
pub fn refresh_codex_creds_after_rejection(
    rejected_access: &str,
) -> Result<(String, String), RefreshError> {
    let _guard = CODEX_REFRESH_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let path = codex_auth_path().ok_or(RefreshError::Missing)?;
    let mut auth = read_json(&path).ok_or(RefreshError::Missing)?;
    let (current_access, account) = codex_tokens_from(&auth).ok_or(RefreshError::Missing)?;
    if current_access != rejected_access {
        // Someone else — the other fetch, or the CLI itself — already
        // refreshed while this call waited.
        return Ok((current_access, account));
    }

    let rt = auth["tokens"]["refresh_token"]
        .as_str()
        .ok_or(RefreshError::Missing)?
        .to_string();
    let tok = refresh_codex(&rt).ok_or(RefreshError::Rejected)?;
    let access = tok["access_token"]
        .as_str()
        .ok_or(RefreshError::Rejected)?
        .to_string();

    let tokens = auth.get_mut("tokens").ok_or(RefreshError::Missing)?;
    tokens["access_token"] = Value::from(access.clone());
    if let Some(id) = tok["id_token"].as_str() {
        tokens["id_token"] = Value::from(id);
    }
    if let Some(new_rt) = tok["refresh_token"].as_str() {
        tokens["refresh_token"] = Value::from(new_rt);
    }
    auth["last_refresh"] = Value::from(Utc::now().to_rfc3339());
    write_json_atomic(&path, &auth).map_err(RefreshError::SaveFailed)?;
    Ok((access, account))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race this lock exists for: two fetches are rejected with the same
    /// token, the first refreshes, and the second must pick up the result
    /// rather than spend the refresh token a second time. Reaching the second
    /// caller's view directly — the file already holds a newer token than the
    /// one that was rejected — needs no network and must not attempt any.
    #[test]
    fn a_token_someone_else_already_refreshed_is_reused() {
        let home = std::env::temp_dir().join(format!("ai-usage-codex-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("auth.json"),
            r#"{"tokens":{"access_token":"fresh","refresh_token":"rt-2","account_id":"acct"}}"#,
        )
        .unwrap();
        // Only this test reads CODEX_HOME, so setting it here can't disturb
        // the others running alongside it.
        std::env::set_var("CODEX_HOME", &home);

        let (access, account) = refresh_codex_creds_after_rejection("stale").unwrap();
        assert_eq!(access, "fresh");
        assert_eq!(account, "acct");
        // Untouched: nothing was refreshed or rewritten.
        let saved = std::fs::read_to_string(home.join("auth.json")).unwrap();
        assert!(saved.contains("rt-2"));

        std::env::remove_var("CODEX_HOME");
        let _ = std::fs::remove_dir_all(home);
    }
}
