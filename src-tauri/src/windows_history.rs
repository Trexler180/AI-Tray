//! Rolling record of every quota window the app has seen.
//!
//! The usage endpoints only ever describe the window that is live *now*: a
//! percentage, a length, and a reset time. That is enough to place the current
//! window on a timeline, but says nothing about the ones that already closed —
//! so the timeline screen would have nothing to draw behind "now". This module
//! samples each window on every refresh and keeps the result in a small
//! versioned cache, which turns those point readings into history.
//!
//! Nothing is ever back-filled or invented: a window the app never observed
//! stays absent, and the UI draws it hollow rather than pretending it was idle.

use crate::alerts::usage_windows;
use crate::history_cache;
use crate::models::Usage;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const CACHE_NAME: &str = "windows";
const CACHE_VERSION: u32 = 1;
/// How far back the timeline can look. The two-week range is the longest the
/// UI offers, plus a little slack so the oldest bar doesn't vanish mid-view.
const RETAIN_SECS: i64 = 15 * 24 * 3600;
/// Samples kept per window instance. A 5h window polled every 30s would reach
/// 600; thinning to this keeps the burn curve's shape at a fraction of the size.
const MAX_SAMPLES: usize = 240;
/// An unchanged reading is only worth storing this far apart — enough to keep
/// the burn strip honest about idle stretches without logging every poll.
const SAMPLE_MIN_GAP_SECS: i64 = 15 * 60;
/// Usage falling by more than this between polls means the window rolled, even
/// if the reported reset time barely moved.
const RESET_DROP: f64 = 15.0;

#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug)]
pub struct Sample {
    pub at: i64,
    pub used: f64,
}

/// One occurrence of a window: the 5h session that ran this morning, the weekly
/// limit that resets on Friday. `used` is the last figure observed, which for a
/// closed window is its final spend.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct WindowInstance {
    pub start: i64,
    pub end: i64,
    pub used: f64,
    pub first_seen: i64,
    pub last_seen: i64,
    pub samples: Vec<Sample>,
}

/// Every instance of one window on one account, e.g. Claude · Work · weekly.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct WindowSeries {
    /// `provider|account|window` — stable across restarts and reorderings.
    pub key: String,
    pub provider: String,
    /// Claude account id (its config directory); empty for Codex.
    pub account: String,
    pub account_label: String,
    pub window: String,
    pub label: String,
    pub group: String,
    pub window_minutes: i64,
    pub instances: Vec<WindowInstance>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct WindowHistory {
    pub series: Vec<WindowSeries>,
    /// First time anything was recorded, so the UI can say how far back it can
    /// speak for instead of implying the gap before it was idle.
    pub recording_since: Option<i64>,
}

/// One window as read from a fresh snapshot, flattened so the merge logic can
/// be tested without a `Usage` tree behind it.
#[derive(Clone, Debug)]
pub struct Observation {
    pub key: String,
    pub provider: String,
    pub account: String,
    pub account_label: String,
    pub window: String,
    pub label: String,
    pub group: String,
    pub window_minutes: i64,
    /// Unix seconds when this window resets.
    pub end: i64,
    pub used: f64,
}

fn store() -> &'static Mutex<WindowHistory> {
    static STORE: OnceLock<Mutex<WindowHistory>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(history_cache::load::<WindowHistory>(
            CACHE_NAME,
            CACHE_VERSION,
        ))
    })
}

