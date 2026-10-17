//! The tray icon, its tooltip and its right-click menu, redrawn from every
//! snapshot so the most pressing limit can be read without opening the panel.
//!
//! The icon is a ring showing how much of the tightest window is left, in that
//! provider's colour, turning amber and red at the same thresholds as the
//! panel's meters (see `forecast`). A dot in its corner means something new
//! needs the user — a reset to spend, one about to expire, or a login that
//! stopped working — and opening the panel clears it.

use crate::alerts::{clock_text, gap_text, usage_windows, AlertState, Provider};
use crate::forecast::{self, Tier};
use crate::models::{DataSource, Gauge, Usage};
use crate::util::{config_dir, write_atomic};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::{AppHandle, Emitter, Manager, Wry};

pub const TRAY_ID: &str = "main";
/// What the icon follows when the user hasn't chosen: the tightest window
/// across every account the panel shows.
const MODE_TIGHTEST: &str = "tightest";
/// The plain app icon, never redrawn.
const MODE_LOGO: &str = "logo";
/// Windows truncates tray tooltips at 127 UTF-16 units.
const TOOLTIP_MAX: usize = 127;
const ICON_SIZE: u32 = 32;

#[derive(Clone, Serialize, Deserialize)]
pub struct TraySettings {
    /// "tightest", "logo", or an account id ("codex", "claude:<dir>") to follow
    /// that account's tightest window.
    #[serde(default = "default_icon_mode")]
    pub icon: String,
}

fn default_icon_mode() -> String {
    MODE_TIGHTEST.to_string()
}

impl Default for TraySettings {
    fn default() -> Self {
        Self {
            icon: default_icon_mode(),
        }
    }
}

#[derive(Default)]
struct Attention {
    /// Things in the latest snapshot worth a badge, as stable keys.
    current: HashSet<String>,
    /// What was showing the last time the panel was opened.
    seen: HashSet<String>,
}

pub struct TrayState {
    settings: Mutex<TraySettings>,
    attention: Mutex<Attention>,
    /// The reset the menu's "Use a reset" item would spend, in the panel's
    /// reset-key form ("codex:<credit id>" / "claude:<account dir>").
    reset_key: Mutex<Option<String>>,
}

impl TrayState {
    pub fn load() -> Self {
        Self {
            settings: Mutex::new(load_settings()),
            attention: Mutex::new(Attention::default()),
            reset_key: Mutex::new(None),
        }
    }

