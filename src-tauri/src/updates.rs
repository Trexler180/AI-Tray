//! Automatic update checks against GitHub Releases, on one of two channels.
//!
//! **Stable** is the feed every install has always read: the `latest.json` of
//! the newest published, non-prerelease release. Builds from before channels
//! existed keep reading exactly that URL, so they only ever see stable.
//!
//! **Nightly** builds are published as prereleases, which GitHub never counts
//! as "latest", and the release workflow points a fixed `nightly` release's
//! `latest.json` at the newest one. A nightly install checks both feeds and
//! takes whichever version is higher, so a stable release that overtakes the
//! nightlies still reaches it. Nightly versions are semver prereleases of the
//! next minor (`0.11.0-nightly.3`), which sort above the current stable and
//! below the stable they lead up to.
//!
//! The default posture is deliberately conservative: check in the background,
//! tell the user, and let them press Install. A tray app that restarts itself
//! mid-task is worse than one that waits, so silent installation is opt-in.
//!
//! Settings live in their own `updates.json` rather than the shared
//! `settings.json`, because `alerts::persist_settings` serializes only
//! `NotificationSettings` and would drop any foreign key on the next
//! notification toggle.

use crate::util::config_dir;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;

/// Ticker cadence. Wall-clock comparisons, so it survives system suspend.
const TICK: Duration = Duration::from_secs(30);
/// Wait before the first check so launch isn't competing with the initial usage
/// collection for network and CPU.
const FIRST_CHECK_DELAY_SECS: i64 = 60;
/// Spacing between automatic checks.
const CHECK_INTERVAL_SECS: i64 = 6 * 60 * 60;
/// Event emitted whenever the status changes, so an open panel repaints without
/// polling.
const STATE_EVENT: &str = "update-state";
/// The stable feed. Unchanged from before channels, which is what keeps older
/// installs updating.
const STABLE_ENDPOINT: &str =
    "https://github.com/Trexler180/AI-Tray/releases/latest/download/latest.json";
/// The nightly feed: a fixed prerelease whose `latest.json` the release
/// workflow replaces on every nightly build.
const NIGHTLY_ENDPOINT: &str =
    "https://github.com/Trexler180/AI-Tray/releases/download/nightly/latest.json";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Nightly,
}

impl Channel {
    /// Which channel a version belongs to: nightlies carry a `-nightly`
    /// prerelease tag, everything else is stable.
    pub fn of(version: &str) -> Self {
        if version.contains("-nightly") {
            Channel::Nightly
        } else {
            Channel::Stable
        }
    }

    fn label(self) -> &'static str {
        match self {
            Channel::Stable => "Stable",
            Channel::Nightly => "Nightly",
        }
    }

    /// The feeds this channel reads. Every one is checked and the highest
    /// version wins, so order doesn't matter.
    fn endpoints(self) -> &'static [&'static str] {
        match self {
            Channel::Stable => &[STABLE_ENDPOINT],
            Channel::Nightly => &[NIGHTLY_ENDPOINT, STABLE_ENDPOINT],
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct UpdateSettings {
    /// Poll the release endpoint in the background.
    #[serde(default = "enabled_by_default")]
    pub check_automatically: bool,
    /// Download, install, and relaunch without asking. Off by default.
    #[serde(default)]
    pub install_automatically: bool,
    /// The channel picked in Settings. Unset means "whatever this build is",
    /// so a nightly install keeps getting nightlies without being told to.
    #[serde(default)]
    pub channel: Option<Channel>,
}

fn enabled_by_default() -> bool {
    true
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check_automatically: true,
            install_automatically: false,
            channel: None,
        }
    }
}

/// What Settings → About renders. Serialized as `{"kind": "...", ...}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdateStatus {
    /// Nothing has happened yet this session.
    #[default]
    Idle,
    Checking,
    /// A check completed and this build is current.
    UpToDate,
    Available {
        version: String,
        /// "stable" or "nightly", from the version itself.
        channel: Channel,
        notes: String,
    },
    Downloading {
        percent: u8,
    },
    /// Installer handed off; the app is about to be replaced.
    Installing {
        version: String,
    },
    Error {
        message: String,
    },
}

/// Flattened view handed to the frontend.
#[derive(Clone, Serialize)]
pub struct UpdateSnapshot {
    pub status: UpdateStatus,
    pub current_version: String,
    /// The channel this build came from.
    pub current_channel: Channel,
    /// The channel checks read: the one picked in Settings, else the build's.
    pub channel: Channel,
    pub last_checked_at: Option<i64>,
    pub settings: UpdateSettings,
}

