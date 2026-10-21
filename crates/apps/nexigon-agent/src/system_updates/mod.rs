//! System updates through Rugix Ctrl, RAUC, or an updater of the host.
//!
//! When enabled in the agent configuration, the agent implements the OTA update
//! contract of Nexigon Hub: it converges toward the desired state in
//! `dev.nexigon.ota.config` and reports its progress in `dev.nexigon.ota.status`.
//! `settings` reads the integration from the configuration. Hosts running the
//! agent in-process may instead provide [`SystemUpdates`] with their own
//! [`SystemUpdater`] through [`RunOptions`](crate::RunOptions). `run` checks
//! for updates periodically, when a scheduled attempt is due, and when the
//! `nexigon.ota.check` command requests a check. Scheduled checks are delayed
//! randomly, so that devices whose checks are due at the same time, for example
//! when a shared maintenance window opens, do not all reach Nexigon Hub at once.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use futures::FutureExt;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use nexigon_client::ClientExecutor;
use nexigon_ids::ids::DeviceId;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::config::Config;

mod check;
mod planning;
mod policy;
mod updater;

use check::Checker;
use updater::Updater;

pub use updater::Bundle;
pub use updater::SystemIdentity;
pub use updater::SystemUpdater;

/// Device command that requests an immediate check.
pub(crate) const CHECK_COMMAND: &str = "nexigon.ota.check";

/// Default time between update checks.
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
/// Default maximum random delay of scheduled checks.
const DEFAULT_CHECK_JITTER: Duration = Duration::from_secs(5 * 60);
/// Delay before checking again after a check failed, for example because
/// Nexigon Hub was unreachable.
const ERROR_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
/// Shortest time between two scheduled checks.
const MIN_CHECK_DELAY: Duration = Duration::from_secs(10);
/// Time to wait for a requested reboot before checking again.
const REBOOT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// System updates through an updater, with the schedule of their checks.
#[derive(Clone)]
pub struct SystemUpdates {
    updater: Arc<dyn SystemUpdater>,
    /// Version path built into the image.
    default_path: Option<String>,
    /// Asset filename built into the image, or the bundle of the running system
    /// if absent.
    default_asset: Option<String>,
    check_interval: Duration,
    /// Maximum random delay of scheduled checks.
    check_jitter: Duration,
    /// Time zone of maintenance windows, or the system time zone if absent.
    time_zone: Option<TimeZone>,
}

impl SystemUpdates {
    /// System updates through `updater` with the default check schedule.
    pub fn new(updater: Arc<dyn SystemUpdater>) -> Self {
        Self {
            updater,
            default_path: None,
            default_asset: None,
            check_interval: DEFAULT_CHECK_INTERVAL,
            check_jitter: DEFAULT_CHECK_JITTER,
            time_zone: None,
        }
    }

    /// Set the version path that applies without a configured path.
    pub fn with_default_path(mut self, path: Option<String>) -> Self {
        self.default_path = path;
        self
    }

    /// Set the asset filename that applies without a configured asset, instead
    /// of the bundle of the running system.
    pub fn with_default_asset(mut self, asset: Option<String>) -> Self {
        self.default_asset = asset;
        self
    }

    /// Set the time between update checks.
    pub fn with_check_interval(mut self, interval: Duration) -> Self {
        self.check_interval = interval;
        self
    }

    /// Set the maximum random delay of scheduled checks.
    pub fn with_check_jitter(mut self, jitter: Duration) -> Self {
        self.check_jitter = jitter;
        self
    }

    /// Set the time zone in which maintenance windows apply.
    pub fn with_time_zone(mut self, time_zone: TimeZone) -> Self {
        self.time_zone = Some(time_zone);
        self
    }
}

