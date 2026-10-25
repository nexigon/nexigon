//! One check of the system update reconciliation loop.
//!
//! A check reads the desired state from `dev.nexigon.ota.config`, finishes an
//! update in progress, plans the next version, applies the hold, spread, retry,
//! and maintenance window policies, and installs the configured asset of the
//! next version if all of them allow it. It publishes the actual state in
//! `dev.nexigon.ota.status`.
//!
//! The status published by the previous check is the only state that persists
//! across checks and reboots. Nexigon Hub stores it, so it survives switching
//! between system partitions.
//!
//! Status writes carry the version read last (compare-and-swap), so that a
//! concurrent change, such as a cleared failure count, is not overwritten. A
//! check that loses such a race starts over from the changed status. Once an
//! installation has started or a booted system awaits its commit, the check
//! writes its status regardless, because the next check needs it to detect the
//! outcome of the update.

use std::sync::Arc;

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use nexigon_api::types::devices::DeviceEventSeverity;
use nexigon_api::types::devices::DeviceProperty;
use nexigon_api::types::properties::OtaUpdateBlocked;
use nexigon_api::types::properties::OtaUpdateCapability;
use nexigon_api::types::properties::OtaUpdateConfig;
use nexigon_api::types::properties::OtaUpdateRetryPolicy;
use nexigon_api::types::properties::OtaUpdateState;
use nexigon_api::types::properties::OtaUpdateStatus;
use nexigon_api::types::properties::OtaUpdateWindow;
use nexigon_api::types::properties::OtaUpdater;
use nexigon_api::types::repositories::GetPackageVersionDetailsOutput;
use nexigon_api::types::repositories::PackageVersionAsset;
use nexigon_api::types::repositories::PackageVersionId;
use nexigon_ids::ids::DeviceId;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing::warn;

use super::SystemUpdates;
use super::planning::ChainEntry;
use super::planning::NextVersion;
use super::planning::Running;
use super::planning::next_version;
use super::updater::Bundle;
use super::updater::SystemIdentity;
use crate::device_api::DeviceExecutor;
use crate::reconcile::WindowGate;
use crate::reconcile::hub::Hub;
use crate::reconcile::hub::PropertyWrite;
use crate::reconcile::hub::VersionPath;
use crate::reconcile::hub::metadata;
use crate::reconcile::hub::rugix_bundle_hash;
use crate::reconcile::next_retry;
use crate::reconcile::policy::SpreadState;
use crate::reconcile::policy::merge_patch;
use crate::reconcile::policy::retry_policy_with_defaults;
use crate::reconcile::policy::spread_position;
use crate::reconcile::policy::spread_state;
use crate::reconcile::window_gate;

/// Device property holding the desired state.
pub(crate) const CONFIG_PROPERTY: &str = "dev.nexigon.ota.config";
/// Device property holding the actual state.
pub(crate) const STATUS_PROPERTY: &str = "dev.nexigon.ota.status";
/// Event category of update events.
const EVENT_CATEGORY: &str = "ota";
/// Number of times a check starts over after the status changed concurrently.
const MAX_STATUS_CONFLICTS: usize = 3;

/// Optional features of the OTA update contract that the agent implements.
const CAPABILITIES: [OtaUpdateCapability; 6] = [
    OtaUpdateCapability::Hold,
    OtaUpdateCapability::Asset,
    OtaUpdateCapability::Window,
    OtaUpdateCapability::Retry,
    OtaUpdateCapability::Spread,
    OtaUpdateCapability::Checkpoints,
];

/// Result of a check.
pub(crate) struct CheckOutcome {
    /// Earliest time of the next attempt, if the device waits for one.
    pub next_attempt_at: Option<Timestamp>,
    /// Whether the device is rebooting into an installed version.
    ///
    /// No further check may run before the reboot, as it would mistake the
    /// still running version for a rollback.
    pub rebooting: bool,
}

