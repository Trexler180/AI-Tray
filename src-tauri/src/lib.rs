mod accounts;
mod alerts;
mod auth;
mod claude;
mod codex;
mod history_cache;
mod live;
mod models;
mod pricing;
mod resets;
mod updates;
mod util;
mod widget;
mod windows_history;

use alerts::{AlertState, NotificationSettings};
use updates::{UpdateSettings, UpdateSnapshot, UpdateState};
use models::{ClaudeAccountUsage, Usage};
use windows_history::{RecentBurn, WindowHistory};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, LogicalSize, Manager, PhysicalPosition, Size, WindowEvent,
};

/// Timestamp of the last auto-hide caused by losing focus. Used to tell
/// "tray click while the panel was open" (the click steals focus, the panel
/// hides, and the click should NOT immediately re-show it) from a normal open.
struct HideGuard(Mutex<Option<Instant>>);

/// Whether the native acrylic effect was applied, so the frontend knows to use
/// a translucent background (true) or its solid fallback (false).
struct Glass(AtomicBool);

/// Set when the user picks Quit, so the run loop lets that exit through instead
/// of preventing it the way it does for an ordinary window close (which keeps
/// the app alive in the tray).
struct Quitting(AtomicBool);

/// True while the timeline is showing its wide layout. The panel is a popover
/// everywhere else, so this flag is what suspends the popover behaviours:
/// auto-hide on blur and the height auto-fit that owns the compact width.
struct Expanded(AtomicBool);

#[tauri::command]
fn glass_enabled(state: tauri::State<'_, Glass>) -> bool {
    state.0.load(Ordering::Relaxed)
}

/// Compact popover width. Also the width the panel returns to when the
/// timeline collapses.
const WIDTH: f64 = 380.0;
/// Preferred size of the expanded timeline, before the work area clamps it.
const EXPANDED_WIDTH: f64 = 940.0;
const EXPANDED_HEIGHT: f64 = 580.0;

#[tauri::command]
fn fit_window_height(
    window: tauri::WebviewWindow,
    expanded: tauri::State<'_, Expanded>,
    height: f64,
) {
    const MIN_HEIGHT: f64 = 420.0;
    const MAX_HEIGHT: f64 = 640.0;

    // The expanded timeline sets its own size; letting the content fit run
    // would immediately snap the panel back to popover width.
    if expanded.0.load(Ordering::Relaxed) {
        return;
    }

    let next_height = height.clamp(MIN_HEIGHT, MAX_HEIGHT).round();
    let scale = window.scale_factor().unwrap_or(1.0);
    let old_size = window.outer_size().ok();
    let old_pos = window.outer_position().ok();
    let old_height = old_size.map(|s| s.height as f64 / scale);

    let _ = window.set_size(Size::Logical(LogicalSize::new(WIDTH, next_height)));

    if let (Some(pos), Some(old_height)) = (old_pos, old_height) {
        let delta = ((old_height - next_height) * scale).round() as i32;
        if delta != 0 {
            let _ = window.set_position(PhysicalPosition::new(pos.x, pos.y + delta));
        }
    }
}

/// Grow the panel to the timeline's wide layout, or put it back. The expanded
/// size is clamped to the monitor's work area, and `position_window` then pulls
/// the wider panel back inside it — a popover anchored near the tray would
/// otherwise hang off the screen edge once it triples in width.
#[tauri::command]
fn set_panel_expanded(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Expanded>,
    expanded: bool,
) {
    if state.0.swap(expanded, Ordering::Relaxed) == expanded {
        return;
    }
    if !expanded {
        // Height is left to the frontend's next fit_window_height call, which
        // knows how tall the compact content actually is.
        let _ = window.set_size(Size::Logical(LogicalSize::new(WIDTH, EXPANDED_HEIGHT)));
        position_window(&window, None);
        return;
    }

    let (mut width, mut height) = (EXPANDED_WIDTH, EXPANDED_HEIGHT);
    if let Ok(Some(monitor)) = window.current_monitor() {
        let scale = monitor.scale_factor();
        let area = monitor.work_area();
        width = width.min(area.size.width as f64 / scale - 24.0);
        height = height.min(area.size.height as f64 / scale - 24.0);
    }
    let _ = window.set_size(Size::Logical(LogicalSize::new(width, height)));
    position_window(&window, None);
}