    fn settings(&self) -> MutexGuard<'_, TraySettings> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn attention(&self) -> MutexGuard<'_, Attention> {
        self.attention.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn pending_reset(&self) -> Option<String> {
        self.reset_key.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// One live window, as the tray reasons about it.
struct Limit<'a> {
    provider: Provider,
    /// Visibility id: "codex" or "claude:<dir>".
    account_id: String,
    /// The Claude config dir, empty for Codex.
    account_dir: String,
    account_label: String,
    gauge: &'a Gauge,
    tier: Tier,
    runs_out_at: Option<i64>,
}

impl Limit<'_> {
    fn left(&self) -> f64 {
        (100.0 - self.gauge.used_percent).clamp(0.0, 100.0)
    }

    /// "Claude · Personal", or "Codex".
    fn who(&self) -> String {
        match self.provider {
            Provider::Codex => "Codex".to_string(),
            Provider::Claude => format!("Claude · {}", self.account_label),
        }
    }

    /// "5h" / "7d", from the length the provider reports.
    fn span(&self) -> String {
        let minutes = self.gauge.window_minutes;
        if minutes <= 0 {
            String::new()
        } else if minutes < 24 * 60 {
            format!("{}h", (minutes as f64 / 60.0).round())
        } else {
            format!("{}d", (minutes as f64 / 1440.0).round())
        }
    }

    /// The order to worry in: worst tier; among windows heading for empty,
    /// the one that gets there soonest; then least left, then soonest reset.
    /// Same as `tightestLimit` in shared.js.
    fn rank(&self, now: i64) -> (Tier, i64, i64, i64) {
        let runs_out = match self.tier {
            Tier::Crit => -self.runs_out_at.unwrap_or(now),
            _ => 0,
        };
        (
            self.tier,
            runs_out,
            (self.gauge.used_percent * 100.0) as i64,
            -self.gauge.resets_at.unwrap_or(i64::MAX),
        )
    }
}

fn limits<'a>(app: &AppHandle, usage: &'a Usage, now: i64) -> Vec<Limit<'a>> {
    let visibility = app.state::<crate::visibility::VisibilityState>();
    let windows = usage_windows(usage);
    // A spent reserve only matters while the main limit is drained, which is
    // when the panel shows it too.
    let codex_drained = windows.iter().any(|w| {
        w.key.provider == Provider::Codex && w.group != "reserve" && w.gauge.used_percent >= 100.0
    });
    windows
        .into_iter()
        .filter(|w| w.group != "additional" && (w.group != "reserve" || codex_drained))
        .filter_map(|w| {
            let account_id = match w.key.provider {
                Provider::Codex => "codex".to_string(),
                Provider::Claude => format!("claude:{}", w.key.account),
            };
            visibility.in_panel(&account_id).then(|| Limit {
                provider: w.key.provider,
                account_dir: w.key.account.clone(),
                account_id,
                account_label: w.account_label,
                gauge: w.gauge,
                tier: forecast::tier(w.gauge, now),
                runs_out_at: forecast::runs_out_at(w.gauge, now),
            })
        })
        .collect()
}

fn tightest<'a, 'b>(
    limits: impl IntoIterator<Item = &'b Limit<'a>>,
    now: i64,
) -> Option<&'b Limit<'a>>
where
    'a: 'b,
{
    limits.into_iter().max_by_key(|limit| limit.rank(now))
}

/// One line per account, its tightest window: "Personal 14% 5h · out 4:52 pm".
fn tooltip(limits: &[Limit], now: i64) -> String {
    let mut lines = vec!["AI Usage".to_string()];
    let mut accounts: Vec<&str> = Vec::new();
    for limit in limits {
        if !accounts.contains(&limit.account_id.as_str()) {
            accounts.push(&limit.account_id);
        }
    }
    let mut ordered: Vec<&Limit> = accounts
        .iter()
        .filter_map(|id| tightest(limits.iter().filter(|l| l.account_id == *id), now))
        .collect();
    ordered.sort_by_key(|limit| std::cmp::Reverse(limit.rank(now)));
    for limit in ordered {
        let resets = limit.gauge.resets_at.filter(|at| *at > now);
        let line = if limit.tier == Tier::Out {
            match resets {
                Some(at) => format!("{} used up · back {}", limit.account_label, clock_text(at)),
                None => format!("{} used up", limit.account_label),
            }
        } else {
            let tail = match (limit.runs_out_at, resets) {
                (Some(at), _) => format!(" · out {}", clock_text(at)),
                (None, Some(at)) if at - now < 24 * 3600 => format!(" · resets {}", clock_text(at)),
                _ => String::new(),
            };
            format!("{} {:.0}% {}{tail}", limit.account_label, limit.left(), limit.span())
        };
        lines.push(line);
    }
    if lines.len() == 1 {
        lines.push("No live limits".to_string());
    }
    let mut text = String::new();
    for line in lines {
        let candidate = if text.is_empty() { line } else { format!("{text}\n{line}") };
        if candidate.encode_utf16().count() > TOOLTIP_MAX {
            break;
        }
        text = candidate;
    }
    text
}

