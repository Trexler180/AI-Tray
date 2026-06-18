use crate::models::DayBucket;
use chrono::{Duration, Local};
use std::collections::BTreeMap;

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

#[cfg(test)]
mod tests {
    use super::*;

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
