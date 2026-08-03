use crate::models::{CodexResets, Gauge, ResetCredit, Usage};
use chrono::Utc;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

/// Warn once per window cycle when usage crosses this percentage.
const WARN_PERCENT: f64 = 90.0;
/// Hysteresis floor: jitter around the warn threshold can't re-arm the
/// warning, but a real window reset (usage falling well below) does.
const WARN_CLEAR_PERCENT: f64 = 80.0;
/// Minimum spacing between alert-driven refreshes. File events arrive every
/// second during an active CLI session; the usage APIs don't need that.
const MIN_REFRESH_SECS: i64 = 30;
/// Periodic poll, so limits exhausted from another device on the same account
/// still notify even without local file activity.
const POLL_INTERVAL_SECS: i64 = 300;
/// Ticker cadence. Wall-clock comparisons, so it survives system suspend.
const TICK: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Provider {
    Codex,
    Claude,
}

impl Provider {
    fn title(self) -> &'static str {
        match self {
            Provider::Codex => "Codex",
            Provider::Claude => "Claude",
        }
    }

    /// Stable identifier for anything persisted or sent to the frontend, where
    /// the display title must be free to change.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Claude => "claude",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct WindowKey {
    pub(crate) provider: Provider,
    pub(crate) window: String,
    /// Per-account discriminator: the Claude account id (its config-dir path),
    /// empty for Codex. Keeps each account's exhaustion/warn state independent.
    pub(crate) account: String,
}

/// One live quota window in a snapshot. Shared with the timeline recorder so
/// that a window kind added in `live.rs` reaches alerts and history together.
pub(crate) struct WindowSnapshot<'a> {
    pub(crate) key: WindowKey,
    /// Provider-level display name for notifications, e.g. "Claude · Work"
    /// when more than one Claude account is signed in, else just "Claude".
    name: String,
    /// Lowercased for notification sentences ("weekly limit almost used up").
    label: String,
    /// The provider's own capitalisation, for anything that shows the window as
    /// a heading rather than mid-sentence.
    pub(crate) display_label: String,
    /// "session" / "weekly" / … — the grouping `live.rs` assigns.
    pub(crate) group: String,
    /// Account label on its own, without the provider prefix `name` carries.
    pub(crate) account_label: String,
    pub(crate) gauge: &'a Gauge,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct NotificationSettings {
    #[serde(default)]
    pub codex: bool,
    #[serde(default)]
    pub claude: bool,
    /// Alerts for Codex reset credits (granted / nearing expiry). Independent of
    /// the `codex` limit-warning toggle.
    #[serde(default)]
    pub codex_resets: bool,
}

struct Exhaustion {
    /// resets_at reported when the window hit 100% (0 when unknown).
    reset_at: i64,
    /// The ticker already forced a confirmation refresh after reset_at
    /// passed; stragglers are left to the periodic poll.
    reset_check_done: bool,
}

#[derive(Default)]
struct WindowState {
    last_used: Option<f64>,
    exhausted: Option<Exhaustion>,
    /// Integer percent of the last advance warning we actually sent, or None
    /// if no warning is currently armed. We only re-warn when usage climbs to
    /// a higher percent, so an unchanged reading — including the reset_at
    /// drift a rolling window emits every poll — stays quiet.
    warned_at: Option<u8>,
}

/// Per-credit notification bookkeeping: whether we've announced it and which
/// expiry stages have already fired.
#[derive(Default)]
struct CreditAlert {
    fired_added: bool,
    fired_stages: HashSet<&'static str>,
}

/// Tracks reset-credit notifications across refreshes. `initialized` flips true
/// after the first successful observation so credits already present at launch
/// (and expiry stages already passed) are recorded as a silent baseline rather
/// than replayed as fresh news.
#[derive(Default)]
struct ResetAlertState {
    initialized: bool,
    known: HashMap<String, CreditAlert>,
}

#[derive(Default)]
struct AlertRuntime {
    latest_usage: Option<Usage>,
    windows: HashMap<WindowKey, WindowState>,
    /// Generation of the snapshot last applied. Collections can finish out
    /// of order; an older snapshot must not step the state machine backwards.
    applied_generation: u64,
    resets: ResetAlertState,
}

