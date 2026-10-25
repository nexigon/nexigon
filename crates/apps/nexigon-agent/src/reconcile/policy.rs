//! Policies that decide when a device may attempt an update.
//!
//! These functions are pure: they evaluate maintenance windows, retry delays,
//! and spreads for a given point in time, and apply the configuration property
//! to the defaults as a JSON merge patch.

use std::time::Duration;

use anyhow::Context;
use jiff::Timestamp;
use jiff::ToSpan;
use jiff::Zoned;
use jiff::civil::Time;
use nexigon_api::types::properties::OtaUpdateRetryPolicy;
use nexigon_api::types::properties::OtaUpdateSpread;
use nexigon_api::types::properties::OtaUpdateWindow;
use nexigon_api::types::properties::Weekday;
use sha2::Digest;
use sha2::Sha256;
use sidex::types::Str;

/// Default delay before the first retry.
pub(crate) const DEFAULT_INITIAL_RETRY_DELAY_SECS: u32 = 300;
/// Default upper bound for the delay between attempts.
pub(crate) const DEFAULT_MAX_RETRY_DELAY_SECS: u32 = 4 * 60 * 60;

/// Whether a maintenance window allows activation at a point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WindowState {
    Open,
    /// The window is closed and opens at the given time, if ever.
    Closed {
        opens_at: Option<Timestamp>,
    },
}

/// Evaluate a maintenance window at `now`, in the time zone of `now`.
///
/// Fails if a range contains a time that is not in the format `HH:MM`.
pub(crate) fn window_state(window: &OtaUpdateWindow, now: &Zoned) -> anyhow::Result<WindowState> {
    let mut opens_at: Option<Zoned> = None;
    // Ranges span at most 24 hours, so a range starting yesterday may still be
    // open. A week ahead covers the next start of every range.
    for offset in -1i32..=7 {
        let day = now.date().checked_add(offset.days())?;
        for range in window.ranges.iter() {
            let starts_on_day = range.days.as_ref().is_none_or(|days| {
                days.iter()
                    .any(|weekday| same_weekday(weekday, day.weekday()))
            });
            if !starts_on_day {
                continue;
            }
            let start_time = parse_time(&Str::from(range.start.clone()))?;
            let end_time = parse_time(&Str::from(range.end.clone()))?;
            let end_day = if end_time <= start_time {
                day.checked_add(1.day())?
            } else {
                day
            };
            let start = day
                .to_datetime(start_time)
                .to_zoned(now.time_zone().clone())?;
            let end = end_day
                .to_datetime(end_time)
                .to_zoned(now.time_zone().clone())?;
            if start <= *now && *now < end {
                return Ok(WindowState::Open);
            }
            if start > *now && opens_at.as_ref().is_none_or(|current| start < *current) {
                opens_at = Some(start);
            }
        }
    }
    Ok(WindowState::Closed {
        opens_at: opens_at.map(|opens_at| opens_at.timestamp()),
    })
}

/// Fill in the defaults the integration applies to a retry policy.
pub(crate) fn retry_policy_with_defaults(
    policy: Option<&OtaUpdateRetryPolicy>,
) -> OtaUpdateRetryPolicy {
    let mut policy = policy.cloned().unwrap_or_else(OtaUpdateRetryPolicy::new);
    policy.initial_delay_secs = Some(
        policy
            .initial_delay_secs
            .unwrap_or(DEFAULT_INITIAL_RETRY_DELAY_SECS),
    );
    policy.max_delay_secs = Some(
        policy
            .max_delay_secs
            .unwrap_or(DEFAULT_MAX_RETRY_DELAY_SECS),
    );
    policy
}

/// Time until the next attempt after the given number of consecutive failures.
///
/// Returns `None` if no further attempt is allowed: the retry limit has been
/// reached and the policy has no cooldown.
pub(crate) fn retry_delay(policy: &OtaUpdateRetryPolicy, failures: u32) -> Option<Duration> {
    if policy
        .max_attempts
        .is_some_and(|max_attempts| failures >= max_attempts)
    {
        return policy
            .cooldown_secs
            .map(|cooldown| Duration::from_secs(u64::from(cooldown)));
    }
    let policy = retry_policy_with_defaults(Some(policy));
    let initial = u64::from(policy.initial_delay_secs.unwrap_or_default());
    let max = u64::from(policy.max_delay_secs.unwrap_or_default());
    let factor = 1u64
        .checked_shl(failures.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Some(Duration::from_secs(initial.saturating_mul(factor).min(max)))
}

/// Position of a device in the spreads of a target version, in `[0, 1)`.
///
/// The position is the first 53 bits of the SHA-256 digest of
/// `<device-id>/<target-version-id>`, divided by 2^53. It is exact in
/// double-precision arithmetic.
pub(crate) fn spread_position(device_id: &str, version_id: &str) -> f64 {
    let digest = Sha256::digest(format!("{device_id}/{version_id}").as_bytes());
    let prefix = u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 digests are longer than 8 bytes"),
    );
    (prefix >> 11) as f64 / (1u64 << 53) as f64
}

