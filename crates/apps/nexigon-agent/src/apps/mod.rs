//! App management through Rugix Apps.
//!
//! When enabled in the agent configuration, the agent implements the app update
//! contract of Nexigon Hub: it converges the apps on the device toward the
//! desired state in `dev.nexigon.apps.config` and reports their status in
//! `dev.nexigon.apps.status`. `settings` reads the integration from the
//! configuration. `run` checks the apps periodically, when a scheduled attempt
//! is due, and when the `nexigon.apps.check` command requests a check. Scheduled
//! checks are delayed randomly, like the checks of system updates.
//!
//! The agent changes apps only while it holds the device lock that it shares
//! with the system update integration, so that a reboot into a new system does
//! not interrupt the installation of an app.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use nexigon_ids::ids::DeviceId;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::config::Config;
use crate::device_api::DeviceExecutor;
use crate::reconcile::DEFAULT_CHECK_INTERVAL;
use crate::reconcile::DEFAULT_CHECK_JITTER;
use crate::reconcile::error_delay;
use crate::reconcile::next_check_delay;
use crate::reconcile::random_delay;

mod check;
mod rugix;

use check::Checker;

/// Device command that requests an immediate check.
pub(crate) const CHECK_COMMAND: &str = "nexigon.apps.check";
/// Number of previously active generations that garbage collection keeps by
/// default, like Rugix Ctrl.
const DEFAULT_GC_KEEP: u32 = 1;

/// App management settings from the agent configuration.
#[derive(Debug, Clone)]
pub(crate) struct AppUpdates {
    /// Architecture that selects the bundles the device installs.
    arch: String,
    check_interval: Duration,
    /// Maximum random delay of scheduled checks.
    check_jitter: Duration,
    /// Pass the bundle hash from the asset metadata to Rugix Ctrl.
    use_bundle_hash: bool,
    /// Number of previously active generations to keep, unless the `gc`
    /// policy sets it.
    gc_keep: u32,
}

/// Read the app management settings from the agent configuration, if enabled.
pub(crate) fn settings(config: &Config) -> Option<AppUpdates> {
    let apps = config
        .integrations
        .as_ref()?
        .rugix
        .as_ref()?
        .apps
        .as_ref()
        .filter(|apps| apps.enabled == Some(true))?;
    Some(AppUpdates {
        arch: apps
            .arch
            .as_deref()
            .unwrap_or(std::env::consts::ARCH)
            .to_owned(),
        check_interval: apps
            .check_interval_secs
            .map_or(DEFAULT_CHECK_INTERVAL, Duration::from_secs),
        check_jitter: apps
            .check_jitter_secs
            .map_or(DEFAULT_CHECK_JITTER, Duration::from_secs),
        use_bundle_hash: apps.use_bundle_hash.unwrap_or(false),
        gc_keep: apps
            .gc
            .as_ref()
            .and_then(|gc| gc.keep)
            .unwrap_or(DEFAULT_GC_KEEP),
    })
}

/// Check the apps until cancelled.
///
/// The first check runs immediately. `check_requested` wakes the loop for an
/// immediate check. Other checks wait for a random delay of up to the configured
/// jitter. `cancellation` stops the loop between checks and before changes of
/// the device. A change in progress runs to completion, so the caller must
/// await the loop.
pub(crate) async fn run(
    settings: AppUpdates,
    executor: DeviceExecutor,
    device_id: DeviceId,
    device_lock: Arc<Mutex<()>>,
    check_requested: Arc<Notify>,
    cancellation: CancellationToken,
) -> anyhow::Result<()> {
    let check_interval = settings.check_interval;
    let check_jitter = settings.check_jitter;
    let mut checker = Checker::new(
        settings,
        executor,
        device_id,
        device_lock,
        cancellation.clone(),
    );
    while !cancellation.is_cancelled() {
        // A panicking check must not end the integration or the agent.
        let delay = match AssertUnwindSafe(checker.check()).catch_unwind().await {
            Ok(Ok(next_attempt_at)) => next_check_delay(next_attempt_at, check_interval),
            Ok(Err(error)) => {
                warn!(error = format!("{error:#}"), "app check failed");
                error_delay(check_interval)
            }
            Err(payload) => {
                let panic = crate::run::panic_message(payload);
                error!(panic, "app check panicked");
                error_delay(check_interval)
            }
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(()),
            () = check_requested.notified() => info!("app check requested"),
            () = tokio::time::sleep(delay + random_delay(check_jitter)) => {}
        }
    }
    Ok(())
}
