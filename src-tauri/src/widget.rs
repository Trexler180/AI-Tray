//! The taskbar widget: a small always-on-top window parked inside the taskbar
//! strip so it reads as if it were docked there.
//!
//! Windows exposes no way for a third-party app to add a real taskbar button —
//! the notification area is the only sanctioned spot, and that is one 16 px
//! icon. So this floats: a borderless, never-focused window positioned over the
//! bar. Everything here is in service of making that float behave like a dock —
//! staying on the strip through resolution and DPI changes, keeping its z-order
//! against a taskbar that is itself topmost, and getting out of the way of
//! fullscreen windows.

use crate::util::config_dir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{LogicalSize, Manager, PhysicalPosition, Size};

/// Logical width the widget starts at. Account count never changes it — more
/// accounts narrow the cells instead — but the user can drag it wider when the
/// names deserve the room.
const DEFAULT_WIDTH: f64 = 114.0;
/// Narrow enough that two cells still read; wide enough to be silly. Both are
/// guard rails against a drag that gets away from you, not design opinions.
const MIN_WIDTH: f64 = 72.0;
const MAX_WIDTH: f64 = 420.0;
/// Logical gap between the widget's right edge and the notification area.
const DEFAULT_TRAY_GAP: f64 = 10.0;
/// Only used when the notification area can't be found: a fixed inset from the
/// right edge of the bar, roughly the width of the clock plus a few icons.
const FALLBACK_INSET: f64 = 220.0;
/// Default reach of the recent-usage band. An hour is long enough that the
/// coarse percentages the providers report can resolve it, short enough to
/// still mean "lately".
const DEFAULT_RECENT_MINUTES: u32 = 60;
/// Range the band's reach is held to. The picker only offers a few values; this
/// is a guard on the stored file, not the choice itself.
const RECENT_MINUTES_RANGE: std::ops::RangeInclusive<u32> = 5..=1440;

#[derive(Serialize, Deserialize, Clone)]
pub struct WidgetSettings {
    /// Off until asked for: this is an always-on-screen element.
    #[serde(default)]
    pub enabled: bool,
    /// Logical pixels between the widget and the notification area. Measured
    /// from the tray rather than the screen edge so it holds still as icons
    /// come and go. Updated when the user drags the widget.
    #[serde(default = "default_gap")]
    pub tray_gap: f64,
    /// Logical width, set by dragging the widget's left edge.
    #[serde(default = "default_width")]
    pub width: f64,
    /// Draw the pace mark — how far through each window the clock is.
    #[serde(default = "yes")]
    pub show_pace: bool,
    /// Draw the dim weekly bar under each session bar. With it off, the session
    /// bar fills the row on its own.
    #[serde(default = "yes")]
    pub show_weekly: bool,
    /// Draw the recent-usage band — what the session window burned over the
    /// last `recent_minutes`. Off by default: it is an extra mark on a bar a few
    /// pixels tall, so it is worth asking for rather than arriving unannounced.
    #[serde(default)]
    pub show_recent: bool,
    /// How far back that band reaches. Shared with the panel's meters, which is
    /// why it lives here rather than in the panel's own display preferences:
    /// this is the only store both windows can read and be notified about.
    #[serde(default = "default_recent_minutes")]
    pub recent_minutes: u32,
    /// Which display to sit on, as the OS device name (`\\.\DISPLAY2`). None
    /// means "wherever the window opens", which is normally the primary.
    ///
    /// Needed because the drag is confined to one taskbar strip — that is what
    /// stops the widget being dropped on the desktop — so it can't be carried
    /// across screens by hand. This is the explicit way to say which screen.
    #[serde(default)]
    pub monitor: Option<String>,
}

fn yes() -> bool {
    true
}

fn default_gap() -> f64 {
    DEFAULT_TRAY_GAP
}

fn default_width() -> f64 {
    DEFAULT_WIDTH
}

fn default_recent_minutes() -> u32 {
    DEFAULT_RECENT_MINUTES
}