/// Whether a spread has released a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpreadState {
    Released,
    /// The device is not released yet. It is released at the given time, if
    /// that time is known.
    Waiting {
        released_at: Option<Timestamp>,
    },
}

/// Evaluate a spread for a device at `position`.
///
/// `start` is the start of the spread over time. Without it, only the
/// percentage applies.
pub(crate) fn spread_state(
    spread: &OtaUpdateSpread,
    start: Option<Timestamp>,
    position: f64,
    now: Timestamp,
) -> SpreadState {
    let within_percentage = spread
        .percentage
        .is_none_or(|percentage| position < percentage / 100.0);
    let released_at = match (spread.duration_secs, start) {
        (Some(duration), Some(start)) => {
            let delay = (f64::from(duration) * position.sqrt()).floor() as i64;
            start.checked_add(delay.seconds()).ok()
        }
        _ => None,
    };
    match released_at {
        _ if !within_percentage => SpreadState::Waiting { released_at: None },
        Some(released_at) if now < released_at => SpreadState::Waiting {
            released_at: Some(released_at),
        },
        _ => SpreadState::Released,
    }
}

/// Apply `patch` to `target` as a JSON merge patch (RFC 7396).
pub(crate) fn merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    let serde_json::Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = serde_json::Value::Object(serde_json::Map::new());
    }
    let target = target
        .as_object_mut()
        .expect("non-object targets were replaced by an empty object");
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            merge_patch(
                target.entry(key.clone()).or_insert(serde_json::Value::Null),
                value,
            );
        }
    }
}

fn parse_time(time: &str) -> anyhow::Result<Time> {
    Time::strptime("%H:%M", time).with_context(|| format!("invalid time of day {time:?}"))
}

fn same_weekday(weekday: &Weekday, civil: jiff::civil::Weekday) -> bool {
    matches!(
        (weekday, civil),
        (Weekday::Monday, jiff::civil::Weekday::Monday)
            | (Weekday::Tuesday, jiff::civil::Weekday::Tuesday)
            | (Weekday::Wednesday, jiff::civil::Weekday::Wednesday)
            | (Weekday::Thursday, jiff::civil::Weekday::Thursday)
            | (Weekday::Friday, jiff::civil::Weekday::Friday)
            | (Weekday::Saturday, jiff::civil::Weekday::Saturday)
            | (Weekday::Sunday, jiff::civil::Weekday::Sunday)
    )
}

#[cfg(test)]
mod tests {
    use nexigon_api::types::properties::OtaUpdateWindowRange;
    use nexigon_api::types::properties::TimeOfDay;
    use serde_json::json;

    use super::*;

    fn window(ranges: &[(Option<&[Weekday]>, &str, &str)]) -> OtaUpdateWindow {
        let time = |time: &str| -> TimeOfDay { serde_json::from_value(json!(time)).unwrap() };
        OtaUpdateWindow::new(
            ranges
                .iter()
                .map(|(days, start, end)| {
                    OtaUpdateWindowRange::new(time(start), time(end))
                        .with_days(days.map(|days| days.to_vec().into()))
                })
                .collect::<Vec<_>>()
                .into(),
        )
    }

    fn at(datetime: &str) -> Zoned {
        format!("{datetime}[Europe/Berlin]").parse().unwrap()
    }

    /// A weekday range crossing midnight is open after its start and until its
    /// end on the next day, and reports when it opens next while closed.
    #[test]
    fn window_ranges_cross_midnight() {
        let weekdays = [
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
        ];
        let window = window(&[(Some(&weekdays), "22:00", "04:00")]);
        // Friday 2026-10-16.
        assert_eq!(
            window_state(&window, &at("2026-10-16T23:30")).unwrap(),
            WindowState::Open
        );
        // The Friday range ends on Saturday morning.
        assert_eq!(
            window_state(&window, &at("2026-10-17T03:59")).unwrap(),
            WindowState::Open
        );
        // Saturday night has no range, so the next opening is Monday.
        assert_eq!(
            window_state(&window, &at("2026-10-17T12:00")).unwrap(),
            WindowState::Closed {
                opens_at: Some(at("2026-10-19T22:00").timestamp())
            }
        );
    }

