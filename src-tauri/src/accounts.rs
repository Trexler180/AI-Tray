//! App-owned multi-account store for Claude.
//!
//! The CLI only ever keeps ONE account in `~/.claude/.credentials.json`;
//! logging into another overwrites it. To show live gauges for more than one
//! account we keep our own copy of each account's OAuth tokens here, keyed by
//! the stable `organizationUuid`, and accumulate accounts as the user logs into
//! them (the credentials file is already watched, so a login is captured within
//! seconds).
//!
//! Refresh tokens ROTATE. For the account that is *currently active* on disk we
//! never refresh from this store — `live.rs` goes through the normal
//! `~/.credentials.json` flow so the CLI stays in sync. For *inactive* accounts
//! we own the rotation: refreshes are written back here only, never to the
//! shared credentials file (that would clobber whichever account the CLI is
//! using right now).
//!
//! `capture_active` runs at the top of every collection, so an active account's
//! stored tokens are re-synced from the CLI's file each cycle; a stale stored
//! copy therefore self-heals on the next refresh.

use crate::auth;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

#[derive(Serialize, Deserialize, Clone, Default)]
struct AccountStore {
    #[serde(default)]
    accounts: Vec<StoredAccount>,
}

#[derive(Serialize, Deserialize, Clone)]
struct StoredAccount {
    org_uuid: String,
    /// User-chosen display name. None falls back to a derived label.
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    subscription_type: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
    access_token: String,
    refresh_token: String,
    /// Access-token expiry, unix milliseconds.
    #[serde(default)]
    expires_at: i64,
    /// Unix seconds this account was last seen active on disk.
    #[serde(default)]
    last_seen: i64,
}

/// A known account, with its label resolved, for the rest of the app.
#[derive(Clone)]
pub struct ClaudeAccount {
    pub org_uuid: String,
    pub label: String,
    pub subscription_type: Option<String>,
}

static STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Serializes every read-modify-write of the store file. Two collections can
/// run at once (a panel refresh alongside an alert poll); without this they
/// could both refresh the same inactive account and double-spend its rotating
/// refresh token, invalidating one of them.
fn lock() -> MutexGuard<'static, ()> {
    STORE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn store_path() -> Option<PathBuf> {
    let mut root = dirs::config_dir().or_else(dirs::home_dir)?;
    root.push("AI Usage Tray");
    root.push("claude-accounts.json");
    Some(root)
}

fn load() -> AccountStore {
    store_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_default()
}

fn save(store: &AccountStore) -> std::io::Result<()> {
    let path = store_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no config dir"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp-aiusage");
    fs::write(&tmp, serde_json::to_string_pretty(store)?)?;
    fs::rename(&tmp, &path)
}