/// Keys for everything worth a badge in this snapshot.
fn attention_keys(app: &AppHandle, usage: &Usage, now: i64) -> HashSet<String> {
    let visibility = app.state::<crate::visibility::VisibilityState>();
    let mut keys = HashSet::new();
    let actionable = |health: &crate::models::DataHealth| {
        matches!(
            health.error_kind.as_deref(),
            Some("authorization" | "credentials_write")
        ) && health.source != DataSource::LiveApi
    };
    if visibility.in_panel("codex") {
        if let Some(resets) = usage.codex.resets.as_ref().filter(|r| r.available) {
            for credit in resets.credits.iter().filter(|c| c.status == "available") {
                keys.insert(format!("codex-credit:{}", credit.id));
                if credit.expires_at.is_some_and(|at| at - now < 24 * 3600) {
                    keys.insert(format!("codex-expiring:{}", credit.id));
                }
            }
        }
        if actionable(&usage.codex.health) {
            keys.insert("codex-login".to_string());
        }
    }
    for account in &usage.claude.accounts {
        if !visibility.in_panel(&format!("claude:{}", account.id)) {
            continue;
        }
        if let Some(resets) = account.resets.as_ref().filter(|r| r.available_count > 0) {
            keys.insert(format!("claude-reset:{}:{}", account.id, resets.available_count));
            if resets.next_expires_at.is_some_and(|at| at - now < 24 * 3600) {
                keys.insert(format!(
                    "claude-expiring:{}:{}",
                    account.id,
                    resets.next_credit_id.as_deref().unwrap_or("")
                ));
            }
        }
        if actionable(&account.health) {
            keys.insert(format!("claude-login:{}", account.id));
        }
    }
    keys
}

/// The reset the menu offers for `limit`'s account, in the panel's key form,
/// and how many there are.
fn reset_for(usage: &Usage, limit: &Limit) -> Option<(String, u64)> {
    match limit.provider {
        Provider::Codex => {
            let resets = usage.codex.resets.as_ref().filter(|r| r.available)?;
            let credits: Vec<_> = resets.credits.iter().filter(|c| c.status == "available").collect();
            let first = credits.first()?;
            Some((format!("codex:{}", first.id), credits.len() as u64))
        }
        Provider::Claude => {
            let account = usage.claude.accounts.iter().find(|a| a.id == limit.account_dir)?;
            let resets = account.resets.as_ref().filter(|r| r.available_count > 0)?;
            resets.next_credit_id.as_ref()?;
            Some((format!("claude:{}", account.id), resets.available_count))
        }
    }
}

/// Redraw the icon, tooltip and menu from `usage`.
pub fn update(app: &AppHandle, usage: &Usage) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    let limits = limits(app, usage, now);
    let state = app.state::<TrayState>();
    let badge = {
        let mut attention = state.attention();
        attention.current = attention_keys(app, usage, now);
        // Something seen once and gone since shouldn't light the dot again if
        // it comes back unchanged; only genuinely new keys do.
        attention.current.iter().any(|key| !attention.seen.contains(key))
    };

    let mode = state.settings().icon.clone();
    let followed = match mode.as_str() {
        MODE_LOGO => None,
        MODE_TIGHTEST => tightest(&limits, now),
        account => tightest(limits.iter().filter(|l| l.account_id == account), now)
            .or_else(|| tightest(&limits, now)),
    };
    let icon = match (mode.as_str(), followed) {
        (MODE_LOGO, _) | (_, None) => app.default_window_icon().cloned(),
        (_, Some(limit)) => Some(ring_icon(limit, badge)),
    };
    if let Some(icon) = icon {
        let _ = tray.set_icon(Some(icon));
    }
    let _ = tray.set_tooltip(Some(tooltip(&limits, now)));

    let top = tightest(&limits, now);
    let reset = top
        .filter(|limit| limit.tier >= Tier::Low)
        .and_then(|limit| reset_for(usage, limit));
    *state.reset_key.lock().unwrap_or_else(|e| e.into_inner()) = reset.as_ref().map(|(key, _)| key.clone());
    if let Ok(menu) = build_menu(app, top, reset.map(|(_, count)| count), now) {
        let _ = tray.set_menu(Some(menu));
    }
}