/// Executes checks against Nexigon Hub for one device.
pub(crate) struct Checker {
    settings: SystemUpdates,
    hub: Hub,
    /// Signals a lost connection or a shutdown, which prevents new installations.
    cancellation: CancellationToken,
    /// Signals a shutdown, which also prevents rebooting into an installed version.
    shutdown: CancellationToken,
    /// Serializes changes of the device with the app integration.
    device_lock: Arc<Mutex<()>>,
    /// Keeps the app integration from changing the device until the reboot
    /// into an installed version.
    reboot_guard: Option<OwnedMutexGuard<()>>,
    /// Version of the status read or written last, or `None` without a status.
    status_version: Option<u64>,
    /// Whether this check started an installation.
    installing: bool,
    /// Whether this check reported the outcome of the previous attempt, which it
    /// does only once, even if it starts over.
    outcome_reported: bool,
}

impl Checker {
    pub(crate) fn new(
        settings: SystemUpdates,
        executor: DeviceExecutor,
        device_id: DeviceId,
        device_lock: Arc<Mutex<()>>,
        cancellation: CancellationToken,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            settings,
            hub: Hub::new(executor, device_id),
            cancellation,
            shutdown,
            device_lock,
            reboot_guard: None,
            status_version: None,
            installing: false,
            outcome_reported: false,
        }
    }

    /// Run one check and publish the resulting status.
    ///
    /// Starts over if the status changes concurrently.
    #[tracing::instrument(skip_all)]
    pub(crate) async fn check(&mut self) -> anyhow::Result<CheckOutcome> {
        self.outcome_reported = false;
        self.reboot_guard = None;
        for _ in 0..MAX_STATUS_CONFLICTS {
            match self.check_once().await {
                Err(error) if error.is::<StatusConflict>() => {
                    info!("the update status changed during the check, starting over");
                }
                outcome => return outcome,
            }
        }
        self.check_once().await
    }

    async fn check_once(&mut self) -> anyhow::Result<CheckOutcome> {
        // Versions may be deleted or rebased between checks.
        self.hub.clear_cache();
        self.installing = false;
        let now = Timestamp::now();
        let previous = self.previous_status().await?;
        let property = self.hub.property(CONFIG_PROPERTY).await?;
        let config_version = property.as_ref().map(|property| property.version);
        let identity = self.settings.updater.identity().await?;
        let mut status = OtaUpdateStatus::new()
            .with_capabilities(Some(CAPABILITIES.to_vec().into()))
            .with_config_version(config_version)
            .with_current_version(Some(identity.version.clone().into()))
            .with_updater(self.updater_info().await);

        let config = effective_config(
            self.defaults(),
            &self.settings.updater.bundle_filename(&identity.name),
            property.as_ref(),
        );
        let retry = retry_policy_with_defaults(
            config
                .as_ref()
                .ok()
                .and_then(|config| config.retry.as_ref()),
        );

        // Committing is never restricted, not even by an invalid configuration.
        if self.settings.updater.awaits_commit().await? {
            let committing = status
                .clone()
                .with_state(Some(OtaUpdateState::Committing))
                .with_target_version_id(previous.target_version_id.clone())
                .with_next_version_id(previous.next_version_id.clone());
            self.publish(&committing).await?;
            if let Err(error) = self.settings.updater.commit().await {
                // The device stays uncommitted and commits again on the next check.
                let error = format!("failed to commit the booted system: {error:#}");
                warn!(%error, "update failed");
                self.event(
                    DeviceEventSeverity::Error,
                    "update failed",
                    &identity,
                    Some(&error),
                )
                .await;
                return self
                    .finish(
                        committing
                            .with_last_error(Some(error.into()))
                            .with_next_attempt_at(next_retry(&retry, 1, now).map(Into::into)),
                    )
                    .await;
            }
            info!("committed the booted system");
            status.updater = self.updater_info().await;
        }

        let config = match config {
            Ok(config) => config,
            Err(error) => return self.finish(failed(status, &error)).await,
        };
        status.config = Some(config.clone().with_retry(Some(retry.clone())));

        let Some(path) = config.path.as_deref() else {
            return self
                .finish(status.with_state(Some(OtaUpdateState::Idle)))
                .await;
        };
        let path = match VersionPath::parse(path) {
            Ok(path) => path,
            Err(error) => return self.finish(failed(status, &error)).await,
        };
        let running_id = self
            .hub
            .resolve(&path.with_version(&identity.version))
            .await?;
        status.current_version_id = running_id.clone();

        let mut completed = false;
        let mut failure = None;
        if let Some(
            state @ (OtaUpdateState::Installing
            | OtaUpdateState::Rebooting
            | OtaUpdateState::Committing),
        ) = &previous.state
        {
            if self
                .runs_installed_version(&previous, running_id.as_ref(), &identity)
                .await?
            {
                if !self.outcome_reported {
                    info!(version = %identity.version, "update completed");
                    self.event(
                        DeviceEventSeverity::Info,
                        "update completed",
                        &identity,
                        None,
                    )
                    .await;
                    self.outcome_reported = true;
                }
                completed = true;
            } else if matches!(state, OtaUpdateState::Installing) {
                failure = Some("the installation was interrupted".to_owned());
            } else {
                failure = Some(format!(
                    "the update was rolled back: the device runs {} instead of the installed version",
                    identity.version
                ));
            }
        }
        if let Some(error) = &failure {
            if !self.outcome_reported {
                warn!(%error, "update failed");
                self.event(
                    DeviceEventSeverity::Error,
                    "update failed",
                    &identity,
                    Some(error),
                )
                .await;
                self.outcome_reported = true;
            }
            status.last_error = Some(error.clone().into());
        }

        let Some(target_id) = self.hub.resolve(&path).await? else {
            let error = anyhow!("version {path} does not exist");
            return self.finish(failed(status, &error)).await;
        };
        let target = self.hub.version_details(&target_id).await?.clone();
        let target_version = image_version(&target);
        status.target_version = target_version.clone().map(Into::into);
        status.target_version_id = Some(target_id.clone());

        let up_to_date = running_id.as_ref() == Some(&target_id)
            || target_version.as_deref() == Some(identity.version.as_str());
        if up_to_date {
            return self
                .finish(
                    status
                        .with_state(Some(OtaUpdateState::Completed))
                        .with_failure_count(Some(0))
                        .with_last_error(None),
                )
                .await;
        }

        let next = match self.plan(running_id.as_ref(), &target).await? {
            NextVersion::Install(next) => next,
            NextVersion::NoUpdatePath => {
                return self.finish(blocked(status, "no-update-path", None)).await;
            }
            NextVersion::UnknownCurrentVersion => {
                return self
                    .finish(blocked(status, "unknown-current-version", None))
                    .await;
            }
        };
        status.next_version_id = Some(next.clone());

        // The failure count belongs to the next version and the configuration.
        let same_attempt = previous.next_version_id.as_ref() == Some(&next)
            && previous.config_version == config_version;
        let mut failures = if same_attempt && !completed {
            previous.failure_count.unwrap_or(0)
        } else {
            0
        };
        if failure.is_some() && same_attempt {
            failures += 1;
        } else if same_attempt && !completed {
            // The error of the last failed attempt stays until the attempt succeeds.
            status.last_error = previous.last_error.clone();
        }
        let limit_reached = retry
            .max_attempts
            .is_some_and(|max_attempts| failures >= max_attempts);
        let retry_at = if failure.is_some() && same_attempt {
            next_retry(&retry, failures, now)
        } else if same_attempt && matches!(previous.state, Some(OtaUpdateState::Failed)) {
            // Without a scheduled attempt, only a reached limit stops attempts.
            match previous.next_attempt_at {
                Some(at) => Some(at.into()),
                None if limit_reached => None,
                None => Some(now),
            }
        } else {
            Some(now)
        };
        status.failure_count = Some(failures);

        if config.hold == Some(true) {
            return self.finish(blocked(status, "hold", None)).await;
        }

        if let Some(spread) = &config.spread {
            let start = spread.start_at.map(Into::into).or_else(|| {
                target
                    .tags
                    .iter()
                    .find(|tag| *tag.tag == *path.version)
                    .and_then(|tag| tag.assigned_at)
                    .map(Into::into)
            });
            let position =
                spread_position(&self.hub.device_id().to_string(), &target_id.to_string());
            if let SpreadState::Waiting { released_at } = spread_state(spread, start, position, now)
            {
                return self.finish(blocked(status, "spread", released_at)).await;
            }
        }

        match retry_at {
            None => {
                return self
                    .finish(status.with_state(Some(OtaUpdateState::Failed)))
                    .await;
            }
            Some(retry_at) if now < retry_at => {
                return self
                    .finish(
                        status
                            .with_state(Some(OtaUpdateState::Failed))
                            .with_next_attempt_at(Some(retry_at.into())),
                    )
                    .await;
            }
            Some(_) if limit_reached => {
                // The cooldown has ended, so a new round of attempts begins.
                failures = 0;
                status.failure_count = Some(0);
            }
            Some(_) => {}
        }

        // A missing asset blocks only an attempt that is due, so that the
        // failure count and the retry schedule persist while it is missing.
        let filename = config
            .asset
            .as_deref()
            .expect("the effective configuration names an asset");
        let Some(asset) = self.hub.available_asset(&next, filename).await? else {
            let blocked = OtaUpdateBlocked::new("no-asset").with_message(Some(
                format!("The next version has no available asset {filename}.").into(),
            ));
            return self
                .finish(
                    status
                        .with_state(Some(OtaUpdateState::Blocked))
                        .with_blocked(Some(blocked)),
                )
                .await;
        };

        if let Some(deferral) = window_closed(&status, config.window.as_ref(), self.time_zone()) {
            return self.finish_deferred(deferral).await;
        }

        self.attempt(status, &identity, &next, &asset, &retry, failures)
            .await
    }

    /// Install and activate the next version.
    ///
    /// Once the installation starts, it runs to completion even if the
    /// connection is lost or updates are put on hold, so that the attempt is not
    /// wasted. If the agent is stopped, it does not reboot into the installed
    /// version.
    async fn attempt(
        &mut self,
        status: OtaUpdateStatus,
        identity: &SystemIdentity,
        next: &PackageVersionId,
        asset: &PackageVersionAsset,
        retry: &OtaUpdateRetryPolicy,
        failures: u32,
    ) -> anyhow::Result<CheckOutcome> {
        // The app integration may hold the lock for a while, during which the
        // agent may be stopped and a download URL would expire.
        let device_guard = self.device_lock.clone().lock_owned().await;
        if self.cancellation.is_cancelled() {
            bail!("the agent is stopping");
        }
        let details = self.hub.version_details(next).await?.clone();
        let url = self.hub.download_url(asset).await?;
        let bundle_hash = rugix_bundle_hash(asset);

        info!(version = %next, "installing update");
        self.publish(&status.clone().with_state(Some(OtaUpdateState::Installing)))
            .await?;
        self.installing = true;
        self.event(
            DeviceEventSeverity::Info,
            "installing update",
            identity,
            None,
        )
        .await;
        let image_version = image_version(&details);
        let installation = self.settings.updater.install(Bundle {
            url: &url,
            hash: bundle_hash.as_deref(),
            image_version: image_version.as_deref(),
        });
        if let Err(error) = installation.await {
            return self
                .fail_attempt(status, identity, retry, failures, error)
                .await;
        }
        if self.shutdown.is_cancelled() {
            self.settings.updater.deactivate().await?;
            bail!("the agent is stopping");
        }

        // The window applies to the activation, which may start long after the
        // attempt. A hold does not stop an update in progress.
        let window = match self.config(identity).await {
            Ok(config) => config.window,
            Err(error) => {
                warn!(
                    error = format!("{error:#}"),
                    "using the update configuration read before the installation"
                );
                status
                    .config
                    .as_ref()
                    .and_then(|config| config.window.clone())
            }
        };
        if let Some(deferral) = window_closed(&status, window.as_ref(), self.time_zone()) {
            self.settings.updater.deactivate().await?;
            return self.finish_deferred(deferral).await;
        }

        info!("rebooting into the installed version");
        // Without this status, the next check still detects the outcome from
        // the running version.
        if let Err(error) = self
            .publish(&status.clone().with_state(Some(OtaUpdateState::Rebooting)))
            .await
        {
            warn!(error = format!("{error:#}"), "failed to report the reboot");
        }
        self.event(
            DeviceEventSeverity::Info,
            "rebooting into the installed version",
            identity,
            None,
        )
        .await;
        if let Err(error) = self.settings.updater.reboot().await {
            // A later, unrelated reboot must not activate the update.
            self.settings.updater.deactivate().await?;
            return self
                .fail_attempt(status, identity, retry, failures, error)
                .await;
        }
        self.reboot_guard = Some(device_guard);
        Ok(CheckOutcome {
            next_attempt_at: None,
            rebooting: true,
        })
    }

    /// Record a failed attempt and schedule the next one.
    async fn fail_attempt(
        &mut self,
        status: OtaUpdateStatus,
        identity: &SystemIdentity,
        retry: &OtaUpdateRetryPolicy,
        failures: u32,
        error: anyhow::Error,
    ) -> anyhow::Result<CheckOutcome> {
        let error = format!("{error:#}");
        warn!(%error, "update failed");
        self.event(
            DeviceEventSeverity::Error,
            "update failed",
            identity,
            Some(&error),
        )
        .await;
        let failures = failures + 1;
        self.finish(
            status
                .with_state(Some(OtaUpdateState::Failed))
                .with_failure_count(Some(failures))
                .with_last_error(Some(error.into()))
                .with_next_attempt_at(
                    next_retry(retry, failures, Timestamp::now()).map(Into::into),
                ),
        )
        .await
    }

    /// Decide which version to install next, fetching the chain of bases.
    async fn plan(
        &mut self,
        running_id: Option<&PackageVersionId>,
        target: &GetPackageVersionDetailsOutput,
    ) -> anyhow::Result<NextVersion> {
        let mut chain = vec![ChainEntry {
            version_id: target.version_id.clone(),
            base_version_id: target.base_version_id.clone(),
        }];
        while let Some(base) = chain.last().and_then(|entry| entry.base_version_id.clone()) {
            if chain.iter().any(|entry| entry.version_id == base) {
                bail!("the base versions of {} form a cycle", target.version_id);
            }
            let details = self.hub.version_details(&base).await?;
            chain.push(ChainEntry {
                version_id: base,
                base_version_id: details.base_version_id.clone(),
            });
        }
        let running = match running_id {
            Some(running_id) => Some(Running {
                version_id: running_id.clone(),
                base_version_id: self
                    .hub
                    .version_details(running_id)
                    .await?
                    .base_version_id
                    .clone(),
            }),
            None => None,
        };
        Ok(next_version(running.as_ref(), &chain))
    }

    /// Whether the device runs the version it installed before rebooting.
    async fn runs_installed_version(
        &mut self,
        previous: &OtaUpdateStatus,
        running_id: Option<&PackageVersionId>,
        identity: &SystemIdentity,
    ) -> anyhow::Result<bool> {
        let Some(installed) = &previous.next_version_id else {
            // Status written by integrations without checkpoint support.
            return Ok(previous.target_version.as_deref() == Some(identity.version.as_str()));
        };
        if running_id == Some(installed) {
            return Ok(true);
        }
        let details = self.hub.version_details(installed).await?;
        Ok(
            image_version(details).as_deref() == Some(identity.version.as_str())
                || details.tags.iter().any(|tag| *tag.tag == identity.version),
        )
    }

    async fn previous_status(&mut self) -> anyhow::Result<OtaUpdateStatus> {
        let property = self.hub.property(STATUS_PROPERTY).await?;
        self.status_version = property.as_ref().map(|property| property.version);
        let Some(property) = property else {
            return Ok(OtaUpdateStatus::new());
        };
        let value = serde_json::to_value(&property.value)?;
        Ok(serde_json::from_value(value).unwrap_or_else(|error| {
            warn!(%error, "ignoring an invalid previous status");
            OtaUpdateStatus::new()
        }))
    }

    /// Read the desired state applied to the defaults of the agent configuration.
    async fn config(&mut self, identity: &SystemIdentity) -> anyhow::Result<OtaUpdateConfig> {
        let property = self.hub.property(CONFIG_PROPERTY).await?;
        effective_config(
            self.defaults(),
            &self.settings.updater.bundle_filename(&identity.name),
            property.as_ref(),
        )
    }

    /// Defaults of the agent configuration.
    fn defaults(&self) -> OtaUpdateConfig {
        OtaUpdateConfig::new()
            .with_path(self.settings.default_path.clone().map(Into::into))
            .with_asset(self.settings.default_asset.clone().map(Into::into))
    }

    /// Time zone in which maintenance windows apply.
    fn time_zone(&self) -> TimeZone {
        self.settings
            .time_zone
            .clone()
            .unwrap_or_else(TimeZone::system)
    }

    /// Describe the update system, logging failures instead of failing the check.
    async fn updater_info(&self) -> Option<OtaUpdater> {
        self.settings
            .updater
            .info()
            .await
            .inspect_err(|error| {
                warn!(
                    error = format!("{error:#}"),
                    "failed to describe the update system"
                );
            })
            .ok()
    }

    /// Publish a status and report when the next attempt is due.
    async fn finish(&mut self, status: OtaUpdateStatus) -> anyhow::Result<CheckOutcome> {
        self.publish(&status).await?;
        Ok(CheckOutcome {
            next_attempt_at: status.next_attempt_at.map(Into::into),
            rebooting: false,
        })
    }

    /// Publish the status of a deferred update and report when to check again.
    async fn finish_deferred(&mut self, deferral: Deferral) -> anyhow::Result<CheckOutcome> {
        let mut outcome = self.finish(deferral.status).await?;
        outcome.next_attempt_at = outcome.next_attempt_at.or(deferral.recheck_at);
        Ok(outcome)
    }

    /// Publish a status unless the status changed since it was read.
    ///
    /// Fails with [`StatusConflict`] if it changed, unless this check started an
    /// installation or the status awaits a commit. Then the status replaces the
    /// changed one.
    async fn publish(&mut self, status: &OtaUpdateStatus) -> anyhow::Result<()> {
        let write = match self
            .hub
            .set_property(STATUS_PROPERTY, status, self.status_version)
            .await?
        {
            PropertyWrite::Conflict
                if self.installing || matches!(status.state, Some(OtaUpdateState::Committing)) =>
            {
                self.hub.set_property(STATUS_PROPERTY, status, None).await?
            }
            PropertyWrite::Conflict => return Err(StatusConflict.into()),
            write => write,
        };
        let PropertyWrite::Written { version } = write else {
            bail!("Nexigon Hub did not store the update status");
        };
        // Without a reported version, the next write of this check is
        // unconditional.
        self.status_version = version;
        Ok(())
    }

    /// Emit an update event, logging failures instead of interrupting the update.
    async fn event(
        &mut self,
        severity: DeviceEventSeverity,
        message: &str,
        identity: &SystemIdentity,
        error: Option<&str>,
    ) {
        let mut attributes = sidex::types::Map::new();
        attributes.insert("currentVersion".into(), identity.version.clone().into());
        if let Some(error) = error {
            attributes.insert("error".into(), error.into());
        }
        self.hub
            .event(EVENT_CATEGORY, severity, message, attributes)
            .await;
    }
}