impl Default for WidgetSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            tray_gap: DEFAULT_TRAY_GAP,
            width: DEFAULT_WIDTH,
            show_pace: true,
            show_weekly: true,
            show_recent: false,
            recent_minutes: DEFAULT_RECENT_MINUTES,
            monitor: None,
        }
    }
}

pub struct WidgetState {
    settings: Mutex<WidgetSettings>,
    /// What `position` decided last time it ran. The Settings note reads this
    /// rather than working it out again: a second, independent calculation can
    /// disagree with the one that actually placed the window — and did, telling
    /// people there was no taskbar while the widget sat on it.
    placement: Mutex<Placement>,
}

impl WidgetState {
    pub fn load() -> Self {
        Self {
            settings: Mutex::new(load_settings()),
            placement: Mutex::new(Placement::Ok),
        }
    }

    fn placement(&self) -> std::sync::MutexGuard<'_, Placement> {
        self.placement.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A poisoned lock here means some other thread panicked mid-update; the
    /// settings are still readable and losing the widget entirely is a worse
    /// outcome than continuing with them.
    fn get(&self) -> std::sync::MutexGuard<'_, WidgetSettings> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn settings_path() -> Option<PathBuf> {
    let mut root = config_dir()?;
    root.push("widget.json");
    Some(root)
}

fn load_settings() -> WidgetSettings {
    let Some(path) = settings_path() else {
        return WidgetSettings::default();
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

fn persist(settings: &WidgetSettings) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "settings directory unavailable".to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    crate::util::write_atomic(&path, body.as_bytes()).map_err(|e| e.to_string())
}

/// Left edge of the notification area — the chevron, the tray icons and the
/// clock. The widget parks to the left of this rather than guessing a fixed
/// inset, because how wide that block is depends on how many icons are showing.
///
/// `Shell_TrayWnd` itself is no use for this: Windows 11 reports it 24 px taller
/// than the bar it paints, so it says nothing reliable about where things sit.
/// Its `TrayNotifyWnd` child, however, is exactly the icons-and-clock block.
#[cfg(windows)]
fn tray_notify_left() -> Option<i32> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, FindWindowW, GetWindowRect,
    };
    let tray: Vec<u16> = "Shell_TrayWnd\0".encode_utf16().collect();
    let notify: Vec<u16> = "TrayNotifyWnd\0".encode_utf16().collect();
    unsafe {
        let bar = FindWindowW(tray.as_ptr(), std::ptr::null());
        if bar.is_null() {
            return None;
        }
        let child = FindWindowExW(bar, std::ptr::null_mut(), notify.as_ptr(), std::ptr::null());
        if child.is_null() {
            return None;
        }
        let mut r = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(child, &mut r) == 0 || r.right <= r.left {
            return None;
        }
        Some(r.left)
    }
}

#[cfg(not(windows))]
fn tray_notify_left() -> Option<i32> {
    None
}

/// A rect as (x, y, width, height) in physical pixels.
type Rect = (i32, i32, i32, i32);

/// Where the taskbar sits, and which way it runs. The widget lays its accounts
/// out in two rows across a wide strip, so a vertical bar is not somewhere it
/// can go — the orientation is carried here so the caller can decline rather
/// than stretch to fit.
#[derive(Debug, PartialEq, Eq)]
pub struct Strip {
    pub rect: Rect,
    pub horizontal: bool,
}

/// The strip the work area doesn't cover, derived purely from two rects so it
/// can be tested without a `tauri::Monitor` (which can't be constructed).
/// `None` when the work area fills the monitor — an auto-hidden bar, or a
/// screen that simply has no taskbar.
pub fn strip_from(full: Rect, work: Rect) -> Option<Strip> {
    let (fx, fy, fw, fh) = full;
    let (wx, wy, ww, wh) = work;

    // Bottom bar is by far the common case, so it is checked first.
    if wy + wh < fy + fh {
        return Some(Strip {
            rect: (fx, wy + wh, fw, (fy + fh) - (wy + wh)),
            horizontal: true,
        });
    }
    if wy > fy {
        return Some(Strip {
            rect: (fx, fy, fw, wy - fy),
            horizontal: true,
        });
    }
    if wx > fx {
        return Some(Strip {
            rect: (fx, fy, wx - fx, fh),
            horizontal: false,
        });
    }
    if wx + ww < fx + fw {
        return Some(Strip {
            rect: (wx + ww, fy, (fx + fw) - (wx + ww), fh),
            horizontal: false,
        });
    }
    None
}