/// Drop the expanded layout when the panel goes away, so the next tray click
/// opens the familiar popover rather than a full-width window.
fn collapse_panel(app: &tauri::AppHandle) {
    if !app.state::<Expanded>().0.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_size(Size::Logical(LogicalSize::new(WIDTH, EXPANDED_HEIGHT)));
        let _ = window.emit("collapse", ());
    }
}

/// Windows 11 rounds decorated windows automatically but leaves undecorated
/// ones square, so the acrylic backdrop would poke out past the panel's CSS
/// corner radius. Ask DWM to round the native window too. No-op on Windows 10.
#[cfg(windows)]
fn round_native_corners(window: &tauri::WebviewWindow) {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    if let Ok(hwnd) = window.hwnd() {
        let pref = DWMWCP_ROUND;
        unsafe {
            DwmSetWindowAttribute(
                hwnd.0 as _,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &pref as *const _ as *const core::ffi::c_void,
                std::mem::size_of_val(&pref) as u32,
            );
        }
    }
}

#[cfg(not(windows))]
fn round_native_corners(_window: &tauri::WebviewWindow) {}

/// Toast notifications on Windows require the sender's AppUserModelID to be
/// registered. The installer normally does this via a Start Menu shortcut,
/// but a portable or dev-built exe has no such registration, so every toast
/// would be silently dropped. Registering the AUMID under
/// HKCU\Software\Classes\AppUserModelId is the documented installer-free way.
#[cfg(windows)]
fn register_notification_aumid(identifier: &str, display_name: &str) {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, REG_SZ,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let key_path = wide(&format!("Software\\Classes\\AppUserModelId\\{identifier}"));
    let value_name = wide("DisplayName");
    let value = wide(display_name);
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegCreateKeyW(HKEY_CURRENT_USER, key_path.as_ptr(), &mut key) == 0 {
            RegSetValueExW(
                key,
                value_name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr() as *const u8,
                (value.len() * 2) as u32,
            );
            RegCloseKey(key);
        }
    }
}

#[cfg(not(windows))]
fn register_notification_aumid(_identifier: &str, _display_name: &str) {}

/// Gather all usage data. Heavy file IO and the network calls run off the UI
/// thread, and independently of each other so one slow source can't stall the
/// rest.
pub(crate) fn collect_usage_sync() -> Usage {
    let (mut codex, mut claude, codex_live, claude_accounts, codex_resets) =
        std::thread::scope(|s| {
            let a = s.spawn(codex::collect);
            let b = s.spawn(claude::collect);
            let c = s.spawn(live::codex_live);
            let d = s.spawn(live::claude_live_accounts);
            let e = s.spawn(resets::codex_resets);
            (
                a.join().unwrap_or_default(),
                b.join().unwrap_or_default(),
                c.join().unwrap_or_default(),
                d.join().unwrap_or_default(),
                e.join().ok().flatten(),
            )
        });

    // Live gauges come straight from the official usage endpoints and
    // override the log-based estimates when reachable.
    let (codex_live, codex_live_health) = codex_live;
    codex.health = codex_live_health;
    if let Some(l) = codex_live {
        codex.available = true;
        codex.live = true;
        if l.plan_type.is_some() {
            codex.plan_type = l.plan_type;
        }
        // Replaced wholesale, gaps included: which windows Codex enforces
        // changes over time, and a log-derived window the endpoint no longer
        // reports is a limit that no longer exists.
        codex.primary = l.primary;
        codex.secondary = l.secondary;
        codex.quotas = l.quotas;
    }
    // Reset credits come from a separate live endpoint. Having reachable credit
    // data also means a Codex session exists, so surface the tab even if the
    // log scan and usage call both came up empty.
    if let Some(r) = codex_resets {
        if r.available {
            codex.available = true;
        }
        codex.resets = Some(r);
    }
    // Live Claude gauges are per-account. The active account also fills the
    // top-level gauges, so the alert windows and any single-account view keep
    // reading `claude.five_hour`/`seven_day` unchanged. Cost/token history
    // stays machine-wide on `claude` since the logs carry no account id.
    if !claude_accounts.is_empty() {
        // Having any configured account makes the Claude tab (and its folder
        // management UI) available even before a live fetch succeeds.
        claude.available = true;
        if claude_accounts.iter().any(|a| a.live) {
            claude.live = true;
        }
        if let Some(active) = claude_accounts.iter().find(|a| a.active) {
            claude.health = active.health.clone();
        }
        if let Some(active) = claude_accounts.iter().find(|a| a.active && a.live) {
            claude.five_hour = active.five_hour.clone();
            claude.seven_day = active.seven_day.clone();
            claude.seven_day_model = active.seven_day_model.clone();
            claude.quotas = active.quotas.clone();
        }
        claude.accounts = claude_accounts
            .into_iter()
            .map(|a| ClaudeAccountUsage {
                id: a.id,
                label: a.label,
                subscription_type: a.subscription_type,
                active: a.active,
                removable: a.removable,
                live: a.live,
                five_hour: a.five_hour,
                seven_day: a.seven_day,
                seven_day_model: a.seven_day_model,
                quotas: a.quotas,
                extra_usage: a.extra_usage,
                health: a.health,
            })
            .collect();
    }

    Usage {
        codex,
        claude,
        generated_at: chrono::Utc::now().timestamp(),
        error: None,
    }
}

