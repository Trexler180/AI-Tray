//! Claude accounts, modeled as credential *directories*.
//!
//! The CLI keeps one login in `<config-dir>/.credentials.json`. The default
//! config dir is `~/.claude`, but a user can run several CLIs against separate
//! `CLAUDE_CONFIG_DIR` folders, each holding an independent login. We never see
//! a stable account id inside the credentials file (current CLIs write no
//! `organizationUuid`), so the *directory path* is the account's identity.
//!
//! The default `~/.claude` is always present; extra directories are registered
//! by the user and stored in `claude-dirs.json`. Each directory is read and its
//! rotating token refreshed in place in its own file (handled by `auth`/`live`),
//! so both the app and the CLI pointed at that folder stay in sync.

use crate::auth;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

#[derive(Serialize, Deserialize, Clone, Default)]
struct DirStore {
    /// User-added config directories. The default `~/.claude` is implicit and
    /// never listed here.
    #[serde(default)]
    added: Vec<String>,
    /// Optional display label per directory path (the default dir included).
    #[serde(default)]
    labels: HashMap<String, String>,
}

/// A known account (one per config directory), with its label resolved.
#[derive(Clone)]
pub struct ClaudeAccount {
    /// Stable identity and display key: the config directory path.
    pub id: String,
    /// The config directory (`id` as a path).
    pub dir: PathBuf,
    pub label: String,
    pub subscription_type: Option<String>,
    /// False for the built-in `~/.claude`, which can't be removed (only the
    /// extra directories the user added can be).
    pub removable: bool,
}

static STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Serializes read-modify-write of the directory list.
fn lock() -> MutexGuard<'static, ()> {
    STORE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn store_path() -> Option<PathBuf> {
    let mut root = crate::util::config_dir()?;
    root.push("claude-dirs.json");
    Some(root)
}

fn load() -> DirStore {
    store_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_default()
}

fn save(store: &DirStore) -> std::io::Result<()> {
    let path = store_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no config dir"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp-aiusage");
    fs::write(&tmp, serde_json::to_string_pretty(store)?)?;
    fs::rename(&tmp, &path)
}

/// The built-in default Claude config directory, `~/.claude`.
pub fn home_dir() -> Option<PathBuf> {
    let mut p = dirs::home_dir()?;
    p.push(".claude");
    Some(p)
}

/// The credentials file inside a Claude config directory.
pub fn creds_file(dir: &Path) -> PathBuf {
    dir.join(".credentials.json")
}

/// Trim surrounding whitespace/quotes and any trailing path separators.
fn clean(p: &str) -> String {
    p.trim()
        .trim_matches('"')
        .trim()
        .trim_end_matches(['/', '\\'])
        .to_string()
}

/// A case-insensitive, separator-insensitive form for comparing two paths.
fn norm(p: &str) -> String {
    clean(p).replace('/', "\\").to_lowercase()
}

/// Every config directory we know about: the default `~/.claude` first, then
/// each user-added one (deduped, the default never duplicated).
pub fn account_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(home) = home_dir() {
        seen.insert(norm(&home.to_string_lossy()));
        out.push(home);
    }
    for p in load().added {
        if seen.insert(norm(&p)) {
            out.push(PathBuf::from(clean(&p)));
        }
    }
    out
}

fn pretty_plan(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn read_subscription(dir: &Path) -> Option<String> {
    let creds = auth::read_json(&creds_file(dir))?;
    creds
        .get("claudeAiOauth")?
        .get("subscriptionType")?
        .as_str()
        .map(str::to_string)
}

fn resolved_label(
    store: &DirStore,
    id: &str,
    dir: &Path,
    subscription: Option<&str>,
    is_home: bool,
    only_home: bool,
) -> String {
    if let Some(custom) = store
        .labels
        .iter()
        .find(|(k, _)| norm(k) == norm(id))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
    {
        return custom.to_string();
    }
    let kind = subscription
        .map(pretty_plan)
        .unwrap_or_else(|| "Claude".to_string());
    // With a single account the plain plan name reads cleanest; once there's
    // more than one, append the folder name so they're distinguishable.
    if is_home && only_home {
        return kind;
    }
    match dir.file_name().and_then(|s| s.to_str()) {
        Some(name) if !name.is_empty() => format!("{kind} · {name}"),
        _ => kind,
    }
}

/// Every account we know about, labels resolved. The default `~/.claude` is
/// listed even when its credentials are missing, so the management UI is always
/// reachable.
pub fn list() -> Vec<ClaudeAccount> {
    let store = load();
    let only_home = store.added.is_empty();
    let home_norm = home_dir().map(|h| norm(&h.to_string_lossy()));
    account_dirs()
        .into_iter()
        .map(|dir| {
            let id = clean(&dir.to_string_lossy());
            let is_home = home_norm.as_deref() == Some(norm(&id).as_str());
            let subscription_type = read_subscription(&dir);
            let label =
                resolved_label(&store, &id, &dir, subscription_type.as_deref(), is_home, only_home);
            ClaudeAccount {
                id,
                dir,
                label,
                subscription_type,
                removable: !is_home,
            }
        })
        .collect()
}

/// Register an extra config directory. Validates that the folder exists and
/// holds a `.credentials.json`, and rejects the default dir and duplicates.
pub fn add_dir(path: &str) -> Result<(), String> {
    let cleaned = clean(path);
    if cleaned.is_empty() {
        return Err("Enter a folder path.".into());
    }
    let dir = PathBuf::from(&cleaned);
    if !dir.is_dir() {
        return Err("That folder doesn't exist.".into());
    }
    if !creds_file(&dir).is_file() {
        return Err("No .credentials.json there — sign in to that folder with Claude Code first.".into());
    }
    let n = norm(&cleaned);
    if home_dir().map(|h| norm(&h.to_string_lossy())) == Some(n.clone()) {
        return Err("That's the default ~/.claude folder — it's already shown.".into());
    }
    let _guard = lock();
    let mut store = load();
    if store.added.iter().any(|p| norm(p) == n) {
        return Err("That folder is already added.".into());
    }
    store.added.push(cleaned);
    save(&store).map_err(|e| e.to_string())
}

/// Drop a user-added directory (and any custom label for it). The default
/// `~/.claude` is not removable.
pub fn remove_dir(id: &str) -> Result<(), String> {
    let n = norm(id);
    let _guard = lock();
    let mut store = load();
    let before = store.added.len();
    store.added.retain(|p| norm(p) != n);
    if store.added.len() == before {
        return Err("unknown folder".into());
    }
    store.labels.retain(|k, _| norm(k) != n);
    save(&store).map_err(|e| e.to_string())
}

/// Set (or clear, when empty) the display label for an account directory.
pub fn set_label(id: &str, label: &str) -> Result<(), String> {
    let _guard = lock();
    let mut store = load();
    let key = store
        .labels
        .keys()
        .find(|k| norm(k) == norm(id))
        .cloned()
        .unwrap_or_else(|| clean(id));
    let trimmed = label.trim();
    if trimmed.is_empty() {
        store.labels.remove(&key);
    } else {
        store.labels.insert(key, trimmed.to_string());
    }
    save(&store).map_err(|e| e.to_string())
}