fn taskbar_strip(monitor: &tauri::Monitor) -> Option<Strip> {
    let full = monitor.size();
    let pos = monitor.position();
    let wa = monitor.work_area();
    strip_from(
        (pos.x, pos.y, full.width as i32, full.height as i32),
        (
            wa.position.x,
            wa.position.y,
            wa.size.width as i32,
            wa.size.height as i32,
        ),
    )
}

/// The display the widget should sit on: the chosen one when it is still
/// attached, otherwise wherever the window currently is, otherwise the primary.
///
/// Falling back rather than hiding matters — a laptop undocked from the screen
/// the widget was pinned to should get its widget back on the built-in display,
/// not lose it until it notices the setting.
/// A stable key for a display. Prefers the OS device name, and falls back to
/// the monitor's origin — an unnamed monitor must still be selectable, and
/// dropping it from the list looks identical to owning one screen.
fn monitor_key(m: &tauri::Monitor) -> String {
    m.name().cloned().unwrap_or_else(|| {
        let p = m.position();
        format!("@{},{}", p.x, p.y)
    })
}

fn widget_monitor(window: &tauri::WebviewWindow, preferred: Option<&str>) -> Option<tauri::Monitor> {
    if let Some(name) = preferred {
        if let Ok(monitors) = window.available_monitors() {
            if let Some(found) = monitors.into_iter().find(|m| monitor_key(m) == name) {
                return Some(found);
            }
        }
    }
    window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten())
}

/// Why the widget can't be placed, for the note in Settings.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    Ok,
    /// A left- or right-docked taskbar. The widget is a wide two-row strip;
    /// there is no sensible way to render it in a 60 px vertical column.
    VerticalTaskbar,
    /// No measurable strip — an auto-hidden bar, or no taskbar on this screen.
    NoTaskbar,
}

/// Size the window to the reserved strip and park it just left of the
/// notification area. Returns why it declined, when it did: the caller hides
/// the window rather than leaving it somewhere wrong.
pub fn position(app: &tauri::AppHandle) -> Placement {
    let result = place(app);
    let changed = {
        let state = app.state::<WidgetState>();
        let mut last = state.placement();
        let changed = *last != result;
        *last = result;
        changed
    };
    // Only on a change: this runs every few seconds and the panel re-renders
    // whenever it hears about it.
    if changed {
        use tauri::Emitter;
        let _ = app.emit("widget-placement", result);
    }
    result
}

