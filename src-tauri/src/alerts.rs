use crate::models::{ClaudeAccountUsage, CodexResets, Gauge, QuotaWindow, ResetCredit, Usage};
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

/// Advance warnings, each sent at most once per window cycle as usage climbs
/// past it, unless the user picks their own. A few steps rather than one per
/// percent: warning at every new whole percent from 90 could send ten
/// notifications ahead of the "used up" one.
const DEFAULT_WARN_LEVELS: [u8; 2] = [90, 95];
/// Hysteresis floor: jitter around the warn threshold can't re-arm the
/// warning, but a real window reset (usage falling well below) does. Also the
/// level under which an exhausted window counts as reset.
const WARN_CLEAR_PERCENT: f64 = 80.0;
/// How far usage has to fall between two readings to count as the window
/// rolling over, which re-arms the "will run out early" warning.
const ROLLOVER_DROP: f64 = 5.0;
/// Minimum spacing between alert-driven refreshes. File events arrive every
/// second during an active CLI session; the usage APIs don't need that.
const MIN_REFRESH_SECS: i64 = 30;
/// Periodic poll, so usage from another device or the web shows up — in
/// notifications, the taskbar widget and the window timeline — even without
/// local file activity. Runs regardless of the notification settings: the
/// widget and the timeline only ever see what a collection brings back.
const POLL_INTERVAL_SECS: i64 = 300;
/// The same poll while the taskbar widget is on. It is on screen permanently
/// and re-reads the cached snapshot every minute, so five-minute-old numbers
/// are visible in a way they aren't behind a closed panel.
const WIDGET_POLL_INTERVAL_SECS: i64 = 120;
/// Ticker cadence. Wall-clock comparisons, so it survives system suspend.
const TICK: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Provider {
    Codex,
    Claude,
}

impl Provider {
    pub(crate) fn title(self) -> &'static str {
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

#[derive(Clone, Serialize, Deserialize)]
pub struct NotificationSettings {
    #[serde(default)]
    pub codex: bool,
    #[serde(default)]
    pub claude: bool,
    /// Alerts for Codex reset credits (granted / nearing expiry). Independent of
    /// the `codex` limit-warning toggle.
    #[serde(default)]
    pub codex_resets: bool,
    /// The same for Claude's banked resets.
    #[serde(default)]
    pub claude_resets: bool,
    /// Usage levels, in percent, that each send one advance warning per window.
    #[serde(default = "default_warn_levels")]
    pub warn_levels: Vec<u8>,
    /// Say when a window is used up, and again when it resets.
    #[serde(default = "enabled_by_default")]
    pub used_up: bool,
    /// Warn once when a window's pace says it will run out before it resets.
    #[serde(default = "enabled_by_default")]
    pub forecast: bool,
    /// Hold every notification between `quiet_start` and `quiet_end`.
    #[serde(default)]
    pub quiet_hours: bool,
    /// Minutes after local midnight.
    #[serde(default = "default_quiet_start")]
    pub quiet_start: u16,
    #[serde(default = "default_quiet_end")]
    pub quiet_end: u16,
}

fn default_warn_levels() -> Vec<u8> {
    DEFAULT_WARN_LEVELS.to_vec()
}
fn enabled_by_default() -> bool {
    true
}
fn default_quiet_start() -> u16 {
    22 * 60
}
fn default_quiet_end() -> u16 {
    8 * 60
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            codex: false,
            claude: false,
            codex_resets: false,
            claude_resets: false,
            warn_levels: default_warn_levels(),
            used_up: true,
            forecast: true,
            quiet_hours: false,
            quiet_start: default_quiet_start(),
            quiet_end: default_quiet_end(),
        }
    }
}

impl NotificationSettings {
    /// The warn levels as thresholds, ascending, without duplicates or values
    /// that could never fire before "used up".
    fn warn_steps(&self) -> Vec<f64> {
        let mut levels: Vec<u8> = self.warn_levels.iter().copied().filter(|l| (1..100).contains(l)).collect();
        levels.sort_unstable();
        levels.dedup();
        levels.into_iter().map(f64::from).collect()
    }