#[tauri::command]
async fn get_usage(
    app: tauri::AppHandle,
    alerts: tauri::State<'_, AlertState>,
) -> Result<Usage, String> {
    let generation = alerts.next_generation();
    let usage = tauri::async_runtime::spawn_blocking(collect_usage_sync)
        .await
        .map_err(|e| e.to_string())?;
    alerts.observe_usage(&app, &usage, generation);
    Ok(usage)
}

/// Last snapshot collected by any prior refresh (watcher, ticker, or panel).
/// Returns instantly so the UI can paint while a fresh collection runs.
#[tauri::command]
fn get_cached_usage(alerts: tauri::State<'_, AlertState>) -> Option<Usage> {
    alerts.latest_usage()
}

#[tauri::command]
fn get_notification_settings(alerts: tauri::State<'_, AlertState>) -> NotificationSettings {
    alerts.settings()
}

#[tauri::command]
fn set_notification_enabled(
    app: tauri::AppHandle,
    alerts: tauri::State<'_, AlertState>,
    provider: String,
    enabled: bool,
) -> Result<NotificationSettings, String> {
    let settings = alerts.set_provider(&provider, enabled)?;
    if enabled {
        if let Some(usage) = alerts.latest_usage() {
            let generation = alerts.next_generation();
            alerts.observe_usage(&app, &usage, generation);
        } else {
            alerts::refresh_for_alerts(app);
        }
    }
    Ok(settings)
}

/// Consume (redeem) a Codex reset credit. This spends the credit, so it is only
/// invoked from an explicit user action in the panel. Refreshes the panel after
/// so the now-spent credit disappears.
#[tauri::command]
async fn consume_codex_reset(app: tauri::AppHandle, credit_id: String) -> Result<(), String> {
    let id = credit_id.clone();
    tauri::async_runtime::spawn_blocking(move || resets::consume_codex_reset(&id))
        .await
        .map_err(|e| e.to_string())??;
    alerts::refresh_for_alerts(app);
    Ok(())
}

/// Rename a Claude account. An empty label clears back to the derived default.
#[tauri::command]
fn set_claude_account_label(id: String, label: String) -> Result<(), String> {
    accounts::set_label(&id, &label)
}

/// Register an extra Claude config directory (a folder holding its own
/// `.credentials.json`) so it shows up as its own account.
#[tauri::command]
fn add_claude_directory(path: String) -> Result<(), String> {
    accounts::add_dir(&path)
}

/// Stop tracking a user-added Claude directory. The built-in `~/.claude` can't
/// be removed.
#[tauri::command]
fn remove_claude_directory(id: String) -> Result<(), String> {
    accounts::remove_dir(&id)
}

/// Current updater status, app version, and the two update toggles.
#[tauri::command]
fn get_update_state(app: tauri::AppHandle, state: tauri::State<'_, UpdateState>) -> UpdateSnapshot {
    state.snapshot(app.package_info().version.to_string())
}

#[tauri::command]
fn set_update_setting(
    state: tauri::State<'_, UpdateState>,
    key: String,
    enabled: bool,
) -> Result<UpdateSettings, String> {
    state.set_flag(&key, enabled)
}

/// User-initiated check. Reports "up to date" explicitly, unlike the silent
/// background poll.
#[tauri::command]
async fn check_for_updates_now(app: tauri::AppHandle) {
    updates::check(&app, true).await;
}

#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    updates::install(&app).await
}

