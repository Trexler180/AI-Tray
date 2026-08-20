use crate::models::DayBucket;
use chrono::{Duration, Local};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// This app's own configuration directory (`%APPDATA%\AI Usage Tray` on Windows),
/// falling back to the home directory on systems without a config dir. Holds the
/// settings files and the derived history indexes.
pub fn config_dir() -> Option<PathBuf> {
    let mut root = dirs::config_dir().or_else(dirs::home_dir)?;
    root.push("AI Usage Tray");
    Some(root)
}

/// Write a file by filling a sibling temp file and renaming it over the target,
/// so a reader never sees a half-written file and a failed write leaves the old
/// contents intact.
///
/// The temp name carries a fresh uuid. Every settings file used to share the
/// single name `<file>.tmp-aiusage`, which meant two saves landing together —
/// the panel and the widget both persisting, say — could interleave their bytes
/// into one temp file and rename the mixture into place.
pub fn write_atomic(path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.tmp-aiusage", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&tmp, body) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Don't leave the temp behind to accumulate in the config directory.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Local calendar date as YYYY-MM-DD.
pub fn today_str() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Expand a sparse per-day map into the last `days` calendar days (oldest
/// first), filling gaps with zeros so the chart reflects real time.
pub fn fill_daily(per_day: &BTreeMap<String, (u64, f64)>, days: i64) -> Vec<DayBucket> {
    let today = Local::now().date_naive();
    (0..days)
        .rev()
        .map(|i| {
            let date = (today - Duration::days(i)).format("%Y-%m-%d").to_string();
            let (tokens, cost) = per_day.get(&date).copied().unwrap_or((0, 0.0));
            DayBucket { date, tokens, cost }
        })
        .collect()
}

/// Human "time remaining" until a unix timestamp, e.g. "36m", "2d 18h".
pub fn human_until(resets_at: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let mut secs = resets_at - now;
    if secs <= 0 {
        return "now".to_string();
    }
    let days = secs / 86400;
    secs %= 86400;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;

    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else {
        format!("{}m", mins.max(1))
    }
}

/// Name a Codex rate-limit window from the duration it actually covers.
///
/// Codex reports its windows positionally (`primary_window`/`secondary_window`
/// live, `primary`/`secondary` in the session logs) and which limits exist has
/// changed over time — with the 5h session limit switched off, the weekly
/// window is the only one reported and arrives in the *primary* slot. Naming
/// from the reported length keeps the bar honest in either arrangement.
///
/// Returns `(id, label, group)`, or `None` when the response carries no usable
/// duration and the caller has to fall back to position.
pub fn codex_window_meta(window_minutes: i64) -> Option<(String, String, &'static str)> {
    if window_minutes <= 0 {
        return None;
    }
    // Anything under a day is the short rolling window users call the
    // "session" limit, whatever length Codex currently sets it to.
    if window_minutes < 1_440 {
        let label = if window_minutes % 60 == 0 {
            format!("Session ({}h)", window_minutes / 60)
        } else {
            format!("Session ({window_minutes}m)")
        };
        return Some(("session".to_string(), label, "session"));
    }
    let days = window_minutes / 1_440;
    Some(match days {
        1 => ("daily".to_string(), "Daily".to_string(), "daily"),
        6..=8 => ("weekly".to_string(), "Weekly".to_string(), "weekly"),
        28..=31 => ("monthly".to_string(), "Monthly".to_string(), "monthly"),
        n => (format!("window_{n}d"), format!("{n}-day limit"), "other"),
    })
}

/// Fallback naming for a Codex window whose duration the response omitted,
/// derived from its slot name (`primary`, `secondary`, or anything newer).
/// The session label carries no hour count here because it isn't known.
pub fn codex_window_meta_by_slot(slot: &str) -> (String, String, &'static str) {
    match slot {
        "primary" => ("session".to_string(), "Session".to_string(), "session"),
        "secondary" => ("weekly".to_string(), "Weekly".to_string(), "weekly"),
        other => (other.to_string(), other.replace('_', " "), "other"),
    }
}

/// Test-only: copy a fixture across, landing its hardcoded calendar dates on
/// recent ones.
///
/// The fixtures are written with absolute dates, but everything asserted about
/// them reads a window measured from `now` — `tokens_30d`, the daily buckets.
/// Left alone they pass until the fixture turns 30 days old and then fail for a
/// reason that has nothing to do with the code under test, which is exactly
/// what happened. `remap` pairs each date string in the file with how many days
/// before today it should land on, so the spacing between them survives.
#[cfg(test)]
pub fn copy_fixture_dated(src: &std::path::Path, dest: &std::path::Path, remap: &[(&str, i64)]) {
    let mut text = std::fs::read_to_string(src).expect("fixture is readable");
    for (original, days_ago) in remap {
        text = text.replace(original, &days_ago_utc(*days_ago));
    }
    std::fs::write(dest, text).expect("fixture copy is writable");
}

/// `YYYY-MM-DD`, that many days before today, UTC. Matches the date format the
/// fixtures and the session folder layout both use.
#[cfg(test)]
pub fn days_ago_utc(days: i64) -> String {
    (chrono::Utc::now() - Duration::days(days))
        .format("%Y-%m-%d")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_windows_are_named_by_duration_not_slot() {
        assert_eq!(codex_window_meta(300).unwrap().1, "Session (5h)");
        assert_eq!(codex_window_meta(180).unwrap().1, "Session (3h)");
        assert_eq!(codex_window_meta(10_080).unwrap().0, "weekly");
        assert_eq!(codex_window_meta(10_080).unwrap().1, "Weekly");
        assert_eq!(codex_window_meta(43_200).unwrap().1, "Monthly");
        assert_eq!(codex_window_meta(0), None);
        // A length nobody has shipped yet still gets a truthful label.
        assert_eq!(codex_window_meta(3 * 1_440).unwrap().1, "3-day limit");
    }

    #[test]
    fn slot_fallback_drops_the_unknown_hour_count() {
        assert_eq!(codex_window_meta_by_slot("primary").1, "Session");
        assert_eq!(codex_window_meta_by_slot("secondary").1, "Weekly");
    }

    #[test]
    fn human_until_past_is_now() {
        assert_eq!(human_until(0), "now");
        assert_eq!(human_until(chrono::Utc::now().timestamp() - 5), "now");
    }

    #[test]
    fn human_until_formats() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(human_until(now + 90), "1m");
        assert_eq!(human_until(now + 3 * 3600 + 70), "3h 1m");
        assert_eq!(human_until(now + 2 * 86400 + 5 * 3600), "2d 5h");
    }

    /// A settings write must land whole and leave nothing behind, including
    /// when it replaces an existing file.
    #[test]
    fn atomic_write_replaces_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("aiusage-test-{}", uuid::Uuid::new_v4()));
        let target = dir.join("settings.json");

        write_atomic(&target, b"{\"first\":true}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"first\":true}");

        write_atomic(&target, b"{\"second\":true}").unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "{\"second\":true}"
        );

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "settings.json")
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug this replaced: every writer shared one temp name, so two saves
    /// landing together could interleave into it. Distinct names per write are
    /// what makes that impossible.
    #[test]
    fn each_write_uses_its_own_temp_name() {
        let dir = std::env::temp_dir().join(format!("aiusage-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.json");
        let b = dir.join("b.json");
        // Same directory, same extension shape — the old scheme gave both of
        // these the same `.tmp-aiusage` sibling.
        write_atomic(&a, b"a").unwrap();
        write_atomic(&b, b"b").unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fill_daily_covers_every_day() {
        let mut map = BTreeMap::new();
        map.insert(today_str(), (100u64, 1.5f64));
        let out = fill_daily(&map, 14);
        assert_eq!(out.len(), 14);
        assert_eq!(out.last().unwrap().date, today_str());
        assert_eq!(out.last().unwrap().tokens, 100);
        assert_eq!(out[0].tokens, 0);
        // strictly ascending dates
        assert!(out.windows(2).all(|w| w[0].date < w[1].date));
    }
}