    /// A range whose end equals its start spans 24 hours, and an empty window
    /// never opens.
    #[test]
    fn window_full_day_and_empty_window() {
        let weekend = [Weekday::Saturday, Weekday::Sunday];
        let window = window(&[(Some(&weekend), "00:00", "00:00")]);
        assert_eq!(
            window_state(&window, &at("2026-10-18T23:59")).unwrap(),
            WindowState::Open
        );
        assert_eq!(
            window_state(&window, &at("2026-10-19T00:00")).unwrap(),
            WindowState::Closed {
                opens_at: Some(at("2026-10-24T00:00").timestamp())
            }
        );
        assert_eq!(
            window_state(
                &OtaUpdateWindow::new(Vec::new().into()),
                &at("2026-10-19T00:00")
            )
            .unwrap(),
            WindowState::Closed { opens_at: None }
        );
    }

    /// Delays double from the initial delay up to the maximum, and the limit
    /// either ends attempts or waits for the cooldown.
    #[test]
    fn retry_delays_and_limits() {
        let policy = OtaUpdateRetryPolicy::new()
            .with_max_attempts(Some(5))
            .with_initial_delay_secs(Some(900))
            .with_max_delay_secs(Some(3600));
        let delays = (1..=4)
            .map(|failures| retry_delay(&policy, failures).unwrap().as_secs())
            .collect::<Vec<_>>();
        assert_eq!(delays, [900, 1800, 3600, 3600]);
        assert_eq!(retry_delay(&policy, 5), None);
        let policy = policy.with_cooldown_secs(Some(86400));
        assert_eq!(retry_delay(&policy, 5), Some(Duration::from_secs(86400)));
    }

    /// The documented test vector of the spread specification.
    #[test]
    fn spread_test_vector() {
        let position = spread_position("d_5mKpR3vXq8TzLw2NhYc7Bf", "pkg_v_3sLgP9eKd7XwHn2QaRt5Zm");
        assert_eq!(position, 0.8796269976531673);
        let spread = OtaUpdateSpread::new().with_duration_secs(Some(604800));
        let start: Timestamp = "2026-10-12T08:00:00Z".parse().unwrap();
        let released_at: Timestamp = "2026-10-18T21:33:52Z".parse().unwrap();
        assert_eq!(
            spread_state(&spread, Some(start), position, start),
            SpreadState::Waiting {
                released_at: Some(released_at)
            }
        );
        assert_eq!(
            spread_state(&spread, Some(start), position, released_at),
            SpreadState::Released
        );
    }

    /// A percentage releases devices below it right away and caps a spread
    /// over time, leaving the release time unknown for devices above it.
    #[test]
    fn spread_percentage() {
        let start: Timestamp = "2026-10-12T08:00:00Z".parse().unwrap();
        let later: Timestamp = "2026-11-12T08:00:00Z".parse().unwrap();
        let spread = OtaUpdateSpread::new().with_percentage(Some(50.0));
        assert_eq!(
            spread_state(&spread, None, 0.3, start),
            SpreadState::Released
        );
        assert_eq!(
            spread_state(&spread, None, 0.7, start),
            SpreadState::Waiting { released_at: None }
        );
        let capped = spread.with_duration_secs(Some(604800));
        assert_eq!(
            spread_state(&capped, Some(start), 0.7, later),
            SpreadState::Waiting { released_at: None }
        );
        assert_eq!(
            spread_state(&capped, Some(start), 0.3, later),
            SpreadState::Released
        );
    }

    /// Merge patches merge objects recursively, replace other values, and
    /// remove values set to `null`.
    #[test]
    fn merge_patch_semantics() {
        let mut target = json!({
            "path": "repo/pkg/stable",
            "window": { "ranges": [{ "start": "02:00", "end": "05:00" }] },
            "retry": { "maxAttempts": 3, "initialDelaySecs": 60 },
        });
        merge_patch(
            &mut target,
            &json!({ "window": null, "retry": { "maxAttempts": 5 }, "hold": true }),
        );
        assert_eq!(
            target,
            json!({
                "path": "repo/pkg/stable",
                "retry": { "maxAttempts": 5, "initialDelaySecs": 60 },
                "hold": true,
            })
        );
    }
}
