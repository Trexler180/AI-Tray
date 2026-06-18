use crate::models::{Gauge, Usage};
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
enum Provider {
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
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WindowKey {
    provider: Provider,
    window: &'static str,
    /// Per-account discriminator: the Claude account id (its config-dir path),
    /// empty for Codex. Keeps each account's exhaustion/warn state independent.
    account: String,
}

struct WindowSnapshot<'a> {
    key: WindowKey,
    /// Provider-level display name for notifications, e.g. "Claude · Work"
    /// when more than one Claude account is signed in, else just "Claude".
    name: String,
    label: &'static str,
    gauge: &'a Gauge,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct NotificationSettings {
    #[serde(default)]
    pub codex: bool,
    #[serde(default)]
    pub claude: bool,
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

#[derive(Default)]
struct AlertRuntime {
    latest_usage: Option<Usage>,
    windows: HashMap<WindowKey, WindowState>,
    /// Generation of the snapshot last applied. Collections can finish out
    /// of order; an older snapshot must not step the state machine backwards.
    applied_generation: u64,
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
                    let climbed = state.warned_at.map_or(true, |prev| pct > prev);
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
                if !settings.codex && !settings.claude {
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

fn usage_windows(usage: &Usage) -> Vec<WindowSnapshot<'_>> {
    let mut out = Vec::new();
    if usage.codex.live {
        let name = Provider::Codex.title();
        if let Some(gauge) = &usage.codex.primary {
            out.push(WindowSnapshot {
                key: WindowKey {
                    provider: Provider::Codex,
                    window: "session",
                    account: String::new(),
                },
                name: name.to_string(),
                label: "session",
                gauge,
            });
        }
        if let Some(gauge) = &usage.codex.secondary {
            out.push(WindowSnapshot {
                key: WindowKey {
                    provider: Provider::Codex,
                    window: "weekly",
                    account: String::new(),
                },
                name: name.to_string(),
                label: "weekly",
                gauge,
            });
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
        if let Some(gauge) = &acct.five_hour {
            out.push(WindowSnapshot {
                key: WindowKey {
                    provider: Provider::Claude,
                    window: "session",
                    account: acct.id.clone(),
                },
                name: name.clone(),
                label: "session",
                gauge,
            });
        }
        if let Some(gauge) = &acct.seven_day {
            out.push(WindowSnapshot {
                key: WindowKey {
                    provider: Provider::Claude,
                    window: "weekly",
                    account: acct.id.clone(),
                },
                name,
                label: "weekly",
                gauge,
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
    let mut root = dirs::config_dir().or_else(dirs::home_dir)?;
    root.push("AI Usage Tray");
    root.push("settings.json");
    Some(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ClaudeAccountUsage, ClaudeUsage, CodexUsage, Usage};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::Mutex;

    fn state_with(codex: bool, claude: bool) -> AlertState {
        AlertState {
            settings: Mutex::new(NotificationSettings { codex, claude }),
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
}