pub struct AlertState {
    settings: Mutex<NotificationSettings>,
    runtime: Mutex<AlertRuntime>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    watched: Mutex<HashSet<PathBuf>>,
    generation: AtomicU64,
    refresh_inflight: AtomicBool,
    last_refresh: Mutex<i64>,
}

impl AlertState {
    pub fn load() -> Self {
        Self {
            settings: Mutex::new(load_settings()),
            runtime: Mutex::new(AlertRuntime::default()),
            watcher: Mutex::new(None),
            watched: Mutex::new(HashSet::new()),
            generation: AtomicU64::new(0),
            refresh_inflight: AtomicBool::new(false),
            last_refresh: Mutex::new(0),
        }
    }

    pub fn settings(&self) -> NotificationSettings {
        self.settings.lock().unwrap().clone()
    }

    pub fn set_provider(
        &self,
        provider: &str,
        enabled: bool,
    ) -> Result<NotificationSettings, String> {
        let mut settings = self.settings.lock().unwrap();
        match provider {
            "codex" => settings.codex = enabled,
            "claude" => settings.claude = enabled,
            "codex_resets" => settings.codex_resets = enabled,
            _ => return Err(format!("unknown provider: {provider}")),
        }
        persist_settings(&settings)?;
        Ok(settings.clone())
    }

    pub fn latest_usage(&self) -> Option<Usage> {
        self.runtime.lock().unwrap().latest_usage.clone()
    }

    /// Reserve an ordering slot for a collection that is about to start.
    pub fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn observe_usage(&self, app: &AppHandle, usage: &Usage, generation: u64) {
        // Independent of the notification settings: the timeline needs the
        // record whether or not this user wants to be told about limits.
        crate::windows_history::record(usage);
        for (title, body) in self.notification_events(usage, generation) {
            send_notification(app, &title, &body);
        }
    }

    fn notification_events(&self, usage: &Usage, generation: u64) -> Vec<(String, String)> {
        let settings = self.settings();
        let snapshots = usage_windows(usage);
        let mut to_notify: Vec<(String, String)> = Vec::new();

        {
            let mut runtime = self.runtime.lock().unwrap();
            if generation < runtime.applied_generation {
                return Vec::new();
            }
            runtime.applied_generation = generation;
            runtime.latest_usage = Some(usage.clone());

            for snapshot in snapshots {
                let provider = snapshot.key.provider;
                let label = snapshot.label;
                let name = snapshot.name;
                let current = snapshot.gauge.used_percent;
                let reset_at = snapshot.gauge.resets_at.unwrap_or(0);
                let state = runtime.windows.entry(snapshot.key).or_default();
                let previous = state.last_used.replace(current);

                if !provider_enabled(&settings, provider) {
                    // Keep tracking raw usage so re-enabling starts from the
                    // present instead of replaying stale transitions.
                    state.exhausted = None;
                    state.warned_at = None;
                    continue;
                }

                // Reset detection on evidence: usage fell well below the cap.
                // Some rolling-window APIs drift reset_at and briefly wobble
                // under 100% while still effectively exhausted; neither should
                // re-arm the "used up" notification.
                let mut clear_exhaustion = false;
                if let Some(exhaustion) = &mut state.exhausted {
                    let reset_at_changed = exhaustion.reset_at != 0
                        && reset_at != 0
                        && reset_at != exhaustion.reset_at;
                    clear_exhaustion = current < WARN_CLEAR_PERCENT;

                    if !clear_exhaustion {
                        if reset_at_changed {
                            exhaustion.reset_at = reset_at;
                            exhaustion.reset_check_done = false;
                        }
                        continue;
                    }
                }

                if clear_exhaustion {
                    state.exhausted = None;
                    state.warned_at = None;
                    to_notify.push((
                        format!("{name} limit reset"),
                        format!("{name} {label} window has reset."),
                    ));
                    continue;
                }

                // On the very first observation after launch we only record
                // state: a limit that was already exhausted before the app
                // started isn't news.
                let first = previous.is_none();
                if current >= 100.0 {
                    if state.exhausted.is_none() {
                        state.exhausted = Some(Exhaustion {
                            reset_at,
                            reset_check_done: false,
                        });
                        if !first {
                            to_notify.push((
                                format!("{name} limit used up"),
                                format!("{name} {label} window is at 100% used."),
                            ));
                        }
                    }
                } else if current >= WARN_PERCENT {
                    // Notify on entering the warn band and again only as usage
                    // climbs to a higher whole percent. An unchanged reading —
                    // or the reset_at drift a rolling window emits each poll —
                    // leaves warned_at untouched and stays silent.
                    let pct = current.round() as u8;
                    let climbed = state.warned_at.is_none_or(|prev| pct > prev);
                    if climbed {
                        state.warned_at = Some(pct);
                        if !first {
                            to_notify.push((
                                format!("{name} limit almost used"),
                                format!("{name} {label} window is at {pct}% used."),
                            ));
                        }
                    }
                } else if current < WARN_CLEAR_PERCENT {
                    state.warned_at = None;
                }
            }

            to_notify.extend(reset_events(
                &mut runtime.resets,
                settings.codex_resets,
                usage.codex.resets.as_ref(),
                Utc::now().timestamp(),
            ));
        }

        to_notify
    }