/// Clear only the derived history indexes. Source logs and credentials are
/// untouched; the next refresh rebuilds both indexes from local history.
#[tauri::command]
fn clear_history_cache() -> Result<(), String> {
    codex::clear_history_cache().map_err(|error| error.to_string())?;
    claude::clear_history_cache().map_err(|error| error.to_string())
}

/// Every quota window the app has recorded, for the timeline screen.
#[tauri::command]
fn get_window_history() -> WindowHistory {
    windows_history::history()
}

/// Quota spent on each window over the last `minutes`, for the meters' recent
/// band. Both the panel and the taskbar widget ask for this rather than reading
/// the samples themselves: the widget would otherwise need the whole record
/// shipped to it every refresh just to derive one number per bar.
#[tauri::command]
fn get_recent_burn(minutes: i64) -> std::collections::HashMap<String, RecentBurn> {
    windows_history::recent(minutes)
}

/// Forget the recorded windows. Unlike the log indexes this can't be rebuilt —
/// the providers only report the window that is live now — so the timeline
/// starts over from this moment.
#[tauri::command]
fn clear_window_history() -> Result<(), String> {
    windows_history::clear().map_err(|error| error.to_string())
}

/// Place the popover near the tray click, or at the bottom-right of the
/// current monitor when no click position is known. The panel is always
/// clamped into the monitor's work area (the screen minus the taskbar), so a
/// click on a pinned tray icon makes it rest on the taskbar edge instead of
/// covering it — and a top/side taskbar is handled by the same clamp.
fn position_window(window: &tauri::WebviewWindow, click: Option<PhysicalPosition<f64>>) {
    let size = match window.outer_size() {
        Ok(s) => s,
        Err(_) => return,
    };
    let monitor = click
        .and_then(|p| window.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| window.current_monitor().ok().flatten());
    let Some(monitor) = monitor else { return };

    let wa = monitor.work_area();
    let (wax, way) = (wa.position.x, wa.position.y);
    let (waw, wah) = (wa.size.width as i32, wa.size.height as i32);
    let margin = 12i32;
    let (w, h) = (size.width as i32, size.height as i32);

    let (mut x, mut y) = match click {
        // Centered above the tray icon.
        Some(p) => (p.x as i32 - w / 2, p.y as i32 - h - margin),
        None => (wax + waw - w - margin, way + wah - h - margin),
    };

    // Clamp into the work area; never overlap the taskbar.
    x = x.clamp(wax, (wax + waw - w).max(wax));
    y = y.clamp(way, (way + wah - h).max(way));
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

fn toggle_window(app: &tauri::AppHandle, click: Option<PhysicalPosition<f64>>) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        collapse_panel(app);
        let _ = window.hide();
        return;
    }
    // If this same click just blurred (and therefore hid) the panel, the user
    // meant "close" — don't flip it straight back open.
    if let Some(t) = *app.state::<HideGuard>().0.lock().unwrap() {
        if t.elapsed() < Duration::from_millis(300) {
            return;
        }
    }
    position_window(&window, click);
    let _ = window.show();
    let _ = window.set_focus();
    // Fresh numbers every time the panel opens.
    let _ = window.emit("refresh", ());
}