/// The status changed since the check read it.
#[derive(Debug, thiserror::Error)]
#[error("the update status changed concurrently")]
struct StatusConflict;

/// Apply the configuration property to the defaults of the agent configuration.
///
/// Without a configured asset, the bundle of the running system applies.
fn effective_config(
    defaults: OtaUpdateConfig,
    running_bundle: &str,
    property: Option<&DeviceProperty>,
) -> anyhow::Result<OtaUpdateConfig> {
    let mut config = serde_json::to_value(defaults)?;
    if let Some(property) = property {
        let patch = serde_json::to_value(&property.value)?;
        merge_patch(&mut config, &patch);
    }
    let mut config: OtaUpdateConfig =
        serde_json::from_value(config).context("invalid update configuration")?;
    config
        .asset
        .get_or_insert_with(|| running_bundle.to_owned().into());
    Ok(config)
}

/// Metadata of a package version.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionMetadata {
    /// Version string built into the image of the version.
    image_version: Option<String>,
}

/// Version string built into the image of a version.
fn image_version(details: &GetPackageVersionDetailsOutput) -> Option<String> {
    metadata::<VersionMetadata>(&details.metadata)?.image_version
}

/// Status of an update that may not be activated yet.
struct Deferral {
    status: OtaUpdateStatus,
    /// When to check again if the status names no next attempt.
    recheck_at: Option<Timestamp>,
}