    /// Watch any usage paths that exist now but weren't watched yet (they may
    /// be created after startup, e.g. on a CLI's first run).
    fn watch_missing_paths(&self) {
        let mut watcher_guard = self.watcher.lock().unwrap();
        let Some(watcher) = watcher_guard.as_mut() else {
            return;
        };
        let mut watched = self.watched.lock().unwrap();
        for path in usage_watch_paths() {
            if watched.contains(&path) || !path.exists() {
                continue;
            }
            let mode = if path.is_dir() {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            if watcher.watch(&path, mode).is_ok() {
                watched.insert(path);
            }
        }
    }
}

pub fn start_usage_watcher(app: &AppHandle) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let watcher = RecommendedWatcher::new(
        move |result| {
            let _ = tx.send(result);
        },
        Config::default(),
    )
    .map_err(|e| e.to_string())?;

    let state = app.state::<AlertState>();
    *state.watcher.lock().unwrap() = Some(watcher);
    state.watch_missing_paths();

    let handle = app.clone();
    std::thread::Builder::new()
        .name("usage-alert-watcher".to_string())
        .spawn(move || {
            while let Ok(result) = rx.recv() {
                if result.is_err() {
                    continue;
                }
                // Coalesce the burst of events a single CLI write produces.
                std::thread::sleep(Duration::from_millis(800));
                while rx.try_recv().is_ok() {}
                refresh_for_alerts(handle.clone());
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// One long-lived thread that drives everything time-based off the wall
/// clock, so a laptop suspend can't strand a pending notification:
/// - confirms a reset shortly after an exhausted window's reset time passes,
/// - polls on a slow interval so usage from other devices is noticed,
/// - re-arms file watches for paths created after startup.
pub fn start_alert_ticker(app: AppHandle) {
    let _ = std::thread::Builder::new()
        .name("usage-alert-ticker".to_string())
        .spawn(move || {
            let mut last_poll = Utc::now().timestamp();
            loop {
                std::thread::sleep(TICK);
                let state = app.state::<AlertState>();
                state.watch_missing_paths();

                let settings = state.settings();
                if !settings.codex && !settings.claude && !settings.codex_resets {
                    continue;
                }

                let now = Utc::now().timestamp();
                let mut due = now - last_poll >= POLL_INTERVAL_SECS;
                {
                    let mut runtime = state.runtime.lock().unwrap();
                    for window in runtime.windows.values_mut() {
                        if let Some(exhaustion) = &mut window.exhausted {
                            if exhaustion.reset_at != 0
                                && exhaustion.reset_at <= now
                                && !exhaustion.reset_check_done
                            {
                                exhaustion.reset_check_done = true;
                                due = true;
                            }
                        }
                    }
                }
                if due {
                    last_poll = now;
                    refresh_for_alerts(app.clone());
                }
            }
        });
}

pub fn refresh_for_alerts(app: AppHandle) {
    let generation = {
        let state = app.state::<AlertState>();
        let now = Utc::now().timestamp();
        {
            let mut last = state.last_refresh.lock().unwrap();
            if now - *last < MIN_REFRESH_SECS {
                return;
            }
            if state.refresh_inflight.swap(true, Ordering::SeqCst) {
                return;
            }
            *last = now;
        }
        state.next_generation()
    };

    tauri::async_runtime::spawn(async move {
        let result = tauri::async_runtime::spawn_blocking(crate::collect_usage_sync).await;
        let state = app.state::<AlertState>();
        state.refresh_inflight.store(false, Ordering::SeqCst);
        let Ok(usage) = result else { return };
        state.observe_usage(&app, &usage, generation);
        if let Some(window) = app.get_webview_window("main") {
            if window.is_visible().unwrap_or(false) {
                let _ = window.emit("refresh", ());
            }
        }
    });
}

fn send_notification(app: &AppHandle, title: &str, body: &str) {
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        eprintln!("notification \"{title}\" failed: {e}");
    }
}

pub(crate) fn usage_windows(usage: &Usage) -> Vec<WindowSnapshot<'_>> {
    let mut out = Vec::new();
    if usage.codex.live {
        let name = Provider::Codex.title();
        if !usage.codex.quotas.is_empty() {
            for quota in &usage.codex.quotas {
                out.push(WindowSnapshot {
                    key: WindowKey {
                        provider: Provider::Codex,
                        window: quota.id.clone(),
                        account: String::new(),
                    },
                    name: name.to_string(),
                    label: quota.label.to_ascii_lowercase(),
                    display_label: quota.label.clone(),
                    group: quota.group.clone(),
                    account_label: name.to_string(),
                    gauge: &quota.gauge,
                });
            }
        } else if let Some(gauge) = &usage.codex.primary {
            out.push(WindowSnapshot {
                key: WindowKey {
                    provider: Provider::Codex,
                    window: "session".to_string(),
                    account: String::new(),
                },
                name: name.to_string(),
                label: "session".to_string(),
                display_label: "Session".to_string(),
                group: "session".to_string(),
                account_label: name.to_string(),
                gauge,
            });
        }
        if usage.codex.quotas.is_empty() {
            if let Some(gauge) = &usage.codex.secondary {
            out.push(WindowSnapshot {
                key: WindowKey {
                    provider: Provider::Codex,
                    window: "weekly".to_string(),
                    account: String::new(),
                },
                name: name.to_string(),
                label: "weekly".to_string(),
                display_label: "Weekly".to_string(),
                group: "weekly".to_string(),
                account_label: name.to_string(),
                gauge,
            });
            }
        }
    }

    // Each Claude account alerts independently. When more than one is signed
    // in, the notification name carries the account label so the user can tell
    // them apart; with a single account it stays the plain provider name.
    let multi = usage.claude.accounts.len() > 1;
    for acct in usage.claude.accounts.iter().filter(|a| a.live) {
        let name = if multi {
            format!("{} · {}", Provider::Claude.title(), acct.label)
        } else {
            Provider::Claude.title().to_string()
        };
        if !acct.quotas.is_empty() {
            for quota in &acct.quotas {
                out.push(WindowSnapshot {
                    key: WindowKey {
                        provider: Provider::Claude,
                        window: quota.id.clone(),
                        account: acct.id.clone(),
                    },
                    name: name.clone(),
                    label: quota.label.to_ascii_lowercase(),
                    display_label: quota.label.clone(),
                    group: quota.group.clone(),
                    account_label: acct.label.clone(),
                    gauge: &quota.gauge,
                });
            }
            continue;
        }
        if let Some(gauge) = &acct.five_hour {
            out.push(WindowSnapshot {
                key: WindowKey {
                    provider: Provider::Claude,
                    window: "session".to_string(),
                    account: acct.id.clone(),
                },
                name: name.clone(),
                label: "session".to_string(),
                display_label: "Session".to_string(),
                group: "session".to_string(),
                account_label: acct.label.clone(),
                gauge,
            });
        }
        if let Some(gauge) = &acct.seven_day {
            out.push(WindowSnapshot {
                key: WindowKey {
                    provider: Provider::Claude,
                    window: "weekly".to_string(),
                    account: acct.id.clone(),
                },
                name: name.clone(),
                label: "weekly".to_string(),
                display_label: "Weekly".to_string(),
                group: "weekly".to_string(),
                account_label: acct.label.clone(),
                gauge,
            });
        }
        // Model-scoped weekly window (e.g. the Fable-only limit) — tracked
        // independently since it usually runs ahead of the all-models weekly.
        if let Some(mg) = &acct.seven_day_model {
            out.push(WindowSnapshot {
                key: WindowKey {
                    provider: Provider::Claude,
                    window: "weekly_model".to_string(),
                    account: acct.id.clone(),
                },
                name,
                label: format!("{} weekly", mg.model),
                display_label: format!("Weekly ({})", mg.model),
                group: "weekly".to_string(),
                account_label: acct.label.clone(),
                gauge: &mg.gauge,
            });
        }
    }
    out
}

fn provider_enabled(settings: &NotificationSettings, provider: Provider) -> bool {
    match provider {
        Provider::Codex => settings.codex,
        Provider::Claude => settings.claude,
    }
}

/// Expiry-warning stages, fired once each as a credit's deadline approaches.
/// They escalate: a day out, on the calendar day it expires, then a few hours
/// before. Expressed as stable keys stored in `CreditAlert::fired_stages`.
const EXPIRY_DAY_SECS: i64 = 24 * 3600;
const EXPIRY_HOURS_SECS: i64 = 3 * 3600;

/// Available, not-yet-expired credits — the only ones worth tracking.
fn active_credits(resets: &CodexResets, now: i64) -> Vec<&ResetCredit> {
    resets
        .credits
        .iter()
        .filter(|c| c.status == "available")
        .filter(|c| c.expires_at.map(|e| e > now).unwrap_or(true))
        .collect()
}

fn same_local_day(a: i64, b: i64) -> bool {
    use chrono::{Local, TimeZone};
    match (
        Local.timestamp_opt(a, 0).single(),
        Local.timestamp_opt(b, 0).single(),
    ) {
        (Some(x), Some(y)) => x.date_naive() == y.date_naive(),
        _ => false,
    }
}

/// Which expiry stages a credit meets right now (cumulative — once "hours" is
/// met, "day" is too). Empty once past expiry.
fn stages_met(now: i64, expires_at: i64) -> Vec<&'static str> {
    let remaining = expires_at - now;
    if remaining <= 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    if remaining <= EXPIRY_DAY_SECS {
        out.push("day");
    }
    if same_local_day(now, expires_at) {
        out.push("today");
    }
    if remaining <= EXPIRY_HOURS_SECS {
        out.push("hours");
    }
    out
}

fn added_body(c: &ResetCredit) -> String {
    match &c.expires_in {
        Some(t) => format!("A free Codex rate-limit reset is available — expires in {t}."),
        None => "A free Codex rate-limit reset is available.".to_string(),
    }
}

fn stage_body(stage: &str) -> String {
    match stage {
        "day" => "A Codex reset credit expires in about a day.",
        "today" => "A Codex reset credit expires today.",
        "hours" => "A Codex reset credit expires in a few hours — use it before it's gone.",
        _ => "A Codex reset credit is expiring.",
    }
    .to_string()
}

/// Notifications for reset credits: a new credit appearing, and each expiry
/// stage as the deadline nears. Only acts on a successful fetch (`available`),
/// so a transient network blip never reads as "all credits gone".
///
/// The first successful observation (and the whole run while the toggle is off)
/// records state silently, mirroring the gauge alerts: credits already present
/// at launch, or stages already passed, aren't news.
fn reset_events(
    state: &mut ResetAlertState,
    enabled: bool,
    resets: Option<&CodexResets>,
    now: i64,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(resets) = resets.filter(|r| r.available) else {
        return out;
    };

    let active = active_credits(resets, now);
    let active_ids: HashSet<String> = active.iter().map(|c| c.id.clone()).collect();

    let first = !state.initialized;
    state.initialized = true;
    // Emit only once past the silent baseline and while the toggle is on. When
    // off we still advance the bookkeeping so enabling later starts from the
    // present rather than replaying history.
    let emit = enabled && !first;

    for c in &active {
        let entry = state.known.entry(c.id.clone()).or_default();
        if !entry.fired_added {
            entry.fired_added = true;
            if emit {
                out.push(("Codex reset available".to_string(), added_body(c)));
            }
        }
        if let Some(exp) = c.expires_at {
            for stage in stages_met(now, exp) {
                if entry.fired_stages.insert(stage) && emit {
                    out.push(("Codex reset expiring".to_string(), stage_body(stage)));
                }
            }
        }
    }

    // Drop credits that are gone (redeemed or expired) so a later re-grant with
    // a fresh id is announced again. Safe here: only reached on a good fetch.
    state.known.retain(|id, _| active_ids.contains(id));

    out
}

fn usage_watch_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        let mut codex_sessions = home.clone();
        codex_sessions.push(".codex");
        codex_sessions.push("sessions");
        paths.push(codex_sessions);