/// Read the enabled system update integration from the agent configuration.
///
/// Fails if more than one integration is enabled.
pub(crate) fn settings(config: &Config) -> anyhow::Result<Option<SystemUpdates>> {
    let integrations = config.integrations.as_ref();
    let rugix = integrations
        .and_then(|integrations| integrations.rugix.as_ref())
        .and_then(|rugix| rugix.system_updates.as_ref())
        .filter(|updates| updates.enabled == Some(true));
    let rauc = integrations
        .and_then(|integrations| integrations.rauc.as_ref())
        .and_then(|rauc| rauc.system_updates.as_ref())
        .filter(|updates| updates.enabled == Some(true));
    let interval = |secs: Option<u64>| secs.map_or(DEFAULT_CHECK_INTERVAL, Duration::from_secs);
    let jitter = |secs: Option<u64>| secs.map_or(DEFAULT_CHECK_JITTER, Duration::from_secs);
    match (rugix, rauc) {
        (Some(_), Some(_)) => bail!("only one system update integration can be enabled"),
        (Some(rugix), None) => Ok(Some(
            SystemUpdates::new(Arc::new(Updater::Rugix {
                use_bundle_hash: rugix.use_bundle_hash.unwrap_or(false),
            }))
            .with_default_path(rugix.path.as_deref().map(str::to_owned))
            .with_default_asset(rugix.asset.as_deref().map(str::to_owned))
            .with_check_interval(interval(rugix.check_interval_secs))
            .with_check_jitter(jitter(rugix.check_jitter_secs)),
        )),
        (None, Some(rauc)) => Ok(Some(
            SystemUpdates::new(Arc::new(Updater::Rauc))
                .with_default_path(rauc.path.as_deref().map(str::to_owned))
                .with_default_asset(rauc.asset.as_deref().map(str::to_owned))
                .with_check_interval(interval(rauc.check_interval_secs))
                .with_check_jitter(jitter(rauc.check_jitter_secs)),
        )),
        (None, None) => Ok(None),
    }
}

/// Check for updates until cancelled.
///
/// The first check runs immediately, so that a newly booted system is committed
/// without delay. `check_requested` wakes the loop for an immediate check.
/// Other checks wait for a random delay of up to the configured jitter.
/// `cancellation` stops the loop between checks and before installations. An installation
/// in progress runs to completion, so the caller must await the loop. After it,
/// the device reboots into the installed version unless `shutdown` signals
/// that the agent stops.
pub(crate) async fn run(
    settings: SystemUpdates,
    executor: ClientExecutor,
    device_id: DeviceId,
    check_requested: Arc<Notify>,
    cancellation: CancellationToken,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let check_interval = settings.check_interval;
    let check_jitter = settings.check_jitter;
    let mut checker = Checker::new(
        settings,
        executor,
        device_id,
        cancellation.clone(),
        shutdown,
    );
    while !cancellation.is_cancelled() {
        let error_delay = ERROR_RETRY_DELAY.min(check_interval).max(MIN_CHECK_DELAY);
        // A panicking check must not end the integration or the agent.
        let delay = match AssertUnwindSafe(checker.check()).catch_unwind().await {
            Ok(Ok(outcome)) if outcome.rebooting => {
                info!("waiting for the reboot into the installed version");
                tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    () = tokio::time::sleep(REBOOT_TIMEOUT) => {
                        warn!("the device did not reboot into the installed version");
                        MIN_CHECK_DELAY
                    }
                }
            }
            Ok(Ok(outcome)) => next_check_delay(outcome.next_attempt_at, check_interval),
            Ok(Err(error)) => {
                warn!(error = format!("{error:#}"), "system update check failed");
                error_delay
            }
            Err(payload) => {
                let panic = crate::run::panic_message(payload);
                error!(panic, "system update check panicked");
                error_delay
            }
        };
        let delay = delay + check_jitter.mul_f64(rand::random::<f64>());
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(()),
            () = check_requested.notified() => info!("update check requested"),
            () = tokio::time::sleep(delay) => {}
        }
    }
    Ok(())
}

/// Time until the next check: the next attempt, if due before the interval.
fn next_check_delay(next_attempt_at: Option<Timestamp>, check_interval: Duration) -> Duration {
    let until_attempt = next_attempt_at.and_then(|at| {
        let seconds = at.duration_since(Timestamp::now()).as_secs_f64();
        Duration::try_from_secs_f64(seconds.max(0.0)).ok()
    });
    until_attempt
        .map_or(check_interval, |until| until.min(check_interval))
        .max(MIN_CHECK_DELAY)
}