fn pretty_plan(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn default_label(acc: &StoredAccount) -> String {
    let kind = acc
        .subscription_type
        .as_deref()
        .map(pretty_plan)
        .unwrap_or_else(|| "Claude".to_string());
    let short: String = acc.org_uuid.chars().take(4).collect();
    if short.is_empty() || acc.org_uuid == "default" {
        kind
    } else {
        format!("{kind} · {short}")
    }
}

fn resolved_label(acc: &StoredAccount) -> String {
    acc.label
        .as_ref()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .unwrap_or_else(|| default_label(acc))
}

/// Refresh an inactive account's tokens via the rotating refresh token and
/// persist the result. Caller holds the store lock. Returns the new access
/// token, or None if the refresh token was rejected / the network failed
/// (the stale entry is left in place so a later re-login can re-capture it).
fn refresh_locked(store: &mut AccountStore, idx: usize) -> Option<String> {
    let rt = store.accounts[idx].refresh_token.clone();
    let fresh = auth::refresh_claude_tokens(&rt)?;
    let acc = &mut store.accounts[idx];
    acc.access_token = fresh.access_token.clone();
    if let Some(new_rt) = fresh.refresh_token {
        acc.refresh_token = new_rt;
    }
    if let Some(exp) = fresh.expires_at {
        acc.expires_at = exp;
    }
    let _ = save(store);
    Some(fresh.access_token)
}

/// Read the account currently in `~/.claude/.credentials.json`, store/refresh
/// our copy of it, and return its org UUID (the "active" account). Returns None
/// when no usable credentials are present.
pub fn capture_active() -> Option<String> {
    let path = auth::claude_creds_path()?;
    let creds = auth::read_json(&path)?;
    let oauth = creds.get("claudeAiOauth")?;
    let access = oauth.get("accessToken")?.as_str()?.to_string();
    let refresh = oauth.get("refreshToken")?.as_str()?.to_string();

    let org = creds
        .get("organizationUuid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0);
    let subscription_type = oauth
        .get("subscriptionType")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let rate_limit_tier = oauth
        .get("rateLimitTier")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let _guard = lock();
    let mut store = load();
    let now = Utc::now().timestamp();
    if let Some(acc) = store.accounts.iter_mut().find(|a| a.org_uuid == org) {
        acc.access_token = access;
        acc.refresh_token = refresh;
        acc.expires_at = expires_at;
        if subscription_type.is_some() {
            acc.subscription_type = subscription_type;
        }
        if rate_limit_tier.is_some() {
            acc.rate_limit_tier = rate_limit_tier;
        }
        acc.last_seen = now;
    } else {
        store.accounts.push(StoredAccount {
            org_uuid: org.clone(),
            label: None,
            subscription_type,
            rate_limit_tier,
            access_token: access,
            refresh_token: refresh,
            expires_at,
            last_seen: now,
        });
    }
    let _ = save(&store);
    Some(org)
}

/// Every account we know about, labels resolved.
pub fn list() -> Vec<ClaudeAccount> {
    let _guard = lock();
    load()
        .accounts
        .iter()
        .map(|a| ClaudeAccount {
            org_uuid: a.org_uuid.clone(),
            label: resolved_label(a),
            subscription_type: a.subscription_type.clone(),
        })
        .collect()
}

/// A valid access token for an inactive account, refreshing (and persisting)
/// it first if it is at or near expiry.
pub fn access_token(org_uuid: &str) -> Option<String> {
    let _guard = lock();
    let mut store = load();
    let idx = store.accounts.iter().position(|a| a.org_uuid == org_uuid)?;
    if !auth::claude_token_is_expiring(store.accounts[idx].expires_at) {
        return Some(store.accounts[idx].access_token.clone());
    }
    refresh_locked(&mut store, idx)
}

/// Refresh an inactive account after the usage endpoint rejected its token.
/// If another collection refreshed it while we waited, use that newer token
/// instead of spending the rotating refresh token again.
pub fn refresh_after_rejection(org_uuid: &str, rejected_access: &str) -> Option<String> {
    let _guard = lock();
    let mut store = load();
    let idx = store.accounts.iter().position(|a| a.org_uuid == org_uuid)?;
    if store.accounts[idx].access_token != rejected_access {
        return Some(store.accounts[idx].access_token.clone());
    }
    refresh_locked(&mut store, idx)
}

/// Set (or clear, when empty) the user-facing label for an account.
pub fn set_label(org_uuid: &str, label: &str) -> Result<(), String> {
    let _guard = lock();
    let mut store = load();
    let acc = store
        .accounts
        .iter_mut()
        .find(|a| a.org_uuid == org_uuid)
        .ok_or_else(|| "unknown account".to_string())?;
    let trimmed = label.trim();
    acc.label = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    };
    save(&store).map_err(|e| e.to_string())
}

/// Drop an account from the store. If it happens to be the active one it will
/// simply be re-captured on the next collection; this is mainly for clearing
/// out a stale account whose refresh token no longer works.
pub fn forget(org_uuid: &str) -> Result<(), String> {
    let _guard = lock();
    let mut store = load();
    store.accounts.retain(|a| a.org_uuid != org_uuid);
    save(&store).map_err(|e| e.to_string())
}