pub struct UpdateState {
    settings: Mutex<UpdateSettings>,
    status: Mutex<UpdateStatus>,
    last_checked_at: Mutex<Option<i64>>,
    /// Version a notification already announced, so a pending update notifies
    /// once instead of every six hours.
    notified_version: Mutex<Option<String>>,
    /// A check or install is already running. The ticker and the manual button
    /// share this so they can't overlap.
    inflight: AtomicBool,
}

impl UpdateState {
    pub fn load() -> Self {
        Self {
            settings: Mutex::new(load_settings()),
            status: Mutex::new(UpdateStatus::default()),
            last_checked_at: Mutex::new(None),
            notified_version: Mutex::new(None),
            inflight: AtomicBool::new(false),
        }
    }

    pub fn settings(&self) -> UpdateSettings {
        self.settings.lock().unwrap().clone()
    }

    fn set_status(&self, status: UpdateStatus) {
        *self.status.lock().unwrap() = status;
    }

    /// The channel to check, given the running version.
    fn channel(&self, current_version: &str) -> Channel {
        self.settings().channel.unwrap_or_else(|| Channel::of(current_version))
    }

    pub fn set_channel(&self, channel: Channel) -> Result<UpdateSettings, String> {
        let updated = {
            let mut settings = self.settings.lock().unwrap();
            settings.channel = Some(channel);
            settings.clone()
        };
        persist_settings(&updated)?;
        // A pending "available" was the old channel's answer.
        self.set_status(UpdateStatus::Idle);
        Ok(updated)
    }

    pub fn snapshot(&self, current_version: String) -> UpdateSnapshot {
        UpdateSnapshot {
            status: self.status.lock().unwrap().clone(),
            current_channel: Channel::of(&current_version),
            channel: self.channel(&current_version),
            current_version,
            last_checked_at: *self.last_checked_at.lock().unwrap(),
            settings: self.settings(),
        }
    }

    /// Claim the right to run a check/install. Returns false when one is
    /// already in flight.
    fn begin(&self) -> bool {
        !self.inflight.swap(true, Ordering::SeqCst)
    }

    fn end(&self) {
        self.inflight.store(false, Ordering::SeqCst);
    }

    pub fn set_flag(&self, key: &str, enabled: bool) -> Result<UpdateSettings, String> {
        let updated = {
            let mut settings = self.settings.lock().unwrap();
            match key {
                "check_automatically" => settings.check_automatically = enabled,
                "install_automatically" => settings.install_automatically = enabled,
                other => return Err(format!("unknown update setting: {other}")),
            }
            settings.clone()
        };
        persist_settings(&updated)?;
        Ok(updated)
    }
}

/// True when this version hasn't been announced yet. Pulled out so the
/// notify-once rule is testable without a running app.
fn should_notify(already_notified: &Option<String>, version: &str) -> bool {
    already_notified.as_deref() != Some(version)
}

fn broadcast(app: &AppHandle) {
    let state = app.state::<UpdateState>();
    let snapshot = state.snapshot(app.package_info().version.to_string());
    let _ = app.emit(STATE_EVENT, snapshot);
}

fn send_notification(app: &AppHandle, title: &str, body: &str) {
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        eprintln!("notification \"{title}\" failed: {e}");
    }
}

/// Run one check. `manual` distinguishes a user-initiated check (which reports
/// "up to date" explicitly) from a background poll (which stays quiet).
pub async fn check(app: &AppHandle, manual: bool) {
    {
        let state = app.state::<UpdateState>();
        if !state.begin() {
            return;
        }
        state.set_status(UpdateStatus::Checking);
    }
    broadcast(app);

    let outcome = run_check(app).await;

    {
        let state = app.state::<UpdateState>();
        *state.last_checked_at.lock().unwrap() = Some(Utc::now().timestamp());

        match outcome {
            Ok(Some((version, notes))) => {
                let auto_install = state.settings().install_automatically;
                let channel = Channel::of(&version);
                let mut notified = state.notified_version.lock().unwrap();
                if !auto_install && should_notify(&notified, &version) {
                    *notified = Some(version.clone());
                    drop(notified);
                    send_notification(
                        app,
                        "AI Usage Tray update available",
                        &format!("{} {version} is ready to install.", channel.label()),
                    );
                }
                state.set_status(UpdateStatus::Available {
                    version,
                    channel,
                    notes,
                });
            }
            Ok(None) => {
                state.set_status(if manual {
                    UpdateStatus::UpToDate
                } else {
                    UpdateStatus::Idle
                });
            }
            Err(message) => state.set_status(UpdateStatus::Error { message }),
        }
        state.end();
    }
    broadcast(app);

    // Auto-install is handled after the status settles so the panel shows the
    // found version before the installer takes the app down.
    let should_install = {
        let state = app.state::<UpdateState>();
        let available = matches!(*state.status.lock().unwrap(), UpdateStatus::Available { .. });
        available && state.settings().install_automatically
    };
    if should_install {
        let _ = install(app).await;
    }
}