/// The decision itself. Split out so `position` is the only thing that records
/// and announces it.
fn place(app: &tauri::AppHandle) -> Placement {
    let Some(window) = app.get_webview_window("widget") else {
        return Placement::NoTaskbar;
    };
    let preferred = app.state::<WidgetState>().get().monitor.clone();
    let Some(monitor) = widget_monitor(&window, preferred.as_deref()) else {
        return Placement::NoTaskbar;
    };
    let scale = monitor.scale_factor();
    // The reserved strip, not the taskbar's window rect: the window is taller
    // than the bar Windows paints, and matching it makes the widget stand proud
    // of the taskbar's top edge.
    //
    // Declining is deliberate in both of the other cases. A vertical bar would
    // otherwise produce a full-monitor-height always-on-top window, and an
    // auto-hidden bar would leave the widget floating over whatever app owns
    // that space — neither is better than not drawing.
    let Some(strip) = taskbar_strip(&monitor) else {
        return Placement::NoTaskbar;
    };
    if !strip.horizontal {
        return Placement::VerticalTaskbar;
    }
    let (sx, sy, sw, sh) = strip.rect;

    // Height follows the bar so the hover highlight lines up with the taskbar's
    // own buttons; a 40 px "small taskbar" gets a 40 px widget.
    let logical_h = (sh as f64 / scale).max(24.0);

    // Right edge of the widget = left edge of the tray block, less the gap.
    // Falling back to a fixed inset only when the tray can't be located.
    let state = app.state::<WidgetState>();
    let (gap, width) = {
        let settings = state.get();
        (settings.tray_gap, settings.width.clamp(MIN_WIDTH, MAX_WIDTH))
    };
    let w_px = (width * scale).round() as i32;
    let gap_px = (gap * scale).round() as i32;
    let anchor = tray_notify_left()
        .filter(|left| *left > sx && *left <= sx + sw)
        .unwrap_or(sx + sw - (FALLBACK_INSET * scale).round() as i32);
    let x = (anchor - gap_px - w_px).clamp(sx, (sx + sw - w_px).max(sx));

    // If the clamp moved it, fold that back into the stored gap. Otherwise a
    // drag that runs past the end of the bar banks distance that has to be
    // given back before the widget starts moving again.
    let effective_gap = (anchor - (x + w_px)) as f64 / scale;
    if (effective_gap - gap).abs() > 0.5 {
        state.get().tray_gap = effective_gap;
    }

    // Only touch the window when something actually moved. This runs on a
    // timer, and set_size/set_position are window messages, not free.
    let h_px = (logical_h * scale).round() as u32;
    if window
        .outer_size()
        .is_ok_and(|s| s.width != w_px as u32 || s.height != h_px)
    {
        let _ = window.set_size(Size::Logical(LogicalSize::new(width, logical_h)));
    }
    if window
        .outer_position()
        .is_ok_and(|p| p.x != x || p.y != sy)
    {
        let _ = window.set_position(PhysicalPosition::new(x, sy));
    }
    Placement::Ok
}


/// `WS_EX_NOACTIVATE` so clicking the widget never pulls focus off whatever the
/// user is typing in, and `WS_EX_TOOLWINDOW` to keep it out of Alt-Tab.
///
/// Must be applied *after* the window is shown: tao rewrites the extended style
/// wholesale when it makes a window visible for the first time, which drops
/// anything set beforehand. The ticker re-applies it for the same reason.
/// Idempotent — it only writes when a bit is actually missing.
#[cfg(windows)]
fn apply_native_styles(window: &tauri::WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW,
    };
    let Ok(hwnd) = window.hwnd() else { return };
    let wanted = (WS_EX_NOACTIVATE as isize) | (WS_EX_TOOLWINDOW as isize);
    unsafe {
        let current = GetWindowLongPtrW(hwnd.0 as _, GWL_EXSTYLE);
        // APPWINDOW and TOOLWINDOW contradict each other; the taskbar button is
        // the one thing this window must never have.
        let next = (current | wanted) & !(WS_EX_APPWINDOW as isize);
        if next != current {
            SetWindowLongPtrW(hwnd.0 as _, GWL_EXSTYLE, next);
        }
    }
}

#[cfg(not(windows))]
fn apply_native_styles(_window: &tauri::WebviewWindow) {}

/// The widget's HWND, for the foreground hook below. The hook callback is a
/// bare `extern "system"` function with nowhere to carry state, so the handle
/// has to reach it through a static.
#[cfg(windows)]
static WIDGET_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// The taskbar is itself a topmost window, so simply being topmost is not
/// enough to stay above it — the position in the topmost band has to be
/// reclaimed. Cheap, and only while the widget is meant to be visible.
#[cfg(windows)]
fn reassert_topmost(window: &tauri::WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else { return };
    WIDGET_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::Relaxed);
    raise(hwnd.0 as isize);
}

