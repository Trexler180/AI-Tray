//! Whether a quota window will run out before it resets, and how worried to be
//! about it.
//!
//! Mirrors `forecast` and `meterTier` in `src/shared.js`. The tray icon, the
//! notifications and the panel's meters all have to agree on which window is
//! "about to run out", so the rule exists once per language and both copies
//! are tested against the same cases.

use crate::models::Gauge;

/// Usage from which a meter reads as running low.
pub const LOW_PERCENT: f64 = 80.0;
/// Usage from which a meter reads as critical whatever its pace.
pub const CRIT_PERCENT: f64 = 95.0;
/// Below this a window is never flagged as running out early. A fast first hour
/// on an empty window is normal, and saying "out by 2 pm" at 20% used is noise.
pub const FORECAST_FLOOR: f64 = 50.0;
/// Until this share of the window has gone by, the average rate describes one
/// busy stretch rather than the window.
const MIN_ELAPSED_FRACTION: f64 = 0.1;
const MIN_ELAPSED_SECS: i64 = 10 * 60;
/// A run-out this close to the reset isn't worth a warning: the window refills
/// before anyone would notice the gap.
const SLACK_SECS: i64 = 120;

/// How a meter should read, ordered from calm to worst so the tightest window
/// is simply the maximum.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Tier {
    Ok,
    Low,
    Crit,
    Out,
}

/// Unix seconds when the window reaches 100% at its average pace so far, when
/// that comes before it resets. None when the window will last, is already
/// used up, or there is too little of it gone by to judge.
pub fn runs_out_at(gauge: &Gauge, now: i64) -> Option<i64> {
    let used = gauge.used_percent;
    if !(FORECAST_FLOOR..100.0).contains(&used) {
        return None;
    }
    let resets_at = gauge.resets_at?;
    let span = gauge.window_minutes * 60;
    if span <= 0 || resets_at <= now {
        return None;
    }
    let elapsed = span - (resets_at - now);
    if elapsed < MIN_ELAPSED_SECS || (elapsed as f64) < span as f64 * MIN_ELAPSED_FRACTION {
        return None;
    }
    let per_second = used / elapsed as f64;
    let at = now + ((100.0 - used) / per_second).round() as i64;
    (at < resets_at - SLACK_SECS).then_some(at)
}

pub fn tier(gauge: &Gauge, now: i64) -> Tier {
    let used = gauge.used_percent;
    if used >= 100.0 {
        Tier::Out
    } else if used >= CRIT_PERCENT || runs_out_at(gauge, now).is_some() {
        Tier::Crit
    } else if used >= LOW_PERCENT {
        Tier::Low
    } else {
        Tier::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    /// A five-hour window `elapsed` of the way through, `used` percent spent.
    fn session(used: f64, elapsed: f64) -> Gauge {
        let span = 300 * 60;
        Gauge {
            used_percent: used,
            window_minutes: 300,
            resets_at: Some(NOW + ((1.0 - elapsed) * span as f64) as i64),
            resets_in: None,
        }
    }

    #[test]
    fn a_window_spending_faster_than_it_refills_runs_out_first() {
        // 86% gone with 19% of the window left: about 40 minutes of quota for
        // 57 minutes of window.
        let gauge = session(86.0, 0.81);
        let at = runs_out_at(&gauge, NOW).expect("runs out");
        assert!(at < gauge.resets_at.unwrap());
        assert!((2_300..2_500).contains(&(at - NOW)), "{}", at - NOW);
        assert_eq!(tier(&gauge, NOW), Tier::Crit);
    }

    #[test]
    fn a_high_window_that_will_last_is_only_low() {
        assert_eq!(runs_out_at(&session(86.0, 0.92), NOW), None);
        assert_eq!(tier(&session(86.0, 0.92), NOW), Tier::Low);
    }

    #[test]
    fn early_or_light_windows_are_not_forecast() {
        // Fast pace, but only five minutes in.
        assert_eq!(runs_out_at(&session(60.0, 0.016), NOW), None);
        // Below the floor however fast it is going.
        assert_eq!(runs_out_at(&session(40.0, 0.2), NOW), None);
        assert_eq!(tier(&session(40.0, 0.2), NOW), Tier::Ok);
    }

    #[test]
    fn tiers_follow_the_thresholds() {
        assert_eq!(tier(&session(96.0, 0.99), NOW), Tier::Crit);
        assert_eq!(tier(&session(100.0, 0.5), NOW), Tier::Out);
        let no_clock = Gauge {
            used_percent: 85.0,
            ..Default::default()
        };
        assert_eq!(runs_out_at(&no_clock, NOW), None);
        assert_eq!(tier(&no_clock, NOW), Tier::Low);
        assert!(Tier::Out > Tier::Crit && Tier::Crit > Tier::Low && Tier::Low > Tier::Ok);
    }
}