/// Defer an update while the maintenance window is closed.
fn window_closed(
    status: &OtaUpdateStatus,
    window: Option<&OtaUpdateWindow>,
    time_zone: TimeZone,
) -> Option<Deferral> {
    let status = match window_gate(window?, time_zone) {
        WindowGate::Open => return None,
        WindowGate::Unsynchronized { recheck_at } => {
            return Some(Deferral {
                status: status
                    .clone()
                    .with_state(Some(OtaUpdateState::Blocked))
                    .with_blocked(Some(
                        OtaUpdateBlocked::new("window")
                            .with_message(Some("The device clock is not synchronized.".into())),
                    )),
                recheck_at,
            });
        }
        WindowGate::Closed { opens_at } => blocked(status.clone(), "window", opens_at),
        WindowGate::Invalid(error) => status
            .clone()
            .with_state(Some(OtaUpdateState::Failed))
            .with_last_error(Some(
                format!("invalid maintenance window: {error:#}").into(),
            )),
    };
    Some(Deferral {
        status,
        recheck_at: None,
    })
}

fn blocked(
    status: OtaUpdateStatus,
    code: &str,
    next_attempt_at: Option<Timestamp>,
) -> OtaUpdateStatus {
    status
        .with_state(Some(OtaUpdateState::Blocked))
        .with_blocked(Some(OtaUpdateBlocked::new(code)))
        .with_next_attempt_at(next_attempt_at.map(Into::into))
}