#[cfg(windows)]
fn raise(raw: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };
    if raw == 0 {
        return;
    }
    unsafe {
        let hwnd = raw as _;
        if IsWindowVisible(hwnd) == 0 {
            return;
        }
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Something took the foreground — which, when it's the taskbar, is exactly the
/// moment the widget gets buried inside the topmost band. Reclaiming here is
/// both faster than polling (it happens on the event, not up to a tick later)
/// and free when nothing is happening.
#[cfg(windows)]
unsafe extern "system" fn on_foreground_changed(
    _hook: windows_sys::Win32::UI::Accessibility::HWINEVENTHOOK,
    _event: u32,
    _hwnd: windows_sys::Win32::Foundation::HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    raise(WIDGET_HWND.load(std::sync::atomic::Ordering::Relaxed));
}

/// Install the foreground hook. Out-of-context, so nothing is injected into
/// other processes — the callback is delivered to this thread's message queue,
/// which is why it has to be installed on the thread running the event loop.
#[cfg(windows)]
static HOOK: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

#[cfg(windows)]
pub fn install_foreground_hook() {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::UI::Accessibility::SetWinEventHook;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
    };
    if HOOK.load(Ordering::Relaxed) != 0 {
        return;
    }
    unsafe {
        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            std::ptr::null_mut(),
            Some(on_foreground_changed),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        HOOK.store(hook as isize, Ordering::Relaxed);
    }
}

/// Release the hook on the way out. Windows would reclaim it at process exit
/// anyway, but an installed hook is a global resource and handing it back
/// explicitly is the honest thing — it also makes install/uninstall symmetric
/// if the widget ever becomes something that can be torn down and rebuilt.
#[cfg(windows)]
pub fn uninstall_foreground_hook() {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::UI::Accessibility::UnhookWinEvent;
    let hook = HOOK.swap(0, Ordering::Relaxed);
    if hook != 0 {
        unsafe {
            UnhookWinEvent(hook as _);
        }
    }
    forget_window();
}

#[cfg(not(windows))]
fn reassert_topmost(_window: &tauri::WebviewWindow) {}

/// Drop the cached HWND when the window goes away, so the hook can't fire a
/// `SetWindowPos` at a handle the OS has since reused.
#[cfg(windows)]
pub fn forget_window() {
    WIDGET_HWND.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(windows))]
pub fn install_foreground_hook() {}

#[cfg(not(windows))]
pub fn uninstall_foreground_hook() {}

#[cfg(not(windows))]
pub fn forget_window() {}

/// State carried into the window scan below. `extern "system"` callbacks have
/// nowhere to put a closure, so it travels as an LPARAM.
#[cfg(windows)]
struct Scan {
    /// The widget's monitor, in physical pixels.
    rect: Rect,
    own_pid: u32,
    covered: bool,
}

/// True when some window covers the widget's monitor edge to edge.
///
/// Deliberately not "is the foreground window fullscreen": focus and coverage
/// are different questions, and asking the wrong one is wrong in both
/// directions. Alt-tabbing from a fullscreen game to another screen leaves the
/// game covering its own monitor — the widget must stay hidden there even
/// though it is no longer foreground — while a game on one screen must not
/// blank the widget on another.
///
/// Coverage is measured against the *whole* monitor, including the strip the
/// taskbar occupies, which is what separates a fullscreen window from a merely
/// maximised one.
#[cfg(windows)]
unsafe extern "system" fn scan_window(
    hwnd: windows_sys::Win32::Foundation::HWND,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::BOOL {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    };

    let scan = &mut *(lparam as *mut Scan);

    if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
        return 1;
    }
    // Our own windows are the thing being hidden, not something to hide from.
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == scan.own_pid {
        return 1;
    }
    // The desktop and the taskbar cover the screen by definition.
    let mut class = [0u16; 64];
    let n = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    if n > 0 {
        let name = String::from_utf16_lossy(&class[..n as usize]);
        if matches!(
            name.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        ) {
            return 1;
        }
    }
    // UWP keeps invisible ghost windows around at full size; they are cloaked.
    let mut cloaked = 0u32;
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED as u32,
        &mut cloaked as *mut _ as *mut core::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    ) == 0
        && cloaked != 0
    {
        return 1;
    }

    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if GetWindowRect(hwnd, &mut r) == 0 {
        return 1;
    }
    let (mx, my, mw, mh) = scan.rect;
    if r.left <= mx && r.top <= my && r.right >= mx + mw && r.bottom >= my + mh {
        scan.covered = true;
        return 0; // found one; stop enumerating
    }
    1
}

