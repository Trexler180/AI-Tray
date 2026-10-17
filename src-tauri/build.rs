/// Every command registered in `generate_handler!` in `src/lib.rs`.
///
/// Listing them gives each one an `allow-<command>` permission, and — because
/// the app then has an ACL of its own — a window can only call the commands
/// its file in `capabilities/` grants. Without this every window could call
/// every command, so the taskbar widget could spend resets or install updates.
///
/// A command left off this list is refused to every window. Add new commands
/// here and to the capability of each window that calls them;
/// `tests/ipc_permissions.test.mjs` checks the three agree.
const COMMANDS: &[&str] = &[
    "get_usage",
    "get_cached_usage",
    "fit_window_height",
    "glass_enabled",
    "get_notification_settings",
    "set_notification_enabled",
    "set_notification_options",
    "set_claude_account_label",
    "add_claude_directory",
    "remove_claude_directory",
    "consume_codex_reset",
    "consume_claude_reset",
    "clear_history_cache",
    "get_window_history",
    "get_recent_burn",
    "clear_window_history",
    "set_panel_expanded",
    "get_update_state",
    "set_update_setting",
    "check_for_updates_now",
    "install_update",
    "open_panel",
    "get_start_on_login",
    "set_start_on_login",
    "get_tray_settings",
    "set_tray_icon_mode",
    "get_account_visibility",
    "set_account_hidden",
    "get_widget_settings",
    "get_widget_placement",
    "get_widget_edges",
    "list_widget_monitors",
    "set_widget_monitor",
    "set_widget_enabled",
    "set_widget_width",
    "set_widget_gap",
    "set_widget_option",
    "set_widget_account_row",
    "set_recent_minutes",
    "reset_widget_position",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