    /// Whether `minute` (after local midnight) falls in the quiet window. The
    /// window may run past midnight, as the default 22:00–08:00 does.
    fn quiet_at(&self, minute: u16) -> bool {
        if !self.quiet_hours || self.quiet_start == self.quiet_end {
            return false;
        }
        if self.quiet_start < self.quiet_end {
            (self.quiet_start..self.quiet_end).contains(&minute)
        } else {
            minute >= self.quiet_start || minute < self.quiet_end
        }
    }
}

/// The part of the settings the Alerts section edits as one.
#[derive(Deserialize)]
pub struct AlertOptions {
    pub warn_levels: Vec<u8>,
    pub used_up: bool,
    pub forecast: bool,
    pub quiet_hours: bool,
    pub quiet_start: u16,
    pub quiet_end: u16,
}

/// "4:52 pm" in local time, matching the panel's clock times.
pub(crate) fn clock_text(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%-I:%M %P").to_string())
        .unwrap_or_default()
}

/// "18 min", "1 h 20 min" — a gap for the middle of a sentence.
pub(crate) fn gap_text(secs: i64) -> String {
    let minutes = (secs.max(0) + 30) / 60;
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
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
    /// The warn level the last advance warning was for, or None if no
    /// warning is currently armed. We only re-warn when usage reaches a higher
    /// step, so an unchanged reading — including the reset_at drift a rolling
    /// window emits every poll — stays quiet.
    warned_at: Option<u8>,
    /// The "will run out early" warning went out for this window cycle.
    forecast_warned: bool,
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

/// What was last seen of one Claude account's banked resets.
#[derive(Default)]
struct ClaudeResetAlert {
    count: u64,
    /// The grant the expiry stages belong to; a new next grant starts them over.
    grant: Option<String>,
    fired_stages: HashSet<&'static str>,
}

/// Claude reset bookkeeping, per account. An account seen for the first time
/// (at launch, or newly added) is a silent baseline, like the Codex credits.
#[derive(Default)]
struct ClaudeResetAlertState {
    known: HashMap<String, ClaudeResetAlert>,
}

#[derive(Default)]
struct AlertRuntime {
    latest_usage: Option<Usage>,
    windows: HashMap<WindowKey, WindowState>,
    /// Generation of the snapshot last applied. Collections can finish out
    /// of order; an older snapshot must not step the state machine backwards.
    applied_generation: u64,
    resets: ResetAlertState,
    claude_resets: ClaudeResetAlertState,
}

pub struct AlertState {
    settings: Mutex<NotificationSettings>,
    runtime: Mutex<AlertRuntime>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    watched: Mutex<HashSet<PathBuf>>,
    generation: AtomicU64,
    refresh_inflight: AtomicBool,
    last_refresh: Mutex<i64>,
    /// A refresh was asked for while throttled or already running. The ticker
    /// runs it once the throttle allows, so the last burst of file activity
    /// before a CLI goes quiet is still picked up.
    refresh_pending: AtomicBool,
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
            refresh_pending: AtomicBool::new(false),
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
            "claude_resets" => settings.claude_resets = enabled,
            _ => return Err(format!("unknown provider: {provider}")),
        }
        persist_settings(&settings)?;
        Ok(settings.clone())
    }

    pub fn set_options(&self, options: AlertOptions) -> Result<NotificationSettings, String> {
        let mut settings = self.settings.lock().unwrap();
        settings.warn_levels = options.warn_levels;
        settings.used_up = options.used_up;
        settings.forecast = options.forecast;
        settings.quiet_hours = options.quiet_hours;
        settings.quiet_start = options.quiet_start.min(24 * 60 - 1);
        settings.quiet_end = options.quiet_end.min(24 * 60 - 1);
        persist_settings(&settings)?;
        Ok(settings.clone())
    }

    pub fn latest_usage(&self) -> Option<Usage> {
        self.runtime.lock().unwrap().latest_usage.clone()
    }

    /// Record that a collection outside this module (the panel's own refresh)
    /// just ran, so the background poll and any deferred refresh don't repeat
    /// it straight away.
    pub fn note_collection(&self) {
        *self.last_refresh.lock().unwrap() = Utc::now().timestamp();
        self.refresh_pending.store(false, Ordering::SeqCst);
    }

    /// Reserve an ordering slot for a collection that is about to start.
    pub fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn observe_usage(&self, app: &AppHandle, usage: &Usage, generation: u64) {
        // Independent of the notification settings: the timeline needs the
        // record whether or not this user wants to be told about limits.
        crate::windows_history::record(usage);
        let events = self.notification_events(usage, generation);
        // Quiet hours drop the toasts, not the bookkeeping: a limit crossed at
        // 3 am isn't replayed as news at 8.
        let minute = {
            use chrono::Timelike;
            let now = chrono::Local::now();
            (now.hour() * 60 + now.minute()) as u16
        };
        if !self.settings().quiet_at(minute) {
            for (title, body) in events {
                send_notification(app, &title, &body);
            }
        }
        // The tray reads whichever snapshot is newest, which an out-of-order
        // collection may not be.
        if let Some(latest) = self.latest_usage() {
            crate::tray::update(app, &latest);
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

            let steps = settings.warn_steps();
            // Warnings re-arm once usage falls well under the lowest step.
            let warn_clear = steps
                .first()
                .map(|lowest| (lowest - 10.0).min(WARN_CLEAR_PERCENT));
            let now = Utc::now().timestamp();

            for snapshot in snapshots {
                let provider = snapshot.key.provider;
                let account = snapshot.key.account.clone();
                let label = snapshot.label;
                let name = snapshot.name;
                let group = snapshot.group;
                let gauge = snapshot.gauge;
                let current = gauge.used_percent;
                let reset_at = gauge.resets_at.unwrap_or(0);
                let state = runtime.windows.entry(snapshot.key).or_default();
                let previous = state.last_used.replace(current);

                if !provider_enabled(&settings, provider) {
                    // Keep tracking raw usage so re-enabling starts from the
                    // present instead of replaying stale transitions.
                    state.exhausted = None;
                    state.warned_at = None;
                    state.forecast_warned = false;
                    continue;
                }
                // A clear drop between readings is the window rolling over.
                if previous.is_some_and(|p| current + ROLLOVER_DROP < p) {
                    state.forecast_warned = false;
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
                    state.forecast_warned = false;
                    if settings.used_up {
                        to_notify.push((
                            format!("{name} limit reset"),
                            format!("{name} {label} window has reset."),
                        ));
                    }
                    continue;
                }

                // On the very first observation after launch we only record
                // state: a limit that was already exhausted before the app
                // started isn't news.
                let first = previous.is_none();
                let pct = current.round() as u8;
                let runs_out = crate::forecast::runs_out_at(gauge, now);
                let forecast_text = runs_out
                    .map(|at| {
                        format!(
                            " At this pace it runs out at {}, {} before it resets.",
                            clock_text(at),
                            gap_text(reset_at - at)
                        )
                    })
                    .unwrap_or_default();

                if current >= 100.0 {
                    if state.exhausted.is_none() {
                        state.exhausted = Some(Exhaustion {
                            reset_at,
                            reset_check_done: false,
                        });
                        if !first && settings.used_up {
                            let mut body = format!("{name} {label} window is at 100% used.");
                            // Draining the main limit is exactly when a reserve
                            // starts to matter, so say it's there.
                            if provider == Provider::Codex && group != "reserve" {
                                if let Some(reserve) = codex_reserve_left(usage) {
                                    body.push_str(&format!(" {} takes over from here.", reserve.label));
                                }
                            }
                            if reset_at > now {
                                body.push_str(&format!(
                                    " It comes back at {}, in {}.",
                                    clock_text(reset_at),
                                    gap_text(reset_at - now)
                                ));
                            }
                            if let Some(resets) = banked_resets_text(usage, provider, &account) {
                                body.push_str(&format!(" {resets} Open the panel to use one."));
                            }
                            to_notify.push((format!("{name} limit used up"), body));
                        }
                    }
                    continue;
                }

                if let Some(step) = steps.iter().rev().copied().find(|step| current >= *step) {
                    // Notify on entering the warn band and again only on
                    // reaching a higher step. An unchanged reading, a climb
                    // within a step, or the reset_at drift a rolling window
                    // emits each poll leaves warned_at untouched.
                    let step = step as u8;
                    let climbed = state.warned_at.is_none_or(|prev| step > prev);
                    if climbed {
                        state.warned_at = Some(step);
                        if !first {
                            // The forecast rides along rather than following
                            // as a second toast about the same window.
                            if runs_out.is_some() {
                                state.forecast_warned = true;
                            }
                            to_notify.push((
                                format!("{name} limit almost used"),
                                format!("{name} {label} window is at {pct}% used.{forecast_text}"),
                            ));
                            continue;
                        }
                    }
                } else if warn_clear.is_none_or(|clear| current < clear) {
                    state.warned_at = None;
                }

                if settings.forecast && runs_out.is_some() && !state.forecast_warned {
                    state.forecast_warned = true;
                    if !first {
                        to_notify.push((
                            format!("{name} may run out early"),
                            format!("{name} {label} window is at {pct}% used.{forecast_text}"),
                        ));
                    }
                }
            }

            to_notify.extend(reset_events(
                &mut runtime.resets,
                settings.codex_resets,
                usage.codex.resets.as_ref(),
                Utc::now().timestamp(),
            ));
            to_notify.extend(claude_reset_events(
                &mut runtime.claude_resets,
                settings.claude_resets,
                &usage.claude.accounts,
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
/// - runs a refresh that the throttle deferred,
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

                let interval = if app.state::<crate::widget::WidgetState>().enabled() {
                    WIDGET_POLL_INTERVAL_SECS
                } else {
                    POLL_INTERVAL_SECS
                };
                let now = Utc::now().timestamp();
                // Any collection counts as the poll, whoever started it.
                last_poll = last_poll.max(*state.last_refresh.lock().unwrap());
                let mut due = now - last_poll >= interval
                    || state.refresh_pending.load(Ordering::SeqCst);
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
            // Deferred rather than dropped: the ticker picks it up once the
            // throttle allows, so the final change in a burst isn't lost.
            if now - *last < MIN_REFRESH_SECS
                || state.refresh_inflight.swap(true, Ordering::SeqCst)
            {
                state.refresh_pending.store(true, Ordering::SeqCst);
                return;
            }
            state.refresh_pending.store(false, Ordering::SeqCst);
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
        // Both windows repaint from the cached snapshot this just replaced.
        // Not "refresh": that makes the panel collect again, asking every
        // provider a second time for the numbers this already has.
        let _ = app.emit("usage-updated", ());
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
                    label: sentence_label(quota),
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

/// A window's label for use mid-sentence. Generic window names are lowercased
/// ("weekly limit almost used"); a named allowance such as Luna Reserve keeps
/// its capitalisation, since it is a name rather than a description.
fn sentence_label(quota: &QuotaWindow) -> String {
    if matches!(quota.group.as_str(), "reserve" | "additional") {
        quota.label.clone()
    } else {
        quota.label.to_ascii_lowercase()
    }
}

/// The reserve allowance still available once a main Codex window is used up,
/// for the "used up" notification to point at. None when there is no reserve,
/// or it is itself spent.
fn codex_reserve_left(usage: &Usage) -> Option<&QuotaWindow> {
    usage
        .codex
        .quotas
        .iter()
        .find(|quota| quota.group == "reserve" && quota.gauge.used_percent < 100.0)
}

/// "You have 2 banked resets." for the account a used-up window belongs to, so
/// the toast says there is a way back before the window refills by itself.
fn banked_resets_text(usage: &Usage, provider: Provider, account: &str) -> Option<String> {
    let (count, noun) = match provider {
        Provider::Codex => (
            usage
                .codex
                .resets
                .as_ref()
                .filter(|r| r.available)
                .map_or(0, |r| r.available_count),
            "reset credit",
        ),
        Provider::Claude => (
            usage
                .claude
                .accounts
                .iter()
                .find(|a| a.id == account)
                .and_then(|a| a.resets.as_ref())
                .map_or(0, |r| r.available_count),
            "banked reset",
        ),
    };
    (count > 0).then(|| format!("You have {count} {noun}{}.", if count == 1 { "" } else { "s" }))
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

fn claude_stage_body(who: &str, stage: &str) -> String {
    match stage {
        "day" => format!("A {who} reset expires in about a day."),
        "today" => format!("A {who} reset expires today."),
        "hours" => format!("A {who} reset expires in a few hours — use it before it's gone."),
        _ => format!("A {who} reset is expiring."),
    }
}

/// Notifications for Claude's banked resets: the count going up, and each
/// expiry stage of the grant that would be spent next. Accounts whose live
/// fetch failed are skipped untouched, so a dropped request never reads as
/// "resets gone" and their return never reads as "resets granted".
fn claude_reset_events(
    state: &mut ClaudeResetAlertState,
    enabled: bool,
    accounts: &[ClaudeAccountUsage],
    now: i64,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let multi = accounts.len() > 1;
    for acct in accounts.iter().filter(|a| a.live) {
        let who = if multi {
            format!("Claude ({})", acct.label)
        } else {
            "Claude".to_string()
        };
        let count = acct.resets.as_ref().map_or(0, |r| r.available_count);
        let grant = acct.resets.as_ref().and_then(|r| r.next_credit_id.clone());
        let expires_at = acct.resets.as_ref().and_then(|r| r.next_expires_at);

        let first = !state.known.contains_key(&acct.id);
        let entry = state.known.entry(acct.id.clone()).or_default();
        let emit = enabled && !first;

        if count > entry.count && emit {
            let body = match acct.resets.as_ref().and_then(|r| r.next_expires_in.as_deref()) {
                Some(left) => format!("A {who} usage-limit reset is available — use it within {left}."),
                None => format!("A {who} usage-limit reset is available."),
            };
            out.push(("Claude reset available".to_string(), body));
        }
        entry.count = count;

        if entry.grant != grant {
            entry.grant = grant;
            entry.fired_stages.clear();
        }
        if let Some(exp) = expires_at.filter(|_| count > 0) {
            for stage in stages_met(now, exp) {
                if entry.fired_stages.insert(stage) && emit {
                    out.push(("Claude reset expiring".to_string(), claude_stage_body(&who, stage)));
                }
            }
        }
    }
    // Forget removed accounts, so re-adding one is a fresh baseline.
    state
        .known
        .retain(|id, _| accounts.iter().any(|a| &a.id == id));
    out
}

fn usage_watch_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(codex) = crate::util::codex_home() {
        paths.push(codex.join("sessions"));
        paths.push(codex.join("auth.json"));
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
    let body = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    crate::util::write_atomic(&path, body.as_bytes()).map_err(|e| e.to_string())
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
                ..Default::default()
            }),
            runtime: Mutex::new(AlertRuntime::default()),
            watcher: Mutex::new(None),
            watched: Mutex::new(HashSet::new()),
            generation: AtomicU64::new(0),
            refresh_inflight: AtomicBool::new(false),
            last_refresh: Mutex::new(0),
            refresh_pending: AtomicBool::new(false),
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
            resets: None,
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

        // A climb within the step stays quiet too: one per percent from 90
        // could send ten warnings on the way to "used up".
        assert!(state
            .notification_events(&codex_weekly_usage(93.0, 1_160), 100)
            .is_empty());

        // Reaching the next step is surfaced, once, with the real figure.
        let events = state.notification_events(&codex_weekly_usage(96.0, 1_190), 101);
        assert_eq!(events.len(), 1);
        assert!(events[0].1.contains("96%"));
        assert!(state
            .notification_events(&codex_weekly_usage(99.0, 1_220), 102)
            .is_empty());
    }

    /// A Codex five-hour window `elapsed` of the way through right now, so the
    /// forecast (which reads the wall clock) has something to judge.
    fn codex_paced(used_percent: f64, elapsed: f64) -> Usage {
        let span = 300 * 60;
        let resets_at = Utc::now().timestamp() + ((1.0 - elapsed) * span as f64) as i64;
        codex_session_usage(used_percent, resets_at)
    }

    #[test]
    fn a_window_on_pace_to_run_out_warns_once_per_cycle() {
        let state = test_state();
        assert!(state.notification_events(&codex_paced(40.0, 0.4), 1).is_empty());

        let events = state.notification_events(&codex_paced(86.0, 0.81), 2);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, "Codex may run out early");
        assert!(events[0].1.contains("runs out at"), "{}", events[0].1);
        assert!(state.notification_events(&codex_paced(87.0, 0.82), 3).is_empty());

        // The window rolls over, then runs hot again: that's a new warning.
        assert!(state.notification_events(&codex_paced(10.0, 0.05), 4).is_empty());
        let events = state.notification_events(&codex_paced(86.0, 0.81), 5);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn a_warn_level_carries_the_forecast_instead_of_a_second_toast() {
        let state = test_state();
        state.notification_events(&codex_paced(40.0, 0.4), 1);
        let events = state.notification_events(&codex_paced(91.0, 0.85), 2);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, "Codex limit almost used");
        assert!(events[0].1.contains("runs out at"));
    }

    #[test]
    fn custom_warn_levels_each_fire_once() {
        let state = test_state();
        state.settings.lock().unwrap().warn_levels = vec![75, 50, 50, 120];
        state.settings.lock().unwrap().forecast = false;
        assert!(state.notification_events(&codex_session_usage(40.0, 1_000), 1).is_empty());
        let events = state.notification_events(&codex_session_usage(60.0, 1_000), 2);
        assert_eq!(events.len(), 1);
        assert!(events[0].1.contains("60%"));
        assert!(state.notification_events(&codex_session_usage(70.0, 1_000), 3).is_empty());
        assert_eq!(state.notification_events(&codex_session_usage(80.0, 1_000), 4).len(), 1);
        assert!(state.notification_events(&codex_session_usage(85.0, 1_000), 5).is_empty());
    }

    #[test]
    fn used_up_alerts_can_be_switched_off() {
        let state = test_state();
        state.settings.lock().unwrap().used_up = false;
        state.notification_events(&codex_session_usage(95.0, 1_000), 1);
        assert!(state.notification_events(&codex_session_usage(100.0, 1_000), 2).is_empty());
        assert!(state.notification_events(&codex_session_usage(20.0, 2_000), 3).is_empty());
    }

    #[test]
    fn quiet_hours_can_run_past_midnight() {
        let mut settings = NotificationSettings {
            quiet_hours: true,
            ..Default::default()
        };
        assert!(settings.quiet_at(23 * 60));
        assert!(settings.quiet_at(7 * 60 + 59));
        assert!(!settings.quiet_at(8 * 60));
        assert!(!settings.quiet_at(12 * 60));
        settings.quiet_start = 13 * 60;
        settings.quiet_end = 14 * 60;
        assert!(settings.quiet_at(13 * 60 + 30));
        assert!(!settings.quiet_at(23 * 60));
        settings.quiet_hours = false;
        assert!(!settings.quiet_at(13 * 60 + 30));
    }

    #[test]
    fn settings_written_before_the_new_options_keep_the_old_behaviour() {
        let settings: NotificationSettings =
            serde_json::from_str(r#"{"codex":true,"claude":false}"#).unwrap();
        assert_eq!(settings.warn_steps(), vec![90.0, 95.0]);
        assert!(settings.used_up && settings.forecast && !settings.quiet_hours);
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

    fn codex_with_reserve(main_used: f64, reserve_used: f64) -> Usage {
        let quota = |id: &str, label: &str, group: &str, used: f64| QuotaWindow {
            id: id.to_string(),
            label: label.to_string(),
            group: group.to_string(),
            gauge: Gauge {
                used_percent: used,
                window_minutes: 300,
                resets_at: Some(1_000),
                resets_in: None,
            },
            ..Default::default()
        };
        Usage {
            codex: CodexUsage {
                available: true,
                live: true,
                quotas: vec![
                    quota("session", "Session (5h)", "session", main_used),
                    quota("codex_reserve:weekly", "Luna Reserve", "reserve", reserve_used),
                ],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Draining the main limit is when the reserve starts to matter, so the
    /// "used up" notice points at it — and names it with its capitals intact.
    #[test]
    fn a_drained_main_limit_points_at_the_reserve() {
        let state = test_state();
        assert!(state
            .notification_events(&codex_with_reserve(95.0, 0.0), 1)
            .is_empty());
        let events = state.notification_events(&codex_with_reserve(100.0, 0.0), 2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "Codex limit used up");
        assert!(events[0].1.contains("Luna Reserve takes over"), "{}", events[0].1);

        // The reserve's own warnings keep its name capitalised.
        let events = state.notification_events(&codex_with_reserve(100.0, 92.0), 3);
        assert_eq!(events.len(), 1);
        assert!(events[0].1.starts_with("Codex Luna Reserve window"), "{}", events[0].1);
    }

    #[test]
    fn a_spent_reserve_is_not_offered() {
        let state = test_state();
        state.notification_events(&codex_with_reserve(95.0, 100.0), 1);
        let events = state.notification_events(&codex_with_reserve(100.0, 100.0), 2);
        let used_up: Vec<_> = events.iter().filter(|e| e.1.contains("Session")).collect();
        assert!(used_up.iter().all(|e| !e.1.contains("takes over")));
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

    fn claude_account(count: u64, expires_at: Option<i64>) -> ClaudeAccountUsage {
        ClaudeAccountUsage {
            id: "dir".to_string(),
            label: "Pro".to_string(),
            live: true,
            resets: Some(crate::models::ClaudeResets {
                available_count: count,
                next_credit_id: (count > 0).then(|| "tenure".to_string()),
                next_expires_at: expires_at,
                next_expires_in: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn claude_reset_baseline_is_silent_then_a_new_reset_notifies() {
        let mut st = ClaudeResetAlertState::default();
        let now = 1_000_000;
        let far = Some(now + 30 * 86400);

        assert!(claude_reset_events(&mut st, true, &[claude_account(1, far)], now).is_empty());
        assert!(claude_reset_events(&mut st, true, &[claude_account(1, far)], now).is_empty());

        let ev = claude_reset_events(&mut st, true, &[claude_account(2, far)], now);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, "Claude reset available");

        // Spending one is not news.
        assert!(claude_reset_events(&mut st, true, &[claude_account(1, far)], now).is_empty());
    }

    #[test]
    fn claude_reset_skips_failed_fetches_and_fires_expiry_once() {
        let mut st = ClaudeResetAlertState::default();
        let expires = 1_700_000_000;
        let acct = claude_account(1, Some(expires));
        assert!(claude_reset_events(&mut st, true, &[acct], expires - 5 * 86400).is_empty());

        // A failed fetch serves no resets; it must not reset the count.
        let mut down = claude_account(0, None);
        down.live = false;
        assert!(claude_reset_events(&mut st, true, &[down], expires - 5 * 86400).is_empty());

        let ev = claude_reset_events(
            &mut st,
            true,
            &[claude_account(1, Some(expires))],
            expires - 10 * 3600,
        );
        assert!(ev.iter().all(|(t, _)| t == "Claude reset expiring"));
        assert!(ev.iter().any(|(_, b)| b.contains("about a day")));
        let ev = claude_reset_events(
            &mut st,
            true,
            &[claude_account(1, Some(expires))],
            expires - 10 * 3600,
        );
        assert!(ev.is_empty());
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