#[cfg(windows)]
fn monitor_is_covered(app: &tauri::AppHandle) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;

    let Some(window) = app.get_webview_window("widget") else {
        return false;
    };
    let preferred = app.state::<WidgetState>().get().monitor.clone();
    let Some(monitor) = widget_monitor(&window, preferred.as_deref()) else {
        return false;
    };
    let pos = monitor.position();
    let size = monitor.size();
    let mut scan = Scan {
        rect: (pos.x, pos.y, size.width as i32, size.height as i32),
        own_pid: std::process::id(),
        covered: false,
    };
    unsafe {
        EnumWindows(Some(scan_window), &mut scan as *mut _ as isize);
    }
    scan.covered
}

#[cfg(not(windows))]
fn monitor_is_covered(_app: &tauri::AppHandle) -> bool {
    false
}

/// Show or hide the widget, and keep the two in step with the stored setting.
pub fn apply(app: &tauri::AppHandle) {
    let enabled = app.state::<WidgetState>().get().enabled;
    let Some(window) = app.get_webview_window("widget") else {
        return;
    };
    // Hidden whenever there is nowhere sensible to put it, not just when it is
    // switched off.
    if enabled && position(app) == Placement::Ok {
        let _ = window.show();
        apply_native_styles(&window);
        reassert_topmost(&window);
    } else {
        let _ = window.hide();
    }
}

/// Reposition and duck under fullscreen apps.
///
/// Z-order is *not* this loop's job — the foreground hook handles that, on the
/// event and for free. What's left are the things Windows doesn't announce: a
/// resolution change, a taskbar that grew an icon, an app going fullscreen. A
/// lazy pass is plenty for those, and it doubles as the safety net for the rare
/// re-order that arrives without a foreground change.
const TICK_SECS: u64 = 3;

pub fn start_ticker(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(TICK_SECS));
        let enabled = app.state::<WidgetState>().get().enabled;
        if !enabled {
            continue;
        }
        let Some(window) = app.get_webview_window("widget") else {
            continue;
        };
        // Checked before placement so a covered screen costs one scan, not a
        // reposition as well.
        if monitor_is_covered(&app) {
            let _ = window.hide();
            continue;
        }
        // A vertical or auto-hidden taskbar means there is nowhere to sit, so
        // the widget stays hidden until that changes.
        if position(&app) != Placement::Ok {
            let _ = window.hide();
            continue;
        }
        if !window.is_visible().unwrap_or(false) {
            let _ = window.show();
        }
        apply_native_styles(&window);
        reassert_topmost(&window);
    });
}

#[tauri::command]
pub fn get_widget_settings(state: tauri::State<'_, WidgetState>) -> WidgetSettings {
    state.get().clone()
}

/// One display the widget could sit on.
#[derive(Serialize)]
pub struct MonitorChoice {
    /// OS device name, the key stored in settings.
    pub name: String,
    /// What Settings shows: "Display 1 · 2048×1152".
    pub label: String,
    pub primary: bool,
    /// False when this screen has no horizontal taskbar to sit on, so Settings
    /// can grey it out instead of letting you pick somewhere nothing appears.
    pub usable: bool,
    pub selected: bool,
}

#[tauri::command]
pub fn list_widget_monitors(
    app: tauri::AppHandle,
    state: tauri::State<'_, WidgetState>,
) -> Vec<MonitorChoice> {
    let Some(window) = app.get_webview_window("widget") else {
        return Vec::new();
    };
    let Ok(monitors) = window.available_monitors() else {
        return Vec::new();
    };
    let chosen = state.get().monitor.clone();
    let primary_name = window
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| monitor_key(&m));

    monitors
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            let name = monitor_key(&m);
            let size = m.size();
            let scale = m.scale_factor();
            let primary = primary_name.as_deref() == Some(name.as_str());
            MonitorChoice {
                label: format!(
                    "Display {} · {}×{}{}",
                    i + 1,
                    (size.width as f64 / scale).round() as i32,
                    (size.height as f64 / scale).round() as i32,
                    if primary { " (primary)" } else { "" }
                ),
                usable: taskbar_strip(&m).is_some_and(|s| s.horizontal),
                selected: chosen.as_deref() == Some(name.as_str()),
                primary,
                name,
            }
        })
        .collect()
}