/// Toggle the popover from the taskbar widget — the same behaviour as clicking
/// the tray icon, including the `HideGuard` check.
///
/// That guard matters here even though the widget carries `WS_EX_NOACTIVATE`:
/// the widget hosts a WebView2, whose child window can take focus on click. If
/// it does, the panel blurs and hides itself first, and without the guard this
/// would immediately show it again — leaving a panel that can't be closed by
/// clicking the widget.
#[tauri::command]
fn open_panel(app: tauri::AppHandle) {
    toggle_window(&app, None);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // A second launch just pops the existing panel.
            toggle_window(app, None);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(AlertState::load())
        .manage(UpdateState::load())
        .manage(HideGuard(Mutex::new(None)))
        .manage(Glass(AtomicBool::new(false)))
        .manage(Quitting(AtomicBool::new(false)))
        .manage(Expanded(AtomicBool::new(false)))
        .manage(widget::WidgetState::load())
        .invoke_handler(tauri::generate_handler![
            get_usage,
            get_cached_usage,
            fit_window_height,
            glass_enabled,
            get_notification_settings,
            set_notification_enabled,
            set_claude_account_label,
            add_claude_directory,
            remove_claude_directory,
            consume_codex_reset,
            clear_history_cache,
            get_window_history,
            get_recent_burn,
            clear_window_history,
            set_panel_expanded,
            get_update_state,
            set_update_setting,
            check_for_updates_now,
            install_update,
            open_panel,
            widget::get_widget_settings,
            widget::get_widget_placement,
            widget::get_widget_edges,
            widget::list_widget_monitors,
            widget::set_widget_monitor,
            widget::set_widget_enabled,
            widget::set_widget_width,
            widget::set_widget_gap,
            widget::set_widget_option,
            widget::set_recent_minutes,
            widget::reset_widget_position
        ])
        .setup(|app| {
            // Registered here rather than in the builder chain because the
            // updater is desktop-only and this keeps the cfg to one line.
            #[cfg(desktop)]
            app.handle()
                .plugin(tauri_plugin_updater::Builder::new().build())?;

            register_notification_aumid(
                &app.config().identifier,
                app.config()
                    .product_name
                    .as_deref()
                    .unwrap_or("AI Usage Tray"),
            );

            // Register this exe to start on login (keeps the registry entry
            // pointed at wherever the app currently lives).
            {
                use tauri_plugin_autostart::ManagerExt;
                let _ = app.autolaunch().enable();
            }

            let handle = app.handle().clone();
            let _ = alerts::start_usage_watcher(&handle);
            alerts::start_alert_ticker(handle.clone());
            alerts::refresh_for_alerts(handle);

            updates::start_update_ticker(app.handle().clone());

            // Right-click menu: manual refresh + update check + quit.
            let refresh = MenuItem::with_id(app, "refresh", "Refresh", true, None::<&str>)?;
            let check_updates = MenuItem::with_id(
                app,
                "check_updates",
                "Check for updates…",
                true,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&refresh, &check_updates, &quit])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().unwrap())
                .tooltip("AI Usage")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => {
                        app.state::<Quitting>().0.store(true, Ordering::Relaxed);
                        app.exit(0);
                    }
                    "refresh" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.emit("refresh", ());
                        }
                    }
                    "check_updates" => {
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            updates::check(&handle, true).await;
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        position,
                        ..
                    } = event
                    {
                        toggle_window(tray.app_handle(), Some(position));
                    }
                })
                .build(app)?;

            // Hide when the popover loses focus, like a native menu-bar panel.
            if let Some(window) = app.get_webview_window("main") {
                // Glassy backdrop: acrylic blur with a dark tint. If the OS
                // refuses, the frontend keeps its solid background instead.
                let glassy = window
                    .set_effects(
                        tauri::window::EffectsBuilder::new()
                            .effect(tauri::window::Effect::Acrylic)
                            .color(tauri::window::Color(16, 19, 26, 120))
                            .build(),
                    )
                    .is_ok();
                app.state::<Glass>().0.store(glassy, Ordering::Relaxed);
                if glassy {
                    round_native_corners(&window);
                }

                let w = window.clone();
                let handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::Focused(false) = event {
                        // The expanded timeline is meant to be read alongside
                        // other windows, so it stays put until it's closed.
                        if handle.state::<Expanded>().0.load(Ordering::Relaxed) {
                            return;
                        }
                        *handle.state::<HideGuard>().0.lock().unwrap() = Some(Instant::now());
                        let _ = w.hide();
                    }
                });
            }

            // The taskbar widget: shown only if the user switched it on, and
            // kept on the strip by its own ticker.
            if let Some(window) = app.get_webview_window("widget") {
                window.on_window_event(move |event| {
                    // The hook's callback holds the HWND in a static; clear it
                    // rather than leave it pointing at a handle Windows is free
                    // to hand to someone else. Position is not tracked here —
                    // the widget drives its own moves, so nothing else can put
                    // it somewhere it doesn't belong.
                    if let WindowEvent::Destroyed = event {
                        widget::forget_window();
                    }
                });
            }
            widget::apply(app.handle());
            // Installed on the main thread: the hook is out-of-context, so its
            // callback arrives through this thread's message queue.
            widget::install_foreground_hook();
            widget::start_ticker(app.handle().clone());

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app, event| {
            // Keep running in the tray when the window is closed, but let an
            // explicit Quit (which sets this flag) actually exit.
            match event {
                tauri::RunEvent::ExitRequested { api, .. } => {
                    if !app.state::<Quitting>().0.load(Ordering::Relaxed) {
                        api.prevent_exit();
                    }
                }
                // Hand the system-wide foreground hook back before going away.
                tauri::RunEvent::Exit => widget::uninstall_foreground_hook(),
                _ => {}
            }
        });
}