        let mut codex_auth = home;
        codex_auth.push(".codex");
        codex_auth.push("auth.json");
        paths.push(codex_auth);
    }

    // Every Claude config directory we track: its credentials file (a re-login
    // / token rotation) and its transcripts (active usage).
    for dir in crate::accounts::account_dirs() {
        paths.push(dir.join("projects"));
        paths.push(crate::accounts::creds_file(&dir));
    }
    paths
}

fn load_settings() -> NotificationSettings {
    let Some(path) = settings_path() else {
        return NotificationSettings::default();
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

fn persist_settings(settings: &NotificationSettings) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "settings directory unavailable".to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp-aiusage");
    let body = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    fs::write(&tmp, body).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn settings_path() -> Option<PathBuf> {
    let mut root = crate::util::config_dir()?;
    root.push("settings.json");
    Some(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ClaudeAccountUsage, ClaudeUsage, CodexUsage, QuotaWindow, Usage};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::Mutex;

    fn state_with(codex: bool, claude: bool) -> AlertState {
        AlertState {
            settings: Mutex::new(NotificationSettings {
                codex,
                claude,
                codex_resets: false,
            }),
            runtime: Mutex::new(AlertRuntime::default()),
            watcher: Mutex::new(None),
            watched: Mutex::new(HashSet::new()),
            generation: AtomicU64::new(0),
            refresh_inflight: AtomicBool::new(false),
            last_refresh: Mutex::new(0),
        }
    }

    fn test_state() -> AlertState {
        state_with(true, false)
    }

    /// Two live Claude accounts, each with only a 5h window at the given usage.
    fn claude_two_account_usage(work_pct: f64, personal_pct: f64) -> Usage {
        let account = |id: &str, label: &str, pct: f64| ClaudeAccountUsage {
            id: id.to_string(),
            label: label.to_string(),
            subscription_type: Some("pro".to_string()),
            active: id == "dirA",
            removable: id != "dirA",
            live: true,
            five_hour: Some(Gauge {
                used_percent: pct,
                window_minutes: 300,
                resets_at: Some(1_000),
                resets_in: None,
            }),
            seven_day: None,
            seven_day_model: None,
            quotas: Vec::new(),
            extra_usage: None,
            health: Default::default(),
        };
        Usage {
            claude: ClaudeUsage {
                available: true,
                live: true,
                accounts: vec![
                    account("dirA", "Work", work_pct),
                    account("dirB", "Personal", personal_pct),
                ],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn codex_session_usage(used_percent: f64, reset_at: i64) -> Usage {
        Usage {
            codex: CodexUsage {
                available: true,
                live: true,
                primary: Some(Gauge {
                    used_percent,
                    window_minutes: 300,
                    resets_at: Some(reset_at),
                    resets_in: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn codex_weekly_usage(used_percent: f64, reset_at: i64) -> Usage {
        Usage {
            codex: CodexUsage {
                available: true,
                live: true,
                secondary: Some(Gauge {
                    used_percent,
                    window_minutes: 10_080,
                    resets_at: Some(reset_at),
                    resets_in: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn warn_fires_once_then_stays_quiet_while_reset_at_drifts() {
        let state = test_state();

        // Establish a baseline below the warn band: first observation only
        // records, so the later crossing reads as news rather than startup.
        assert!(state
            .notification_events(&codex_weekly_usage(50.0, 1_000), 1)
            .is_empty());

        // Crossing into the warn band notifies exactly once.
        let events = state.notification_events(&codex_weekly_usage(91.0, 1_010), 2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "Codex limit almost used");
        assert!(events[0].1.contains("91%"));

        // The rolling weekly window keeps drifting resets_at every poll while
        // usage holds at 91%. None of these may re-notify (the reported bug).
        for (i, reset_at) in [1_040, 1_070, 1_100, 1_130].into_iter().enumerate() {
            assert!(
                state
                    .notification_events(&codex_weekly_usage(91.0, reset_at), 3 + i as u64)
                    .is_empty(),
                "drift poll {i} re-notified"
            );
        }

        // A genuine climb to a higher percent is still surfaced.
        let events = state.notification_events(&codex_weekly_usage(93.0, 1_160), 100);
        assert_eq!(events.len(), 1);
        assert!(events[0].1.contains("93%"));
    }

    #[test]
    fn used_up_notification_stays_quiet_until_real_reset() {
        let state = test_state();

        assert!(state
            .notification_events(&codex_session_usage(95.0, 1_000), 1)
            .is_empty());

        let events = state.notification_events(&codex_session_usage(100.0, 1_000), 2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "Codex limit used up");

        assert!(state
            .notification_events(&codex_session_usage(100.0, 1_030), 3)
            .is_empty());
        assert!(state
            .notification_events(&codex_session_usage(99.5, 1_060), 4)
            .is_empty());
        assert!(state
            .notification_events(&codex_session_usage(100.0, 1_090), 5)
            .is_empty());

        let events = state.notification_events(&codex_session_usage(50.0, 2_000), 6);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "Codex limit reset");

        let events = state.notification_events(&codex_session_usage(100.0, 2_000), 7);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "Codex limit used up");
    }

    #[test]
    fn claude_accounts_notify_independently_and_name_the_account() {
        let state = state_with(false, true);

        // Baseline below the warn band for both accounts: records only.
        assert!(state
            .notification_events(&claude_two_account_usage(50.0, 50.0), 1)
            .is_empty());

        // "Work" crosses into the warn band; "Personal" stays low. Exactly one
        // notification fires, and it names the account it's about.
        let events = state.notification_events(&claude_two_account_usage(91.0, 50.0), 2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "Claude · Work limit almost used");
        assert!(events[0].1.contains("Work"));
        assert!(events[0].1.contains("91%"));

        // Now "Personal" crosses too. "Work" is unchanged (already warned at
        // 91%), so only "Personal" notifies — proving the two accounts track
        // their warn state independently.
        let events = state.notification_events(&claude_two_account_usage(91.0, 95.0), 3);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "Claude · Personal limit almost used");
        assert!(events[0].1.contains("Personal"));
        assert!(events[0].1.contains("95%"));
    }

    #[test]
    fn dynamic_quota_ids_are_distinct_alert_keys() {
        let quota = |id: &str| QuotaWindow {
            id: id.to_string(),
            label: id.to_string(),
            gauge: Gauge {
                used_percent: 50.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let usage = Usage {
            claude: ClaudeUsage {
                accounts: vec![ClaudeAccountUsage {
                    id: "account".to_string(),
                    label: "Account".to_string(),
                    live: true,
                    quotas: vec![quota("weekly:fable"), quota("weekly:cowork")],
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let windows = usage_windows(&usage);
        let keys: HashSet<_> = windows.iter().map(|window| &window.key).collect();
        assert_eq!(windows.len(), 2);
        assert_eq!(keys.len(), 2);
    }

    // ---------------- reset credits ----------------

    fn credit(id: &str, expires_at: i64) -> ResetCredit {
        ResetCredit {
            id: id.to_string(),
            status: "available".to_string(),
            expires_at: Some(expires_at),
            ..Default::default()
        }
    }

    fn resets(credits: Vec<ResetCredit>) -> CodexResets {
        let available_count = credits.iter().filter(|c| c.status == "available").count() as u64;
        CodexResets {
            available: true,
            available_count,
            credits,
        }
    }

    #[test]
    fn reset_baseline_is_silent_then_new_credit_notifies() {
        let mut st = ResetAlertState::default();
        let now = 1_000_000;
        let far = now + 30 * 86400;

        // First observation records existing credits as a silent baseline.
        let r = resets(vec![credit("A", far)]);
        assert!(reset_events(&mut st, true, Some(&r), now).is_empty());

        // Same set again: nothing new.
        assert!(reset_events(&mut st, true, Some(&r), now).is_empty());

        // A genuinely new credit appears → announced once.
        let r = resets(vec![credit("A", far), credit("B", far)]);
        let ev = reset_events(&mut st, true, Some(&r), now);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, "Codex reset available");

        // And not again on the next poll.
        assert!(reset_events(&mut st, true, Some(&r), now).is_empty());
    }

    #[test]
    fn stage_helpers_are_threshold_based() {
        let exp = 1_700_000_000;
        // Outside a day (25h apart is always a different calendar day too).
        assert!(stages_met(exp - 25 * 3600, exp).is_empty());
        // Inside a day → at least the "day" stage.
        assert!(stages_met(exp - 10 * 3600, exp).contains(&"day"));
        // Inside a few hours → "hours" as well.
        let near = stages_met(exp - 3600, exp);
        assert!(near.contains(&"day"));
        assert!(near.contains(&"hours"));
        // Past expiry → empty.
        assert!(stages_met(exp + 10, exp).is_empty());
        // Calendar comparison: identical instant is the same day; 3 days apart
        // is always a different one regardless of timezone.
        assert!(same_local_day(exp, exp));
        assert!(!same_local_day(exp, exp + 3 * 86400));
    }

    #[test]
    fn reset_expiry_stages_fire_once_as_deadline_nears() {
        let mut st = ResetAlertState::default();
        let expires = 1_700_000_000;
        let r = resets(vec![credit("A", expires)]);

        // Baseline far from expiry: silent.
        assert!(reset_events(&mut st, true, Some(&r), expires - 5 * 86400).is_empty());

        // ~10h out → the "day" stage fires.
        let ev = reset_events(&mut st, true, Some(&r), expires - 10 * 3600);
        assert!(ev
            .iter()
            .any(|(t, b)| t == "Codex reset expiring" && b.contains("about a day")));

        // Same point again → the "day" stage does not re-fire.
        let ev = reset_events(&mut st, true, Some(&r), expires - 10 * 3600);
        assert!(ev.iter().all(|(_, b)| !b.contains("about a day")));

        // ~1h out → the "few hours" stage fires.
        let ev = reset_events(&mut st, true, Some(&r), expires - 3600);
        assert!(ev.iter().any(|(_, b)| b.contains("few hours")));

        // Past expiry → the credit is pruned and nothing fires.
        assert!(reset_events(&mut st, true, Some(&r), expires + 60).is_empty());
    }

    #[test]
    fn reset_disabled_records_baseline_without_emitting() {
        let mut st = ResetAlertState::default();
        let now = 1_000_000;
        let far = now + 30 * 86400;

        // Toggle off: a new credit must not notify, but state advances so that
        // enabling later doesn't replay it.
        let r = resets(vec![credit("A", far)]);
        assert!(reset_events(&mut st, false, Some(&r), now).is_empty());

        // Now enabled, same credit → still silent (already baselined).
        assert!(reset_events(&mut st, true, Some(&r), now).is_empty());
    }

    #[test]
    fn reset_failed_fetch_does_not_prune_or_notify() {
        let mut st = ResetAlertState::default();
        let now = 1_000_000;
        let far = now + 30 * 86400;

        let r = resets(vec![credit("A", far)]);
        assert!(reset_events(&mut st, true, Some(&r), now).is_empty()); // baseline

        // A fetch failure surfaces as None (or available=false): no pruning.
        assert!(reset_events(&mut st, true, None, now).is_empty());
        let unavailable = CodexResets::default(); // available = false
        assert!(reset_events(&mut st, true, Some(&unavailable), now).is_empty());

        // The credit is still known, so it doesn't re-announce when it returns.
        assert!(reset_events(&mut st, true, Some(&r), now).is_empty());
    }
}