/// Sample every live window in a snapshot. Called from the one place every
/// refresh path funnels through, so the record doesn't depend on which trigger
/// (panel, watcher, or ticker) collected the data.
pub fn record(usage: &Usage) {
    let observations: Vec<Observation> = usage_windows(usage)
        .into_iter()
        .filter_map(|snapshot| {
            // A window with no reported reset time or length can't be placed on
            // a timeline; the gauges still show it, this simply skips it.
            let end = snapshot.gauge.resets_at?;
            let minutes = snapshot.gauge.window_minutes;
            if minutes <= 0 {
                return None;
            }
            Some(Observation {
                key: format!(
                    "{}|{}|{}",
                    snapshot.key.provider.key(),
                    snapshot.key.account,
                    snapshot.key.window
                ),
                provider: snapshot.key.provider.key().to_string(),
                account: snapshot.key.account.clone(),
                account_label: snapshot.account_label.clone(),
                window: snapshot.key.window.clone(),
                label: snapshot.display_label.clone(),
                group: snapshot.group.clone(),
                window_minutes: minutes,
                end,
                used: snapshot.gauge.used_percent,
            })
        })
        .collect();

    if observations.is_empty() {
        return;
    }

    let now = Utc::now().timestamp();
    let mut history = store().lock().unwrap();
    if !apply(&mut history, &observations, now) {
        return;
    }
    if let Err(error) = history_cache::save(CACHE_NAME, CACHE_VERSION, &*history) {
        eprintln!("window history save failed: {error}");
    }
}

/// Fold a round of observations into the record. Returns whether anything
/// changed, so an idle poll doesn't rewrite the cache file.
pub fn apply(history: &mut WindowHistory, observations: &[Observation], now: i64) -> bool {
    let mut changed = false;

    if history.recording_since.is_none() {
        history.recording_since = Some(now);
        changed = true;
    }

    for observation in observations {
        let index = match history.series.iter().position(|s| s.key == observation.key) {
            Some(index) => index,
            None => {
                history.series.push(WindowSeries {
                    key: observation.key.clone(),
                    provider: observation.provider.clone(),
                    account: observation.account.clone(),
                    account_label: observation.account_label.clone(),
                    window: observation.window.clone(),
                    label: observation.label.clone(),
                    group: observation.group.clone(),
                    window_minutes: observation.window_minutes,
                    instances: Vec::new(),
                });
                changed = true;
                history.series.len() - 1
            }
        };
        let series = &mut history.series[index];
        // Labels and lengths follow the provider: a renamed account or a plan
        // change shouldn't leave the timeline captioned with the old value.
        if series.label != observation.label {
            series.label.clone_from(&observation.label);
            changed = true;
        }
        if series.account_label != observation.account_label {
            series.account_label.clone_from(&observation.account_label);
            changed = true;
        }
        if series.window_minutes != observation.window_minutes {
            series.window_minutes = observation.window_minutes;
            changed = true;
        }
        changed |= observe(series, observation, now);
    }

    if changed {
        prune(history, now);
    }
    changed
}

/// Place one reading on its series, either extending the instance it belongs to
/// or starting a new one.
fn observe(series: &mut WindowSeries, observation: &Observation, now: i64) -> bool {
    let span = observation.window_minutes * 60;
    // Rolling windows nudge their reset time every poll, so an exact match on
    // `end` would file each reading as a brand-new window. Anything within half
    // a span, without a usage collapse, is the same window still running.
    let tolerance = (span / 2).max(300);
    let continues = series.instances.last().is_some_and(|last| {
        (observation.end - last.end).abs() <= tolerance && observation.used + RESET_DROP >= last.used
    });

    if !continues {
        series.instances.push(WindowInstance {
            start: observation.end - span,
            end: observation.end,
            used: observation.used,
            first_seen: now,
            last_seen: now,
            samples: vec![Sample {
                at: now,
                used: observation.used,
            }],
        });
        return true;
    }

    let last = series.instances.last_mut().expect("checked above");
    let previous = last.used;
    last.end = observation.end;
    last.start = observation.end - span;
    last.used = observation.used;
    last.last_seen = now;

    // Store a sample when the number moved, or when enough time has passed that
    // a flat stretch is itself worth recording.
    let due = last
        .samples
        .last()
        .is_none_or(|s| now - s.at >= SAMPLE_MIN_GAP_SECS);
    if (observation.used - previous).abs() >= 0.05 || due {
        last.samples.push(Sample {
            at: now,
            used: observation.used,
        });
        thin_samples(last);
        return true;
    }
    // The instance's end/last_seen still moved, but not by enough to be worth a
    // disk write on its own.
    false
}