/// Redraw from the newest snapshot, for changes that aren't a new collection:
/// a setting the menu reflects, an account hidden, the panel opened.
pub fn refresh(app: &AppHandle) {
    if let Some(usage) = app.state::<AlertState>().latest_usage() {
        update(app, &usage);
    } else if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(menu) = build_menu(app, None, None, 0) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

/// The panel was opened: whatever the badge was pointing at has been seen.
pub fn mark_seen(app: &AppHandle) {
    let state = app.state::<TrayState>();
    let changed = {
        let mut attention = state.attention();
        let fresh = attention.current.iter().any(|key| !attention.seen.contains(key));
        attention.seen = attention.current.clone();
        fresh
    };
    if changed {
        refresh(app);
    }
}

fn build_menu(
    app: &AppHandle,
    top: Option<&Limit>,
    resets: Option<u64>,
    now: i64,
) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    if let Some(limit) = top {
        let headline = if limit.tier == Tier::Out {
            format!("{}: used up", limit.who())
        } else {
            format!("{}: {:.0}% left", limit.who(), limit.left())
        };
        menu.append(&MenuItem::with_id(app, "info", headline, false, None::<&str>)?)?;
        let resets_at = limit.gauge.resets_at.filter(|at| *at > now);
        let detail = match (limit.tier, limit.runs_out_at, resets_at) {
            (Tier::Out, _, Some(at)) => Some(format!("Back at {}, in {}", clock_text(at), gap_text(at - now))),
            (_, Some(out), Some(at)) => Some(format!("Out by {} · resets {}", clock_text(out), clock_text(at))),
            _ => None,
        };
        if let Some(detail) = detail {
            menu.append(&MenuItem::with_id(app, "info_detail", detail, false, None::<&str>)?)?;
        }
        if let Some(count) = resets {
            let noun = match limit.provider {
                Provider::Codex => "reset credit",
                Provider::Claude => "banked reset",
            };
            menu.append(&MenuItem::with_id(
                app,
                "use_reset",
                format!("Use a {noun} ({count} left)…"),
                true,
                None::<&str>,
            )?)?;
        }
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }

    let widget_on = app.state::<crate::widget::WidgetState>().enabled();
    let notify = app.state::<AlertState>().settings();
    menu.append(&MenuItem::with_id(app, "open", "Open panel", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?)?;
    menu.append(&CheckMenuItem::with_id(
        app,
        "toggle_widget",
        "Taskbar widget",
        true,
        widget_on,
        None::<&str>,
    )?)?;
    menu.append(&CheckMenuItem::with_id(
        app,
        "toggle_alerts",
        "Limit alerts",
        true,
        notify.codex || notify.claude,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "refresh", "Refresh", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "check_updates", "Check for updates…", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?)?;
    Ok(menu)
}

// ---------------- icon ----------------

type Rgb = [f64; 3];
const TEAL: Rgb = [45.0, 212.0, 191.0];
const ORANGE: Rgb = [240.0, 163.0, 106.0];
const AMBER: Rgb = [223.0, 160.0, 95.0];
const RED: Rgb = [239.0, 107.0, 107.0];
const TRACK: Rgb = [140.0, 146.0, 158.0];
const OUTLINE: Rgb = [20.0, 22.0, 28.0];

/// A ring showing `limit`'s remaining share, clockwise from twelve o'clock.
fn ring_icon(limit: &Limit, badge: bool) -> Image<'static> {
    let colour = match limit.tier {
        Tier::Ok => match limit.provider {
            Provider::Codex => TEAL,
            Provider::Claude => ORANGE,
        },
        Tier::Low => AMBER,
        Tier::Crit | Tier::Out => RED,
    };
    let (track, track_alpha) = if limit.tier == Tier::Out { (RED, 0.5) } else { (TRACK, 0.45) };
    let pixels = draw_ring(
        limit.left() / 100.0,
        colour,
        track,
        track_alpha,
        limit.tier == Tier::Out,
        badge,
    );
    Image::new_owned(pixels, ICON_SIZE, ICON_SIZE)
}

/// RGBA pixels, 4×4 supersampled so the ring's edges and the arc's ends don't
/// stair-step at tray size.
fn draw_ring(fraction: f64, colour: Rgb, track: Rgb, track_alpha: f64, slash: bool, badge: bool) -> Vec<u8> {
    const SAMPLES: u32 = 4;
    const CENTRE: f64 = 16.0;
    const OUTER: f64 = 14.5;
    const INNER: f64 = 8.8;
    let mut out = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            // Premultiplied accumulation of every sample's colour and coverage.
            let mut acc = [0.0f64; 4];
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = x as f64 + (sx as f64 + 0.5) / SAMPLES as f64;
                    let py = y as f64 + (sy as f64 + 0.5) / SAMPLES as f64;
                    let (dx, dy) = (px - CENTRE, py - CENTRE);
                    let dist = (dx * dx + dy * dy).sqrt();
                    let mut sample: Option<(Rgb, f64)> = None;
                    if (INNER..=OUTER).contains(&dist) {
                        // 0 at twelve o'clock, growing clockwise.
                        let mut angle = dx.atan2(-dy) / std::f64::consts::TAU;
                        if angle < 0.0 {
                            angle += 1.0;
                        }
                        sample = Some(if angle <= fraction { (colour, 1.0) } else { (track, track_alpha) });
                    }
                    if slash && segment_distance(px, py, (10.0, 22.0), (22.0, 10.0)) <= 1.7 {
                        sample = Some((RED, 1.0));
                    }
                    if badge {
                        let bd = ((px - 26.0).powi(2) + (py - 6.0).powi(2)).sqrt();
                        if bd <= 4.6 {
                            sample = Some((ORANGE, 1.0));
                        } else if bd <= 6.0 {
                            sample = Some((OUTLINE, 1.0));
                        }
                    }
                    if let Some((rgb, alpha)) = sample {
                        acc[0] += rgb[0] * alpha;
                        acc[1] += rgb[1] * alpha;
                        acc[2] += rgb[2] * alpha;
                        acc[3] += alpha;
                    }
                }
            }
            let coverage = acc[3] / (SAMPLES * SAMPLES) as f64;
            if coverage <= 0.0 {
                continue;
            }
            let i = ((y * ICON_SIZE + x) * 4) as usize;
            out[i] = (acc[0] / acc[3]).round() as u8;
            out[i + 1] = (acc[1] / acc[3]).round() as u8;
            out[i + 2] = (acc[2] / acc[3]).round() as u8;
            out[i + 3] = (coverage * 255.0).round() as u8;
        }
    }
    out
}

