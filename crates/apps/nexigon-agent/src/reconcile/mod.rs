//! Building blocks of the reconciliation loops of the agent's integrations.
//!
//! The system update and app integrations converge the device toward a desired
//! state in a device property and report the actual state in another one.
//! [`hub::Hub`] reads and writes these properties and the package versions they
//! refer to, [`policy`] decides when an attempt may start, and [`process`] runs
//! the tools that change the device. The functions of this module schedule the
//! checks of a loop. The loops run for the lifetime of the agent and reach
//! Nexigon Hub through the agent's [`HubLink`], so a failed check is retried
//! soon after the agent reconnects.

use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use nexigon_api::types::properties::OtaUpdateRetryPolicy;
use nexigon_api::types::properties::OtaUpdateWindow;

use crate::hub_link::HubLink;

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

/// Wait until the next scheduled check, or until the next check after a failed
/// one if `delay` is `None`.
///
/// A failed check has most likely failed because the agent was disconnected.
/// A session that starts after the first `sessions` sessions therefore ends the
/// wait early, after a random delay of up to `jitter`.
pub(crate) async fn wait_for_check(
    link: &HubLink,
    sessions: u64,
    delay: Option<Duration>,
    check_interval: Duration,
    jitter: Duration,
) {
    let Some(delay) = delay else {
        let reconnected = async {
            link.connected_after(sessions).await;
            tokio::time::sleep(random_delay(jitter)).await;
        };
        tokio::select! {
            () = tokio::time::sleep(error_delay(check_interval) + random_delay(jitter)) => {}
            () = reconnected => {}
        }
        return;
    };
    tokio::time::sleep(delay + random_delay(jitter)).await;
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use nexigon_ids::Generate;
    use nexigon_ids::ids::DeviceId;
    use nexigon_multiplex::Connection;
    use nexigon_multiplex::transport::InMemory;

    use super::wait_for_check;
    use crate::hub_link::hub_link;

    /// After a failed check, a new session ends the wait before the error delay.
    #[tokio::test(start_paused = true)]
    async fn failed_check_is_retried_after_reconnecting() {
        let (controller, link) = hub_link();
        let wait = tokio::spawn({
            let link = link.clone();
            async move {
                wait_for_check(&link, 0, None, Duration::from_secs(3600), Duration::ZERO).await;
            }
        });
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(!wait.is_finished());

        let (_hub_transport, agent_transport) = InMemory::<Bytes, Bytes>::new_buffered(1);
        let connection = Connection::new(agent_transport);
        controller.connected(DeviceId::generate(), connection.make_ref());
        tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .expect("the wait did not end after reconnecting")
            .unwrap();
    }
}