/// Pin the widget to a display, or pass None to let it follow the window.
#[tauri::command]
pub fn set_widget_monitor(
    app: tauri::AppHandle,
    state: tauri::State<'_, WidgetState>,
    name: Option<String>,
) -> Result<(), String> {
    let snapshot = {
        let mut settings = state.get();
        settings.monitor = name;
        // The gap is measured from the new screen's tray, and carrying the old
        // screen's value over would drop the widget somewhere arbitrary on it.
        settings.tray_gap = DEFAULT_TRAY_GAP;
        settings.clone()
    };
    persist(&snapshot)?;
    apply(&app);
    broadcast(&app, &snapshot);
    Ok(())
}

/// Whether the widget can currently be placed, so Settings can say why it is
/// switched on but not on screen.
/// The placement the widget is actually running with, as recorded by the last
/// `position` call. Not recalculated here — that is what let the note claim
/// there was no taskbar while the widget was sitting on one.
#[tauri::command]
pub fn get_widget_placement(state: tauri::State<'_, WidgetState>) -> Placement {
    *state.placement()
}

/// Tell the widget its settings changed. It renders from these, and it is a
/// separate window from the panel that changed them.
fn broadcast(app: &tauri::AppHandle, settings: &WidgetSettings) {
    use tauri::Emitter;
    let _ = app.emit("widget-settings", settings);
}

/// The drawing options, which only the widget cares about. Kept as one command
/// rather than one per flag so adding the next one is a single match arm.
#[tauri::command]
pub fn set_widget_option(
    app: tauri::AppHandle,
    state: tauri::State<'_, WidgetState>,
    name: String,
    value: bool,
) -> Result<(), String> {
    let snapshot = {
        let mut settings = state.get();
        match name.as_str() {
            "show_pace" => settings.show_pace = value,
            "show_weekly" => settings.show_weekly = value,
            "show_recent" => settings.show_recent = value,
            other => return Err(format!("unknown widget option: {other}")),
        }
        settings.clone()
    };
    persist(&snapshot)?;
    broadcast(&app, &snapshot);
    Ok(())
}

/// How far back the recent-usage band reaches, on both the panel's meters and
/// the widget's. Separate from `set_widget_option` because that one carries a
/// flag; this is the one drawing preference the two windows share.
#[tauri::command]
pub fn set_recent_minutes(
    app: tauri::AppHandle,
    state: tauri::State<'_, WidgetState>,
    minutes: u32,
) -> Result<(), String> {
    let snapshot = {
        let mut settings = state.get();
        settings.recent_minutes = minutes.clamp(
            *RECENT_MINUTES_RANGE.start(),
            *RECENT_MINUTES_RANGE.end(),
        );
        settings.clone()
    };
    persist(&snapshot)?;
    broadcast(&app, &snapshot);
    Ok(())
}

#[tauri::command]
pub fn set_widget_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, WidgetState>,
    enabled: bool,
) -> Result<(), String> {
    let snapshot = {
        let mut settings = state.get();
        settings.enabled = enabled;
        settings.clone()
    };
    persist(&snapshot)?;
    apply(&app);
    Ok(())
}

/// Move the widget along the bar, as a gap from the notification area. The
/// counterpart to `set_widget_width`, and the reason the widget doesn't use
/// `startDragging`: the OS drag loop would happily carry it into the middle of
/// the desktop, and this thing only belongs on the taskbar. Driving the
/// position ourselves means the strip clamp in `position` is the only place it
/// can end up.
#[tauri::command]
pub fn set_widget_gap(
    app: tauri::AppHandle,
    state: tauri::State<'_, WidgetState>,
    gap: f64,
    commit: bool,
) -> Result<(), String> {
    {
        let mut settings = state.get();
        settings.tray_gap = gap.max(0.0);
    }
    position(&app);
    if commit {
        // `position` may have clamped it at the end of the bar, so persist what
        // was actually applied rather than what was asked for.
        let snapshot = state.get().clone();
        persist(&snapshot)?;
        broadcast(&app, &snapshot);
    }
    Ok(())
}