fn segment_distance(px: f64, py: f64, a: (f64, f64), b: (f64, f64)) -> f64 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let t = (((px - a.0) * vx + (py - a.1) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * vx, a.1 + t * vy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

// ---------------- settings ----------------

fn store_path() -> Option<PathBuf> {
    let mut root = config_dir()?;
    root.push("tray.json");
    Some(root)
}

fn load_settings() -> TraySettings {
    store_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

#[tauri::command]
pub fn get_tray_settings(state: tauri::State<'_, TrayState>) -> TraySettings {
    state.settings().clone()
}

/// Choose what the icon follows: "tightest", "logo", or an account id.
#[tauri::command]
pub fn set_tray_icon_mode(
    app: AppHandle,
    state: tauri::State<'_, TrayState>,
    mode: String,
) -> Result<TraySettings, String> {
    let snapshot = {
        let mut settings = state.settings();
        settings.icon = mode;
        settings.clone()
    };
    let path = store_path().ok_or_else(|| "settings directory unavailable".to_string())?;
    let body = serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())?;
    write_atomic(&path, body.as_bytes()).map_err(|e| e.to_string())?;
    refresh(&app);
    let _ = app.emit("tray-settings", &snapshot);
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit<'a>(gauge: &'a Gauge, provider: Provider, label: &str, now: i64) -> Limit<'a> {
        Limit {
            provider,
            account_id: label.to_ascii_lowercase(),
            account_dir: String::new(),
            account_label: label.to_string(),
            gauge,
            tier: forecast::tier(gauge, now),
            runs_out_at: forecast::runs_out_at(gauge, now),
        }
    }

    fn gauge(used: f64, minutes: i64, resets_in: i64, now: i64) -> Gauge {
        Gauge {
            used_percent: used,
            window_minutes: minutes,
            resets_at: Some(now + resets_in),
            resets_in: None,
        }
    }

    #[test]
    fn the_tightest_window_is_the_worst_tier_then_the_fullest() {
        let now = 1_800_000_000;
        let calm = gauge(40.0, 300, 3_600, now);
        let low = gauge(85.0, 10_080, 86_400, now);
        let out = gauge(100.0, 300, 600, now);
        let limits = [
            limit(&calm, Provider::Codex, "Codex", now),
            limit(&low, Provider::Claude, "Work", now),
            limit(&out, Provider::Claude, "Personal", now),
        ];
        assert_eq!(tightest(&limits, now).unwrap().account_label, "Personal");
        assert_eq!(tightest(&limits[..2], now).unwrap().account_label, "Work");

        // Heading for empty: the sooner run-out wins over the fuller window.
        let weekly = gauge(91.0, 10_080, 4 * 86_400, now);
        let session = gauge(86.0, 300, 3_420, now);
        let racing = [
            limit(&weekly, Provider::Claude, "Work", now),
            limit(&session, Provider::Claude, "Personal", now),
        ];
        assert_eq!(racing[0].tier, Tier::Crit);
        assert_eq!(tightest(&racing, now).unwrap().account_label, "Personal");
    }

    #[test]
    fn the_tooltip_leads_with_the_worst_account_and_stays_under_the_limit() {
        let now = 1_800_000_000;
        let calm = gauge(38.0, 300, 7_800, now);
        let out = gauge(100.0, 300, 3_480, now);
        let limits = [
            limit(&calm, Provider::Codex, "Codex", now),
            limit(&out, Provider::Claude, "Personal", now),
        ];
        let text = tooltip(&limits, now);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "AI Usage");
        assert!(lines[1].starts_with("Personal used up · back"), "{text}");
        assert!(lines[2].starts_with("Codex 62% 5h"), "{text}");

        let many: Vec<Gauge> = (0..12).map(|i| gauge(10.0 + i as f64, 300, 3_600, now)).collect();
        let labels: Vec<String> = (0..12).map(|i| format!("A long account name {i}")).collect();
        let limits: Vec<Limit> = many
            .iter()
            .zip(&labels)
            .map(|(g, l)| limit(g, Provider::Claude, l, now))
            .collect();
        assert!(tooltip(&limits, now).encode_utf16().count() <= TOOLTIP_MAX);
    }

    #[test]
    fn the_ring_fills_clockwise_from_twelve() {
        let pixels = draw_ring(0.25, TEAL, TRACK, 0.45, false, false);
        let at = |x: u32, y: u32| {
            let i = ((y * ICON_SIZE + x) * 4) as usize;
            (pixels[i], pixels[i + 3])
        };
        // Three o'clock is the end of a quarter: still filled just above it.
        let (red_right, alpha_right) = at(27, 14);
        assert_eq!(red_right, TEAL[0] as u8);
        assert!(alpha_right > 200);
        // Nine o'clock is track, drawn semi-transparent.
        let (_, alpha_left) = at(4, 16);
        assert!(alpha_left < 160, "{alpha_left}");
        // The centre is empty.
        assert_eq!(at(16, 16).1, 0);
    }
}
