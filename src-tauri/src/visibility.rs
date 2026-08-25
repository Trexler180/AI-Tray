//! Which accounts each surface draws.
//!
//! Hiding an account is a display choice, never a removal: the Claude folder
//! stays registered, its credentials are left alone, its usage keeps being
//! collected and recorded, and turning it back on restores everything at once.
//! That is the difference between this and `accounts::remove_dir`, which
//! forgets the folder outright.
//!
//! The panel and the taskbar widget keep separate lists. They are read in very
//! different ways — the panel is opened deliberately and has room for
//! everything, the widget is a permanent few pixels of taskbar with one cell
//! per account — so "show me all four here, only the one I'm burning there" is
//! the normal case rather than an edge one.
//!
//! What is stored is the *hidden* set, not the visible one. An account the app
//! has never seen (a folder added later, a Codex login that appears once you
//! sign in) is therefore shown by default instead of silently missing from a
//! list written before it existed.

use crate::util::{config_dir, write_atomic};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AccountVisibility {
    /// Ids hidden from the popover panel.
    #[serde(default)]
    pub panel_hidden: Vec<String>,
    /// Ids hidden from the taskbar widget.
    #[serde(default)]
    pub widget_hidden: Vec<String>,
}

impl AccountVisibility {
    fn list_mut(&mut self, surface: &str) -> Option<&mut Vec<String>> {
        match surface {
            "panel" => Some(&mut self.panel_hidden),
            "widget" => Some(&mut self.widget_hidden),
            _ => None,
        }
    }
}

/// Add or drop one id, reporting whether the list actually moved. Idempotent
/// on purpose: hiding something already hidden must not write a duplicate that
/// a later unhide would only half remove.
fn toggle(list: &mut Vec<String>, id: &str, hidden: bool) -> bool {
    let at = list.iter().position(|entry| entry == id);
    match (hidden, at) {
        (true, None) => {
            list.push(id.to_string());
            true
        }
        (false, Some(at)) => {
            list.remove(at);
            true
        }
        _ => false,
    }
}

pub struct VisibilityState(Mutex<AccountVisibility>);

impl VisibilityState {
    pub fn load() -> Self {
        Self(Mutex::new(load_file()))
    }

    /// A poisoned lock means another thread panicked mid-update. The list is
    /// still readable, and losing every account from both surfaces is a far
    /// worse outcome than carrying on with it.
    fn get(&self) -> MutexGuard<'_, AccountVisibility> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn store_path() -> Option<PathBuf> {
    let mut root = config_dir()?;
    root.push("account-visibility.json");
    Some(root)
}

fn load_file() -> AccountVisibility {
    store_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

fn persist(state: &AccountVisibility) -> Result<(), String> {
    let path = store_path().ok_or_else(|| "settings directory unavailable".to_string())?;
    let body = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    write_atomic(&path, body.as_bytes()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_account_visibility(state: tauri::State<'_, VisibilityState>) -> AccountVisibility {
    state.get().clone()
}

/// Show or hide one account on one surface.
///
/// Broadcast to every window rather than returned alone: the widget is a
/// separate window from the Settings tab that changed this, and it has to
/// redraw without being asked.
#[tauri::command]
pub fn set_account_hidden(
    app: tauri::AppHandle,
    state: tauri::State<'_, VisibilityState>,
    surface: String,
    id: String,
    hidden: bool,
) -> Result<AccountVisibility, String> {
    let snapshot = {
        let mut current = state.get();
        let list = current
            .list_mut(&surface)
            .ok_or_else(|| format!("unknown surface: {surface}"))?;
        // Nothing changed, so there is nothing to write or announce; returning
        // early keeps a double-click off the disk.
        if !toggle(list, &id, hidden) {
            return Ok(current.clone());
        }
        current.clone()
    };
    persist(&snapshot)?;
    use tauri::Emitter;
    let _ = app.emit("account-visibility", &snapshot);
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hiding_is_idempotent_and_reversible() {
        let mut list = Vec::new();
        assert!(toggle(&mut list, "codex", true));
        assert!(!toggle(&mut list, "codex", true));
        assert_eq!(list, vec!["codex".to_string()]);
        assert!(toggle(&mut list, "codex", false));
        assert!(!toggle(&mut list, "codex", false));
        assert!(list.is_empty());
    }

    #[test]
    fn surfaces_are_independent() {
        let path = r"claude:C:\Users\me\.claude";
        let mut state = AccountVisibility::default();
        toggle(state.list_mut("widget").unwrap(), path, true);
        assert!(state.panel_hidden.is_empty());
        assert_eq!(state.widget_hidden, vec![path.to_string()]);
        assert!(state.list_mut("nowhere").is_none());
    }

    /// A file written before either field existed still loads, and leaves both
    /// surfaces showing everything rather than failing to parse.
    #[test]
    fn missing_fields_load_as_nothing_hidden() {
        let state: AccountVisibility = serde_json::from_str("{}").unwrap();
        assert!(state.panel_hidden.is_empty());
        assert!(state.widget_hidden.is_empty());
    }
}