/// Halve a sample list that has outgrown the cap, keeping the first and last
/// readings so the instance still starts and ends where it really did.
fn thin_samples(instance: &mut WindowInstance) {
    if instance.samples.len() <= MAX_SAMPLES {
        return;
    }
    let last = *instance.samples.last().expect("non-empty");
    let mut kept: Vec<Sample> = instance
        .samples
        .iter()
        .step_by(2)
        .copied()
        .collect();
    if kept.last().map(|s| s.at) != Some(last.at) {
        kept.push(last);
    }
    instance.samples = kept;
}

fn prune(history: &mut WindowHistory, now: i64) {
    let cutoff = now - RETAIN_SECS;
    for series in history.series.iter_mut() {
        series.instances.retain(|instance| instance.end >= cutoff);
    }
    history.series.retain(|series| !series.instances.is_empty());
}

pub fn history() -> WindowHistory {
    store().lock().unwrap().clone()
}

/// Quota spent on one window over a recent stretch, in the same percent-of-
/// allowance scale the gauges use. This is what the meters draw as a band at
/// the fill edge, so both windows read it from here rather than each deriving
/// it from the raw samples — two copies would drift the moment either is
/// tweaked.
#[derive(Serialize, Clone, Default, Debug, PartialEq)]
pub struct RecentBurn {
    pub spent: f64,
    /// How far back the figure really reaches, which the record decides rather
    /// than the range asked for: shorter when the app hasn't been running that
    /// long, longer when a climb straddles the start of the range.
    pub covered_seconds: i64,
    /// Whether that is close enough to the range asked for to be drawn as one.
    pub matched: bool,
}

/// How much further back than the range asked for a figure may reach and still
/// be drawn. A climb recorded either side of a stretch with the app shut lands
/// in a single pair of samples that can span hours; the reading is still
/// honest, but a 20% band under a "15m" setting would be read as 20% in fifteen
/// minutes, so the caller is told not to draw it.
const STRETCH_LIMIT: i64 = 2;

/// Recent spend for every window on record, keyed the same way the series are.
pub fn recent(minutes: i64) -> HashMap<String, RecentBurn> {
    recent_in(&store().lock().unwrap(), minutes, Utc::now().timestamp())
}

/// Only rises count, the same reasoning the timeline's burn strip uses: a
/// rolling window's percentage falls as old usage ages out, and that is not
/// quota coming back. Earlier instances are left out — they describe a window
/// that has since reset, so their spend is no longer on the meter.
pub fn recent_in(
    history: &WindowHistory,
    minutes: i64,
    now: i64,
) -> HashMap<String, RecentBurn> {
    let minutes = minutes.max(1);
    let from = now - minutes * 60;
    let mut out = HashMap::new();

    for series in &history.series {
        let Some(samples) = series
            .instances
            .last()
            .map(|instance| &instance.samples)
            .filter(|samples| !samples.is_empty())
        else {
            continue;
        };

        // The last reading taken at or before the range opened is the baseline;
        // every sample after it is inside the range.
        let mut base = 0;
        while base + 1 < samples.len() && samples[base + 1].at <= from {
            base += 1;
        }

        let mut spent = 0.0;
        let mut anchor = samples[base].at.max(from);
        for i in base + 1..samples.len() {
            let rise = samples[i].used - samples[i - 1].used;
            if rise <= 0.0 {
                continue;
            }
            spent += rise;
            anchor = anchor.min(samples[i - 1].at);
        }

        let covered_seconds = now - anchor;
        out.insert(
            series.key.clone(),
            RecentBurn {
                spent,
                covered_seconds,
                matched: covered_seconds <= minutes * 60 * STRETCH_LIMIT,
            },
        );
    }
    out
}