/// Ask the channel's feeds whether a newer build exists. `Ok(None)` means
/// current.
async fn run_check(app: &AppHandle) -> Result<Option<(String, String)>, String> {
    Ok(find_update(app).await?.map(|update| {
        let notes = update.body.clone().unwrap_or_default();
        (update.version.clone(), notes)
    }))
}

/// The highest version newer than this build across the channel's feeds.
/// A feed that can't be read only fails the check when every feed failed: the
/// nightly feed being briefly missing mustn't hide a stable release.
async fn find_update(app: &AppHandle) -> Result<Option<tauri_plugin_updater::Update>, String> {
    let current = app.package_info().version.to_string();
    let channel = app.state::<UpdateState>().channel(&current);
    let mut best: Option<tauri_plugin_updater::Update> = None;
    let mut errors = Vec::new();
    for endpoint in channel.endpoints() {
        let url = tauri::Url::parse(endpoint).map_err(|e| e.to_string())?;
        let updater = app
            .updater_builder()
            .endpoints(vec![url])
            .and_then(|builder| builder.build())
            .map_err(|e| e.to_string())?;
        match updater.check().await {
            Ok(Some(update)) => {
                if best.as_ref().is_none_or(|b| is_newer(&update.version, &b.version)) {
                    best = Some(update);
                }
            }
            Ok(None) => {}
            Err(e) => errors.push(e.to_string()),
        }
    }
    if best.is_none() && errors.len() == channel.endpoints().len() {
        return Err(errors.join("; "));
    }
    Ok(best)
}

/// Semver order, so `0.11.0-nightly.2` > `0.11.0-nightly.1` > `0.10.0` and
/// `0.11.0` > `0.11.0-nightly.9`. Anything unparsable loses.
fn is_newer(candidate: &str, than: &str) -> bool {
    match (semver::Version::parse(candidate), semver::Version::parse(than)) {
        (Ok(a), Ok(b)) => a > b,
        (Ok(_), Err(_)) => true,
        _ => false,
    }
}

/// Download and run the installer for whatever the endpoint currently offers.
///
/// This re-checks rather than reusing the handle from the earlier check: the
/// `Update` value isn't practically storable across command invocations, and a
/// second cheap request is preferable to holding it in shared state.
pub async fn install(app: &AppHandle) -> Result<(), String> {
    {
        let state = app.state::<UpdateState>();
        if !state.begin() {
            return Err("an update check is already running".to_string());
        }
    }

    let result = run_install(app).await;

    {
        let state = app.state::<UpdateState>();
        if let Err(message) = &result {
            state.set_status(UpdateStatus::Error {
                message: message.clone(),
            });
        }
        state.end();
    }
    broadcast(app);

    if result.is_ok() {
        // On Windows the NSIS installer normally terminates and relaunches the
        // app itself, so this is a fallback for the cases where it doesn't.
        app.restart();
    }
    result
}

async fn run_install(app: &AppHandle) -> Result<(), String> {
    let update = find_update(app)
        .await?
        .ok_or_else(|| "no update is available".to_string())?;

    let version = update.version.clone();
    {
        let state = app.state::<UpdateState>();
        state.set_status(UpdateStatus::Downloading { percent: 0 });
    }
    broadcast(app);

    let handle = app.clone();
    let mut downloaded: u64 = 0;
    let mut last_percent: Option<u8> = None;
    update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk as u64;
                let percent = match total {
                    Some(total) if total > 0 => {
                        ((downloaded as f64 / total as f64) * 100.0).min(100.0) as u8
                    }
                    _ => 0,
                };
                // Chunks arrive far faster than the percentage moves; each
                // broadcast re-renders the panel, so only send real progress.
                if last_percent == Some(percent) {
                    return;
                }
                last_percent = Some(percent);
                let state = handle.state::<UpdateState>();
                state.set_status(UpdateStatus::Downloading { percent });
                broadcast(&handle);
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;

    {
        let state = app.state::<UpdateState>();
        state.set_status(UpdateStatus::Installing { version });
    }
    broadcast(app);
    Ok(())
}

