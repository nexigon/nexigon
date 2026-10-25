//! Building blocks of the reconciliation loops of the agent's integrations.
//!
//! The system update and app integrations converge the device toward a desired
//! state in a device property and report the actual state in another one.
//! [`hub::Hub`] reads and writes these properties and the package versions they
//! refer to, [`policy`] decides when an attempt may start, and [`process`] runs
//! the tools that change the device. The functions of this module schedule the
//! checks of a loop.

use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use nexigon_api::types::properties::OtaUpdateRetryPolicy;
use nexigon_api::types::properties::OtaUpdateWindow;

pub(crate) mod hub;
pub(crate) mod policy;
pub(crate) mod process;

use policy::WindowState;
use policy::window_state;

/// Default time between checks.
pub(crate) const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
/// Default maximum random delay of scheduled checks.
pub(crate) const DEFAULT_CHECK_JITTER: Duration = Duration::from_secs(5 * 60);
/// Delay before checking again after a check failed, for example because
/// Nexigon Hub was unreachable.
const ERROR_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
/// Shortest time between two scheduled checks.
pub(crate) const MIN_CHECK_DELAY: Duration = Duration::from_secs(10);
/// Time until checking whether the clock has been synchronized.
const CLOCK_RECHECK_DELAY: Duration = Duration::from_secs(60);

/// Time until the next check after a failed one.
pub(crate) fn error_delay(check_interval: Duration) -> Duration {
    ERROR_RETRY_DELAY.min(check_interval).max(MIN_CHECK_DELAY)
}

/// Time until the next check: the next attempt, if due before the interval.
pub(crate) fn next_check_delay(
    next_attempt_at: Option<Timestamp>,
    check_interval: Duration,
) -> Duration {
    let until_attempt = next_attempt_at.and_then(|at| {
        let seconds = at.duration_since(Timestamp::now()).as_secs_f64();
        Duration::try_from_secs_f64(seconds.max(0.0)).ok()
    });
    until_attempt
        .map_or(check_interval, |until| until.min(check_interval))
        .max(MIN_CHECK_DELAY)
}

/// Random delay of up to `jitter`.
pub(crate) fn random_delay(jitter: Duration) -> Duration {
    jitter.mul_f64(rand::random::<f64>())
}

/// Whether a maintenance window allows an activation now.
pub(crate) enum WindowGate {
    Open,
    /// The window is closed and opens at the given time, if ever.
    Closed {
        opens_at: Option<Timestamp>,
    },
    /// The device clock is not synchronized, so the window cannot be evaluated
    /// until the given time.
    Unsynchronized {
        recheck_at: Option<Timestamp>,
    },
    /// The window is invalid.
    Invalid(anyhow::Error),
}

/// Evaluate a maintenance window now, in the given time zone.
pub(crate) fn window_gate(window: &OtaUpdateWindow, time_zone: TimeZone) -> WindowGate {
    // The clock usually synchronizes shortly after boot.
    if !clock_synchronized() {
        return WindowGate::Unsynchronized {
            recheck_at: Timestamp::now().checked_add(CLOCK_RECHECK_DELAY).ok(),
        };
    }
    match window_state(window, &Timestamp::now().to_zoned(time_zone)) {
        Ok(WindowState::Open) => WindowGate::Open,
        Ok(WindowState::Closed { opens_at }) => WindowGate::Closed { opens_at },
        Err(error) => WindowGate::Invalid(error),
    }
}

/// Earliest time of the next attempt after the given number of failures.
pub(crate) fn next_retry(
    retry: &OtaUpdateRetryPolicy,
    failures: u32,
    now: Timestamp,
) -> Option<Timestamp> {
    let delay = policy::retry_delay(retry, failures)?;
    now.checked_add(delay).ok()
}

/// Whether the kernel considers the system clock synchronized.
#[cfg(target_os = "linux")]
fn clock_synchronized() -> bool {
    // SAFETY: `adjtimex` with zero `modes` only reads the kernel clock state
    // into the provided, fully initialized structure.
    let mut timex: libc::timex = unsafe { std::mem::zeroed() };
    let state = unsafe { libc::adjtimex(&mut timex) };
    state != libc::TIME_ERROR && timex.status & libc::STA_UNSYNC == 0
}

#[cfg(not(target_os = "linux"))]
fn clock_synchronized() -> bool {
    true
}