pub fn clear() -> std::io::Result<()> {
    *store().lock().unwrap() = WindowHistory::default();
    history_cache::clear(CACHE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3600;

    fn observation(end: i64, used: f64) -> Observation {
        Observation {
            key: "codex||session".to_string(),
            provider: "codex".to_string(),
            account: String::new(),
            account_label: String::new(),
            window: "session".to_string(),
            label: "Session (5h)".to_string(),
            group: "session".to_string(),
            window_minutes: 300,
            end,
            used,
        }
    }

    #[test]
    fn a_rolled_window_becomes_a_second_instance_and_keeps_the_old_final_figure() {
        let mut history = WindowHistory::default();
        let now = 1_800_000_000;
        apply(&mut history, &[observation(now + HOUR, 82.0)], now);
        // Five hours later the window has rolled: new reset time, usage reset.
        let later = now + 5 * HOUR;
        apply(&mut history, &[observation(later + 5 * HOUR, 4.0)], later);

        let series = &history.series[0];
        assert_eq!(series.instances.len(), 2);
        assert_eq!(series.instances[0].used, 82.0);
        assert_eq!(series.instances[1].used, 4.0);
        // Start is derived from the reported length, not guessed.
        assert_eq!(
            series.instances[1].start,
            series.instances[1].end - 300 * 60
        );
    }

    #[test]
    fn a_drifting_reset_time_stays_one_instance() {
        let mut history = WindowHistory::default();
        let now = 1_800_000_000;
        apply(&mut history, &[observation(now + 2 * HOUR, 30.0)], now);
        // Same window a minute later, reset time nudged the way rolling windows do.
        apply(
            &mut history,
            &[observation(now + 2 * HOUR + 45, 31.0)],
            now + 60,
        );

        assert_eq!(history.series[0].instances.len(), 1);
        assert_eq!(history.series[0].instances[0].used, 31.0);
        assert_eq!(history.series[0].instances[0].samples.len(), 2);
    }

    #[test]
    fn a_usage_collapse_starts_a_new_instance_even_when_the_reset_time_holds() {
        let mut history = WindowHistory::default();
        let now = 1_800_000_000;
        apply(&mut history, &[observation(now + HOUR, 96.0)], now);
        apply(&mut history, &[observation(now + HOUR + 30, 2.0)], now + 120);

        assert_eq!(history.series[0].instances.len(), 2);
    }

    #[test]
    fn an_unchanged_reading_is_not_resampled_until_the_gap_passes() {
        let mut history = WindowHistory::default();
        let now = 1_800_000_000;
        apply(&mut history, &[observation(now + HOUR, 30.0)], now);
        let quiet = apply(&mut history, &[observation(now + HOUR, 30.0)], now + 60);
        assert!(!quiet, "an identical reading a minute later is not news");
        assert_eq!(history.series[0].instances[0].samples.len(), 1);

        let due = apply(
            &mut history,
            &[observation(now + HOUR, 30.0)],
            now + SAMPLE_MIN_GAP_SECS,
        );
        assert!(due);
        assert_eq!(history.series[0].instances[0].samples.len(), 2);
    }

    #[test]
    fn instances_older_than_the_retention_window_are_dropped() {
        let mut history = WindowHistory::default();
        let old = 1_800_000_000;
        apply(&mut history, &[observation(old + HOUR, 50.0)], old);
        let now = old + RETAIN_SECS + 2 * HOUR;
        apply(&mut history, &[observation(now + HOUR, 10.0)], now);

        assert_eq!(history.series[0].instances.len(), 1);
        assert_eq!(history.series[0].instances[0].used, 10.0);
    }

    /// A series whose newest instance carries `samples`, given as
    /// (minutes before `now`, used percent).
    fn series_with(now: i64, samples: &[(i64, f64)]) -> WindowHistory {
        WindowHistory {
            series: vec![WindowSeries {
                key: "codex||session".to_string(),
                instances: vec![WindowInstance {
                    samples: samples
                        .iter()
                        .map(|(ago, used)| Sample {
                            at: now - ago * 60,
                            used: *used,
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            recording_since: Some(now),
        }
    }

    fn burn(now: i64, samples: &[(i64, f64)], minutes: i64) -> RecentBurn {
        recent_in(&series_with(now, samples), minutes, now)
            .remove("codex||session")
            .expect("the series is on record")
    }

    #[test]
    fn recent_spend_sums_the_climb_inside_the_range() {
        let now = 1_800_000_000;
        let got = burn(now, &[(70, 5.0), (10, 5.0), (5, 8.0), (0, 12.0)], 60);
        assert_eq!(got.spent, 7.0);
        assert_eq!(got.covered_seconds, 60 * 60);
        assert!(got.matched);
    }

    #[test]
    fn a_rolling_windows_fall_does_not_net_against_the_rises() {
        let now = 1_800_000_000;
        // 20 → 30 → 26 → 31: the dip is usage aging out, not quota returning.
        let got = burn(now, &[(70, 20.0), (40, 30.0), (20, 26.0), (0, 31.0)], 60);
        assert_eq!(got.spent, 15.0);
        // The first climb straddles the start of the range, so the figure has to
        // own up to reaching further back than an hour.
        assert_eq!(got.covered_seconds, 70 * 60);
    }

    #[test]
    fn a_record_shorter_than_the_range_only_speaks_for_what_it_covers() {
        let now = 1_800_000_000;
        let got = burn(now, &[(8, 40.0), (4, 43.0), (0, 45.0)], 60);
        assert_eq!(got.spent, 5.0);
        assert_eq!(got.covered_seconds, 8 * 60);
        assert!(got.matched);
    }

    #[test]
    fn an_idle_window_reads_as_no_usage_over_the_full_range() {
        let now = 1_800_000_000;
        // Flat windows only resample every SAMPLE_MIN_GAP_SECS, so the newest
        // reading can predate the range without anything having happened.
        let got = burn(now, &[(90, 30.0), (14, 30.0)], 10);
        assert_eq!(got.spent, 0.0);
        assert_eq!(got.covered_seconds, 10 * 60);
        assert!(got.matched);
    }

    #[test]
    fn a_climb_across_a_long_gap_is_reported_but_not_drawable() {
        let now = 1_800_000_000;
        // One pair spanning an app restart: honest, but not a fifteen-minute
        // figure however it is labelled.
        let got = burn(now, &[(70, 10.0), (1, 30.0)], 15);
        assert_eq!(got.spent, 20.0);
        assert_eq!(got.covered_seconds, 70 * 60);
        assert!(!got.matched);
    }

    #[test]
    fn a_window_that_only_fell_reads_as_no_usage() {
        let now = 1_800_000_000;
        let got = burn(now, &[(50, 60.0), (20, 52.0), (0, 44.0)], 60);
        assert_eq!(got.spent, 0.0);
        // The record starts 50 minutes in, so that is all it can speak for.
        assert_eq!(got.covered_seconds, 50 * 60);
    }

    #[test]
    fn a_window_with_no_samples_is_absent_rather_than_zero() {
        let now = 1_800_000_000;
        assert!(recent_in(&series_with(now, &[]), 60, now).is_empty());
        assert!(recent_in(&WindowHistory::default(), 60, now).is_empty());
    }

    #[test]
    fn only_the_live_instance_counts_toward_recent_spend() {
        let now = 1_800_000_000;
        let mut history = series_with(now, &[(10, 4.0), (0, 9.0)]);
        // A window that closed inside the range: its spend is not on the meter
        // any more, so it must not be added to the live one's.
        history.series[0].instances.insert(
            0,
            WindowInstance {
                samples: vec![
                    Sample {
                        at: now - 40 * 60,
                        used: 50.0,
                    },
                    Sample {
                        at: now - 20 * 60,
                        used: 90.0,
                    },
                ],
                ..Default::default()
            },
        );
        let got = recent_in(&history, 60, now).remove("codex||session").unwrap();
        assert_eq!(got.spent, 5.0);
    }

    #[test]
    fn samples_are_thinned_but_keep_the_final_reading() {
        let mut instance = WindowInstance {
            samples: (0..MAX_SAMPLES + 40)
                .map(|i| Sample {
                    at: i as i64,
                    used: i as f64,
                })
                .collect(),
            ..Default::default()
        };
        let last = *instance.samples.last().unwrap();
        thin_samples(&mut instance);

        assert!(instance.samples.len() <= MAX_SAMPLES);
        assert_eq!(instance.samples.first().unwrap().at, 0);
        assert_eq!(instance.samples.last().unwrap().at, last.at);
    }
}