/// Background poller. Mirrors `alerts::start_alert_ticker`: a short tick with
/// wall-clock comparisons, so a suspended machine doesn't skew the schedule.
pub fn start_update_ticker(app: AppHandle) {
    let _ = std::thread::Builder::new()
        .name("update-check-ticker".to_string())
        .spawn(move || {
            let started = Utc::now().timestamp();
            let mut last_check: Option<i64> = None;
            loop {
                std::thread::sleep(TICK);

                if !app.state::<UpdateState>().settings().check_automatically {
                    continue;
                }

                let now = Utc::now().timestamp();
                let due = match last_check {
                    None => now - started >= FIRST_CHECK_DELAY_SECS,
                    Some(previous) => now - previous >= CHECK_INTERVAL_SECS,
                };
                if !due {
                    continue;
                }
                last_check = Some(now);

                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    check(&handle, false).await;
                });
            }
        });
}

fn settings_path() -> Option<PathBuf> {
    let mut root = config_dir()?;
    root.push("updates.json");
    Some(root)
}

fn load_settings() -> UpdateSettings {
    let Some(path) = settings_path() else {
        return UpdateSettings::default();
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

fn persist_settings(settings: &UpdateSettings) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "settings directory unavailable".to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    crate::util::write_atomic(&path, body.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checking_is_on_and_installing_is_off_by_default() {
        let settings = UpdateSettings::default();
        assert!(settings.check_automatically);
        assert!(!settings.install_automatically);
    }

    #[test]
    fn absent_fields_keep_the_conservative_defaults() {
        // An empty or partial file must not silently turn auto-install on, and
        // must not turn auto-check off.
        let settings: UpdateSettings = serde_json::from_str("{}").unwrap();
        assert!(settings.check_automatically);
        assert!(!settings.install_automatically);

        let partial: UpdateSettings =
            serde_json::from_str(r#"{"install_automatically":true}"#).unwrap();
        assert!(partial.check_automatically);
        assert!(partial.install_automatically);
    }

    #[test]
    fn settings_round_trip_through_json() {
        let settings = UpdateSettings {
            check_automatically: false,
            install_automatically: true,
            channel: Some(Channel::Nightly),
        };
        let body = serde_json::to_string(&settings).unwrap();
        let back: UpdateSettings = serde_json::from_str(&body).unwrap();
        assert!(!back.check_automatically);
        assert!(back.install_automatically);
        assert_eq!(back.channel, Some(Channel::Nightly));
    }

    #[test]
    fn versions_name_their_channel() {
        assert_eq!(Channel::of("0.10.0"), Channel::Stable);
        assert_eq!(Channel::of("0.11.0-nightly.3"), Channel::Nightly);
        // Settings written before channels existed pick nothing, which means
        // "follow the build".
        let old: UpdateSettings = serde_json::from_str(r#"{"check_automatically":true}"#).unwrap();
        assert_eq!(old.channel, None);
    }

    #[test]
    fn nightlies_sit_between_the_stables_either_side() {
        assert!(is_newer("0.11.0-nightly.1", "0.10.0"));
        assert!(is_newer("0.11.0-nightly.2", "0.11.0-nightly.1"));
        assert!(is_newer("0.11.0-nightly.10", "0.11.0-nightly.9"));
        assert!(is_newer("0.11.0", "0.11.0-nightly.9"));
        assert!(!is_newer("0.10.0", "0.11.0-nightly.1"));
        assert!(!is_newer("garbage", "0.10.0"));
    }

    #[test]
    fn the_nightly_channel_also_reads_the_stable_feed() {
        assert_eq!(Channel::Stable.endpoints(), &[STABLE_ENDPOINT]);
        assert!(Channel::Nightly.endpoints().contains(&STABLE_ENDPOINT));
        assert!(Channel::Nightly.endpoints().contains(&NIGHTLY_ENDPOINT));
    }

    #[test]
    fn a_pending_version_notifies_once_but_a_newer_one_notifies_again() {
        let mut notified: Option<String> = None;
        assert!(should_notify(&notified, "0.2.0"));

        notified = Some("0.2.0".to_string());
        assert!(!should_notify(&notified, "0.2.0"));

        // A newer release supersedes the one already announced.
        assert!(should_notify(&notified, "0.3.0"));
    }

    #[test]
    fn status_serializes_with_a_kind_tag_the_panel_can_switch_on() {
        let body = serde_json::to_string(&UpdateStatus::Available {
            version: "0.2.0".to_string(),
            channel: Channel::Stable,
            notes: "Fixes".to_string(),
        })
        .unwrap();
        assert!(body.contains(r#""kind":"available""#));
        assert!(body.contains(r#""version":"0.2.0""#));

        let idle = serde_json::to_string(&UpdateStatus::Idle).unwrap();
        assert_eq!(idle, r#"{"kind":"idle"}"#);
    }
}