/// Set the width, in logical pixels. Called repeatedly while the left edge is
/// being dragged, so `commit` keeps the settings file out of the drag loop —
/// only the release writes to disk.
#[tauri::command]
pub fn set_widget_width(
    app: tauri::AppHandle,
    state: tauri::State<'_, WidgetState>,
    width: f64,
    commit: bool,
) -> Result<(), String> {
    let snapshot = {
        let mut settings = state.get();
        settings.width = width.clamp(MIN_WIDTH, MAX_WIDTH);
        settings.clone()
    };
    position(&app);
    if commit {
        persist(&snapshot)?;
        broadcast(&app, &snapshot);
    }
    Ok(())
}

/// Put it back where and how it started, for when a drag has left it somewhere
/// unhelpful — or on a monitor that no longer exists.
#[tauri::command]
pub fn reset_widget_position(
    app: tauri::AppHandle,
    state: tauri::State<'_, WidgetState>,
) -> Result<(), String> {
    let snapshot = {
        let mut settings = state.get();
        settings.tray_gap = DEFAULT_TRAY_GAP;
        settings.width = DEFAULT_WIDTH;
        settings.clone()
    };
    persist(&snapshot)?;
    position(&app);
    broadcast(&app, &snapshot);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_is_off_until_asked_for() {
        let settings = WidgetSettings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.tray_gap, DEFAULT_TRAY_GAP);
        assert_eq!(settings.width, DEFAULT_WIDTH);
    }

    /// Settings written before a field existed must keep working: the file on
    /// disk predates `width`, `show_pace` and `show_weekly`.
    #[test]
    fn older_settings_files_take_the_defaults_for_new_fields() {
        let settings: WidgetSettings = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.width, DEFAULT_WIDTH);
        assert!(settings.show_pace);
        assert!(settings.show_weekly);
    }

    const MON: Rect = (0, 0, 2048, 1152);

    #[test]
    fn a_bottom_bar_is_the_strip_under_the_work_area() {
        let strip = strip_from(MON, (0, 0, 2048, 1104)).unwrap();
        assert_eq!(strip.rect, (0, 1104, 2048, 48));
        assert!(strip.horizontal);
    }

    #[test]
    fn a_top_bar_is_the_strip_above_it() {
        let strip = strip_from(MON, (0, 48, 2048, 1104)).unwrap();
        assert_eq!(strip.rect, (0, 0, 2048, 48));
        assert!(strip.horizontal);
    }

    /// The case that produced a full-monitor-height always-on-top window: the
    /// strip is real, but it runs the wrong way for a two-row widget.
    #[test]
    fn side_bars_are_found_but_flagged_vertical() {
        let left = strip_from(MON, (60, 0, 1988, 1152)).unwrap();
        assert_eq!(left.rect, (0, 0, 60, 1152));
        assert!(!left.horizontal);

        let right = strip_from(MON, (0, 0, 1988, 1152)).unwrap();
        assert_eq!(right.rect, (1988, 0, 60, 1152));
        assert!(!right.horizontal);
    }

    #[test]
    fn an_auto_hidden_bar_leaves_no_strip_at_all() {
        assert!(strip_from(MON, MON).is_none());
    }

    /// Monitors to the right of the primary have a non-zero origin, and the
    /// strip has to be reported in the same space.
    #[test]
    fn a_secondary_monitors_strip_keeps_its_origin() {
        let mon = (2048, 0, 1920, 1080);
        let strip = strip_from(mon, (2048, 0, 1920, 1032)).unwrap();
        assert_eq!(strip.rect, (2048, 1032, 1920, 48));
        assert!(strip.horizontal);
    }
}