/// Record an update that cannot proceed, for example because of an invalid
/// configuration.
fn failed(status: OtaUpdateStatus, error: &anyhow::Error) -> OtaUpdateStatus {
    let error = format!("{error:#}");
    warn!(%error, "update failed");
    status
        .with_state(Some(OtaUpdateState::Failed))
        .with_last_error(Some(error.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property overrides the asset of the agent configuration, and
    /// removing the asset with `null` selects the bundle of the running system.
    #[test]
    fn effective_asset() {
        let asset = |defaults: OtaUpdateConfig, patch: Option<&str>| {
            let property = patch.map(config_property);
            effective_config(defaults, "running.rugixb", property.as_ref())
                .unwrap()
                .asset
                .unwrap()
                .to_string()
        };
        let image = OtaUpdateConfig::new().with_asset(Some("image.rugixb".into()));
        assert_eq!(asset(OtaUpdateConfig::new(), None), "running.rugixb");
        assert_eq!(asset(image.clone(), None), "image.rugixb");
        assert_eq!(
            asset(image.clone(), Some(r#"{"asset":"other.rugixb"}"#)),
            "other.rugixb"
        );
        assert_eq!(asset(image, Some(r#"{"asset":null}"#)), "running.rugixb");
    }

    fn config_property(value: &str) -> DeviceProperty {
        serde_json::from_str(&format!(
            r#"{{"name":"{CONFIG_PROPERTY}","value":{value},"protected":true,
            "version":1,"lastUpdated":"2026-10-17T00:00:00Z"}}"#
        ))
        .unwrap()
    }
}
