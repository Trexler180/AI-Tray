//! Start on login.
//!
//! The Run-key entry is re-registered on every launch while this is on, which
//! keeps it pointed at wherever the exe currently lives (an update or a moved
//! portable copy would otherwise leave it dangling). That same re-registration
//! is why the choice has to be stored here: without it, removing the entry by
//! any means other than Task Manager lasted only until the next launch.

use crate::util::{config_dir, write_atomic};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri_plugin_autostart::ManagerExt;

#[derive(Serialize, Deserialize)]
struct StartupSettings {
    /// On by default, which is how the app has always behaved.
    #[serde(default = "yes")]
    start_on_login: bool,
}

fn yes() -> bool {
    true
}

impl Default for StartupSettings {
    fn default() -> Self {
        Self {
            start_on_login: true,
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    Some(config_dir()?.join("startup.json"))
}

fn load() -> StartupSettings {
    settings_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

fn persist(settings: &StartupSettings) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "settings directory unavailable".to_string())?;
    let body = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    write_atomic(&path, body.as_bytes()).map_err(|e| e.to_string())
}

/// Bring the OS registration in line with the stored choice.
fn register(app: &tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    if enabled {
        launcher.enable().map_err(|e| e.to_string())
    } else if launcher.is_enabled().unwrap_or(true) {
        launcher.disable().map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

/// Run once at startup.
pub fn apply_on_launch(app: &tauri::AppHandle) {
    if let Err(error) = register(app, load().start_on_login) {
        eprintln!("start-on-login registration failed: {error}");
    }
}

#[tauri::command]
pub fn get_start_on_login() -> bool {
    load().start_on_login
}

#[tauri::command]
pub fn set_start_on_login(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    register(&app, enabled)?;
    persist(&StartupSettings {
        start_on_login: enabled,
    })?;
    Ok(enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_empty_file_keeps_starting_on_login() {
        assert!(StartupSettings::default().start_on_login);
        let settings: StartupSettings = serde_json::from_str("{}").unwrap();
        assert!(settings.start_on_login);
    }
}
