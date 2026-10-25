//! One check of the app reconciliation loop.
//!
//! A check reads the desired state from `dev.nexigon.apps.config` and the
//! installed apps from Rugix Ctrl. It brings every managed app into its desired
//! state and installs and activates the version that the app's path resolves
//! to, if the hold, spread, retry, and maintenance window policies allow it. It
//! publishes the status of the managed and installed apps in
//! `dev.nexigon.apps.status`.
//!
//! Every generation that the agent installs records its package version, so the
//! device itself knows which versions its apps run. The previous status only
//! carries the failure counts and the retry schedules across checks. They belong
//! to the target version and the configuration of an app, and start over when
//! either changes.
//!
//! A check first starts or stops each app according to its desired state,
//! regardless of the update policies. A version change then has two steps.
//! Installing downloads the bundle into a new generation without interrupting
//! the app, so it may happen outside the maintenance window. Activating switches
//! the app to the generation, which the window restricts while the app runs. A
//! stopped app keeps the installed generation until it starts, and starts with
//! it. After an activation, the check removes old generations of the app,
//! except for those that the `gc` policy keeps.
//!
//! Status writes carry the version read last (compare-and-swap), so that a
//! concurrent change, such as a cleared failure count, is not overwritten. A
//! check that loses such a race starts over from the changed status. Once a
//! check has changed the device, it writes its status regardless.

use std::sync::Arc;

use anyhow::Context;
use anyhow::bail;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use nexigon_api::types::devices::DeviceEventSeverity;
use nexigon_api::types::devices::DeviceProperty;
use nexigon_api::types::properties::AppDesiredState;
use nexigon_api::types::properties::AppGcPolicy;
use nexigon_api::types::properties::AppRuntime;
use nexigon_api::types::properties::AppStatus;
use nexigon_api::types::properties::AppUpdateBlocked;
use nexigon_api::types::properties::AppUpdateCapability;
use nexigon_api::types::properties::AppUpdateConfig;
use nexigon_api::types::properties::AppUpdateState;
use nexigon_api::types::properties::AppUpdateStatus;
use nexigon_api::types::properties::ManagedAppConfig;
use nexigon_api::types::properties::OtaUpdateRetryPolicy;
use nexigon_api::types::repositories::PackageVersionAsset;
use nexigon_api::types::repositories::PackageVersionId;
use nexigon_ids::ids::DeviceId;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing::warn;

use super::AppUpdates;
use super::rugix::AppBundle;
use super::rugix::AppInfo;
use super::rugix::RugixApps;
use crate::device_api::DeviceExecutor;
use crate::reconcile::WindowGate;
use crate::reconcile::error_delay;
use crate::reconcile::hub::Hub;
use crate::reconcile::hub::PropertyWrite;
use crate::reconcile::hub::VersionPath;
use crate::reconcile::hub::rugix_bundle_hash;
use crate::reconcile::next_retry;
use crate::reconcile::policy::SpreadState;
use crate::reconcile::policy::retry_policy_with_defaults;
use crate::reconcile::policy::spread_position;
use crate::reconcile::policy::spread_state;
use crate::reconcile::window_gate;

/// Device property holding the desired state.
const CONFIG_PROPERTY: &str = "dev.nexigon.apps.config";
/// Device property holding the actual state.
const STATUS_PROPERTY: &str = "dev.nexigon.apps.status";
/// Event category of app events.
const EVENT_CATEGORY: &str = "apps";
/// Number of times a check starts over after the status changed concurrently.
const MAX_STATUS_CONFLICTS: usize = 3;

/// Optional features of the app update contract that the agent implements.
const CAPABILITIES: [AppUpdateCapability; 6] = [
    AppUpdateCapability::Hold,
    AppUpdateCapability::Asset,
    AppUpdateCapability::Window,
    AppUpdateCapability::Retry,
    AppUpdateCapability::Spread,
    AppUpdateCapability::Gc,
];

/// Executes checks against Nexigon Hub and Rugix Ctrl for one device.
pub(crate) struct Checker {
    settings: AppUpdates,
    hub: Hub,
    rugix: RugixApps,
    /// Serializes changes of the device with the system update integration.
    device_lock: Arc<Mutex<()>>,
    /// Signals a lost connection or a shutdown, which prevents new changes.
    cancellation: CancellationToken,
    /// Version of the status read or written last, or `None` without a status.
    status_version: Option<u64>,
    /// Whether this check changed the device.
    changed: bool,
    /// Whether an app failed for a reason other than an attempt, such as an
    /// unreachable Nexigon Hub, so that the next check is due soon.
    recheck: bool,
}

/// An app that a check brings into its desired state.
struct ManagedApp<'a> {
    name: &'a str,
    /// Configuration of the app with the policies that apply to it.
    config: ManagedAppConfig,
    /// Status of the app published by the previous check, if the app's
    /// configuration has not changed since.
    previous: Option<&'a AppStatus>,
}

impl Checker {
    pub(crate) fn new(
        settings: AppUpdates,
        executor: DeviceExecutor,
        device_id: DeviceId,
        device_lock: Arc<Mutex<()>>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            settings,
            hub: Hub::new(executor, device_id),
            rugix: RugixApps,
            device_lock,
            cancellation,
            status_version: None,
            changed: false,
            recheck: false,
        }
    }

    /// Run one check and publish the resulting status.
    ///
    /// Starts over if the status changes concurrently. Returns the earliest
    /// time at which an app's next attempt is due.
    #[tracing::instrument(skip_all)]
    pub(crate) async fn check(&mut self) -> anyhow::Result<Option<Timestamp>> {
        for _ in 0..MAX_STATUS_CONFLICTS {
            match self.check_once().await {
                Err(error) if error.is::<StatusConflict>() => {
                    info!("the app status changed during the check, starting over");
                }
                outcome => return outcome,
            }
        }
        self.check_once().await
    }

    async fn check_once(&mut self) -> anyhow::Result<Option<Timestamp>> {
        // Versions may be deleted or rebased between checks.
        self.hub.clear_cache();
        self.changed = false;
        self.recheck = false;
        let previous = self.previous_status().await?;
        let property = self.hub.property(CONFIG_PROPERTY).await?;
        let config_version = property.as_ref().map(|property| property.version);
        // Apps keep their previous status until the check reaches them.
        let mut status = AppUpdateStatus::new(previous.apps.clone())
            .with_capabilities(Some(CAPABILITIES.to_vec().into()))
            .with_config_version(config_version)
            .with_arch(Some(self.settings.arch.clone().into()))
            .with_runtime(Some(self.runtime().await));
        let mut installed = match self.rugix.installed().await {
            Ok(installed) => installed,
            Err(error) => {
                let error = error.context("failed to list the installed apps");
                status.last_error = Some(format!("{error:#}").into());
                status.config_version = previous.config_version;
                self.publish(&status).await?;
                return Err(error);
            }
        };
        let config = parse_config(property.as_ref()).unwrap_or_else(|error| {
            let error = format!("{error:#}");
            warn!(%error, "invalid app configuration");
            status.last_error = Some(error.into());
            AppUpdateConfig::new()
        });
        let managed = config.apps.clone().unwrap_or_default();
        status
            .apps
            .retain(|name, _| managed.contains_key(name) || installed.contains_key(name.as_str()));
        for (name, info) in &installed {
            if !managed.contains_key(name.as_str()) {
                let app_status = match info {
                    Ok(info) => observed(AppStatus::new(false), info),
                    Err(error) => failed(AppStatus::new(false), error),
                };
                status.apps.insert(name.clone().into(), app_status);
            }
        }

        let mut next_attempt_at: Option<Timestamp> = None;
        for (name, app_config) in managed.iter() {
            let config = effective_app_config(&config, app_config, self.settings.gc_keep);
            let app = ManagedApp {
                name,
                previous: previous
                    .apps
                    .get(name.as_str())
                    .filter(|previous| same_app_config(previous.config.as_ref(), &config)),
                config,
            };
            let info = installed.remove(name.as_str());
            let app_status = match self.reconcile(&mut status, app, info).await {
                Ok(app_status) => app_status,
                Err(error) => {
                    // The agent stops, so the remaining apps are left for the
                    // next check, but the changes made so far are published.
                    self.publish(&status).await?;
                    return Err(error);
                }
            };
            if let Some(at) = app_status.next_attempt_at {
                let at = Timestamp::from(at);
                next_attempt_at = Some(next_attempt_at.map_or(at, |earliest| earliest.min(at)));
            }
            status.apps.insert(name.clone(), app_status);
        }
        self.publish(&status).await?;
        if self.recheck
            && let Ok(at) = Timestamp::now().checked_add(error_delay(self.settings.check_interval))
        {
            next_attempt_at = Some(next_attempt_at.map_or(at, |earliest| earliest.min(at)));
        }
        Ok(next_attempt_at)
    }

    /// Bring a managed app into its desired state and return its status.
    ///
    /// A failure becomes part of the app's status, so that it does not keep
    /// the check from bringing the other apps into their desired states. Fails
    /// only if the agent stops.
    async fn reconcile(
        &mut self,
        status: &mut AppUpdateStatus,
        app: ManagedApp<'_>,
        info: Option<anyhow::Result<AppInfo>>,
    ) -> anyhow::Result<AppStatus> {
        let mut app_status = AppStatus::new(true).with_config(Some(app.config.clone()));
        let info = match info.transpose() {
            Ok(info) => info,
            Err(error) => return Ok(self.app_error(&app, app_status, error)),
        };
        if let Some(info) = &info {
            app_status = observed(app_status, info);
        }
        match self.converge(status, &app, info, app_status.clone()).await {
            Err(error) if !self.cancellation.is_cancelled() => {
                Ok(self.app_error(&app, app_status, error))
            }
            result => result,
        }
    }

    async fn converge(
        &mut self,
        status: &mut AppUpdateStatus,
        app: &ManagedApp<'_>,
        info: Option<AppInfo>,
        mut app_status: AppStatus,
    ) -> anyhow::Result<AppStatus> {
        let desired = app
            .config
            .state
            .clone()
            .expect("the effective configuration has a state");
        if matches!(desired, AppDesiredState::Removed) {
            if info.is_none() {
                return Ok(app_status.with_state(Some(AppUpdateState::Completed)));
            }
            let _guard = self.lock_device().await?;
            self.report(status, app.name, &app_status, AppUpdateState::Removing)
                .await?;
            self.rugix.remove(app.name).await?;
            info!(app = app.name, "removed app");
            return Ok(AppStatus::new(true)
                .with_config(Some(app.config.clone()))
                .with_state(Some(AppUpdateState::Completed)));
        }

        let target = self.target(&app.config).await;
        // Starting and stopping an app are no version changes, so the update
        // policies do not apply to them.
        let info = match info {
            Some(info) => {
                let target_generation = match &target {
                    Ok(Some((_, target_id))) => info.generation_of(target_id),
                    _ => None,
                };
                Some(
                    self.apply_state(app, &desired, info, target_generation)
                        .await?,
                )
            }
            None => None,
        };
        if let Some(info) = &info {
            app_status = observed(app_status, info);
        }
        let Some((path, target_id)) = target? else {
            return Ok(app_status.with_state(Some(AppUpdateState::Idle)));
        };
        app_status.target_version_id = Some(target_id.clone());
        let target_generation = info
            .as_ref()
            .and_then(|info| info.generation_of(&target_id));
        // A stopped app has its target version once the version is installed.
        let converged = match (&info, target_generation) {
            (Some(info), Some(generation)) => {
                info.deployed_generation() == Some(generation)
                    || matches!(desired, AppDesiredState::Stopped)
            }
            _ => false,
        };
        if converged {
            return Ok(app_status
                .with_generation(target_generation)
                .with_current_version_id(Some(target_id))
                .with_state(Some(AppUpdateState::Completed))
                .with_failure_count(Some(0)));
        }
        self.update(status, app, info.as_ref(), app_status, &path, &target_id)
            .await
    }

    /// Resolve the configured path of an app, if any.
    async fn target(
        &mut self,
        config: &ManagedAppConfig,
    ) -> anyhow::Result<Option<(VersionPath, PackageVersionId)>> {
        let Some(path) = config.path.as_deref() else {
            return Ok(None);
        };
        let path = VersionPath::parse(path)?;
        let Some(target_id) = self.hub.resolve(&path).await? else {
            bail!("version {path} does not exist");
        };
        Ok(Some((path, target_id)))
    }

    /// Start or stop an installed app according to its desired state.
    ///
    /// A stopped app whose target version is installed starts with that version,
    /// which [`Self::update`] activates as a version change. Returns the
    /// information about the app after the change.
    async fn apply_state(
        &mut self,
        app: &ManagedApp<'_>,
        desired: &AppDesiredState,
        info: AppInfo,
        target_generation: Option<u64>,
    ) -> anyhow::Result<AppInfo> {
        let deployed = info.deployed_generation();
        let starts_with_target = target_generation.is_some()
            && target_generation != deployed
            && app.config.hold != Some(true);
        match (desired, deployed) {
            (AppDesiredState::Running, Some(generation))
                if !info.is_active() && !starts_with_target =>
            {
                let _guard = self.lock_device().await?;
                self.changed = true;
                self.rugix.activate(app.name, generation).await?;
                info!(app = app.name, "started app");
            }
            (AppDesiredState::Stopped, _) if info.is_active() => {
                let _guard = self.lock_device().await?;
                self.changed = true;
                self.rugix.deactivate(app.name).await?;
                info!(app = app.name, "stopped app");
            }
            _ => return Ok(info),
        }
        self.rugix.info(app.name).await
    }

    /// Install the target version of an app and activate it, unless the app is
    /// stopped, as far as the policies allow it.
    async fn update(
        &mut self,
        status: &mut AppUpdateStatus,
        app: &ManagedApp<'_>,
        info: Option<&AppInfo>,
        mut app_status: AppStatus,
        path: &VersionPath,
        target_id: &PackageVersionId,
    ) -> anyhow::Result<AppStatus> {
        let now = Timestamp::now();
        let retry = app
            .config
            .retry
            .clone()
            .expect("the effective configuration has a retry policy");
        let target_generation = info.and_then(|info| info.generation_of(target_id));
        // The failure count belongs to the target version and the configuration.
        let previous = app
            .previous
            .filter(|previous| previous.target_version_id.as_ref() == Some(target_id));
        let mut failures = previous
            .and_then(|previous| previous.failure_count)
            .unwrap_or(0);
        // An installation that completed before the agent stopped is no failure.
        let interrupted = previous.is_some_and(|previous| match previous.state {
            Some(AppUpdateState::Installing) => target_generation.is_none(),
            Some(AppUpdateState::Activating) => true,
            _ => false,
        });
        if interrupted {
            let error = "the update was interrupted";
            warn!(app = app.name, error, "app update failed");
            self.event(
                app.name,
                DeviceEventSeverity::Error,
                "app update failed",
                target_id,
                Some(error),
            )
            .await;
            failures += 1;
            app_status.last_error = Some(error.into());
        } else {
            // The error of the last failed attempt stays until the attempt succeeds.
            app_status.last_error = previous.and_then(|previous| previous.last_error.clone());
        }
        let limit_reached = retry
            .max_attempts
            .is_some_and(|max_attempts| failures >= max_attempts);
        let retry_at = if interrupted {
            next_retry(&retry, failures, now)
        } else if previous
            .is_some_and(|previous| matches!(previous.state, Some(AppUpdateState::Failed)))
        {
            // Without a scheduled attempt, only a reached limit stops attempts.
            match previous.and_then(|previous| previous.next_attempt_at) {
                Some(at) => Some(at.into()),
                None if limit_reached => None,
                None => Some(now),
            }
        } else {
            Some(now)
        };
        app_status.failure_count = Some(failures);

        if app.config.hold == Some(true) {
            return Ok(blocked(app_status, "hold", None, None));
        }
        if let Some(spread) = &app.config.spread {
            let start = match spread.start_at {
                Some(start) => Some(start.into()),
                None => self.tag_assigned_at(target_id, &path.version).await?,
            };
            let position =
                spread_position(&self.hub.device_id().to_string(), &target_id.to_string());
            if let SpreadState::Waiting { released_at } = spread_state(spread, start, position, now)
            {
                return Ok(blocked(app_status, "spread", None, released_at));
            }
        }
        match retry_at {
            None => return Ok(app_status.with_state(Some(AppUpdateState::Failed))),
            Some(retry_at) if now < retry_at => {
                return Ok(app_status
                    .with_state(Some(AppUpdateState::Failed))
                    .with_next_attempt_at(Some(retry_at.into())));
            }
            Some(_) if limit_reached => {
                // The cooldown has ended, so a new round of attempts begins.
                failures = 0;
                app_status.failure_count = Some(0);
            }
            Some(_) => {}
        }

        let runs = info.is_some_and(AppInfo::is_active);
        let (generation, guard) = match target_generation {
            Some(generation) => {
                if let Some(deferred) = window_deferral(&app_status, &app.config, runs) {
                    return Ok(deferred);
                }
                (generation, self.lock_device().await?)
            }
            None => {
                let Some(asset) = self.bundle(app.name, &app.config, target_id).await? else {
                    let message = format!(
                        "The target version has no bundle {} for architecture {}.",
                        bundle_filenames(app.name, &app.config, &self.settings.arch).join(" or "),
                        self.settings.arch,
                    );
                    return Ok(blocked(app_status, "no-asset", Some(message), None));
                };
                app_status.asset = Some(asset.filename.clone());
                // A system update may hold the lock for long, so the download
                // URL is issued afterwards. Failures before the download starts
                // do not count as attempts.
                let guard = self.lock_device().await?;
                let url = self.hub.download_url(&asset).await?;
                self.report(status, app.name, &app_status, AppUpdateState::Installing)
                    .await?;
                let generation = match self.install(app.name, target_id, &asset, &url).await {
                    Ok(generation) => generation,
                    Err(error) => {
                        return Ok(self
                            .fail_attempt(app_status, app.name, target_id, &retry, failures, error)
                            .await);
                    }
                };
                // The next check activates the installed generation.
                if self.cancellation.is_cancelled() {
                    bail!("the agent is stopping");
                }
                if let Some(deferred) = window_deferral(&app_status, &app.config, runs) {
                    return Ok(deferred);
                }
                (generation, guard)
            }
        };

        let start = matches!(app.config.state, Some(AppDesiredState::Running));
        if start {
            self.report(status, app.name, &app_status, AppUpdateState::Activating)
                .await?;
            if let Err(error) = self.rugix.activate(app.name, generation).await {
                return Ok(self
                    .fail_attempt(app_status, app.name, target_id, &retry, failures, error)
                    .await);
            }
            // Garbage collection keeps installed generations until they or a
            // newer generation are activated, so it runs after an activation.
            let keep = app
                .config
                .gc
                .as_ref()
                .and_then(|gc| gc.keep)
                .expect("the effective configuration keeps a number of generations");
            if let Err(error) = self.rugix.collect_garbage(app.name, keep).await {
                warn!(
                    app = app.name,
                    error = format!("{error:#}"),
                    "failed to remove old generations"
                );
            }
        }
        drop(guard);
        info!(app = app.name, version = %target_id, "app update completed");
        self.event(
            app.name,
            DeviceEventSeverity::Info,
            "app update completed",
            target_id,
            None,
        )
        .await;
        let app_status = if start {
            observed(app_status, &self.rugix.info(app.name).await?)
        } else {
            app_status
                .with_generation(Some(generation))
                .with_current_version_id(Some(target_id.clone()))
        };
        Ok(app_status
            .with_state(Some(AppUpdateState::Completed))
            .with_failure_count(Some(0))
            .with_last_error(None))
    }

    /// Install the bundle of a version as a new generation of an app.
    async fn install(
        &mut self,
        app: &str,
        version_id: &PackageVersionId,
        asset: &PackageVersionAsset,
        url: &str,
    ) -> anyhow::Result<u64> {
        let hash = rugix_bundle_hash(asset).filter(|_| self.settings.use_bundle_hash);
        info!(app, version = %version_id, "installing app update");
        self.event(
            app,
            DeviceEventSeverity::Info,
            "installing app update",
            version_id,
            None,
        )
        .await;
        self.rugix
            .install(AppBundle {
                url,
                hash: hash.as_deref(),
                version_id,
                asset,
            })
            .await?;
        self.rugix
            .info(app)
            .await?
            .generation_of(version_id)
            .context("the bundle did not install a generation of the app")
    }

    /// Bundle of a version for the device's architecture, if available.
    async fn bundle(
        &mut self,
        app: &str,
        config: &ManagedAppConfig,
        version_id: &PackageVersionId,
    ) -> anyhow::Result<Option<PackageVersionAsset>> {
        for filename in bundle_filenames(app, config, &self.settings.arch) {
            if let Some(asset) = self.hub.available_asset(version_id, &filename).await? {
                return Ok(Some(asset));
            }
        }
        Ok(None)
    }

    /// Time at which the version's tag was assigned, if the tag points to it.
    async fn tag_assigned_at(
        &mut self,
        version_id: &PackageVersionId,
        tag: &str,
    ) -> anyhow::Result<Option<Timestamp>> {
        Ok(self
            .hub
            .version_details(version_id)
            .await?
            .tags
            .iter()
            .find(|candidate| *candidate.tag == *tag)
            .and_then(|tag| tag.assigned_at)
            .map(Into::into))
    }

    /// Record a failed attempt and schedule the next one.
    async fn fail_attempt(
        &mut self,
        app_status: AppStatus,
        app: &str,
        version_id: &PackageVersionId,
        retry: &OtaUpdateRetryPolicy,
        failures: u32,
        error: anyhow::Error,
    ) -> AppStatus {
        let error = format!("{error:#}");
        warn!(app, %error, "app update failed");
        self.event(
            app,
            DeviceEventSeverity::Error,
            "app update failed",
            version_id,
            Some(&error),
        )
        .await;
        let failures = failures + 1;
        app_status
            .with_state(Some(AppUpdateState::Failed))
            .with_failure_count(Some(failures))
            .with_last_error(Some(error.into()))
            .with_next_attempt_at(next_retry(retry, failures, Timestamp::now()).map(Into::into))
    }

    /// Record a failure that is no attempt, such as an unreachable Nexigon Hub.
    ///
    /// The retry state of the app stays, and the next check is due soon.
    fn app_error(
        &mut self,
        app: &ManagedApp<'_>,
        app_status: AppStatus,
        error: anyhow::Error,
    ) -> AppStatus {
        self.recheck = true;
        failed(app_status, &error)
            .with_target_version_id(
                app.previous
                    .and_then(|previous| previous.target_version_id.clone()),
            )
            .with_failure_count(app.previous.and_then(|previous| previous.failure_count))
    }

    /// Acquire the device lock before changing the device.
    async fn lock_device(&mut self) -> anyhow::Result<OwnedMutexGuard<()>> {
        let guard = self.device_lock.clone().lock_owned().await;
        if self.cancellation.is_cancelled() {
            bail!("the agent is stopping");
        }
        Ok(guard)
    }

    /// Publish the status with an app in a transient state, right before
    /// changing the app.
    ///
    /// From then on, the check publishes its status even if the status
    /// changed concurrently.
    async fn report(
        &mut self,
        status: &mut AppUpdateStatus,
        app: &str,
        app_status: &AppStatus,
        state: AppUpdateState,
    ) -> anyhow::Result<()> {
        self.changed = true;
        status
            .apps
            .insert(app.into(), app_status.clone().with_state(Some(state)));
        self.publish(status).await
    }

    async fn previous_status(&mut self) -> anyhow::Result<AppUpdateStatus> {
        let property = self.hub.property(STATUS_PROPERTY).await?;
        self.status_version = property.as_ref().map(|property| property.version);
        let empty = || AppUpdateStatus::new(Default::default());
        let Some(property) = property else {
            return Ok(empty());
        };
        let value = serde_json::to_value(&property.value)?;
        Ok(serde_json::from_value(value).unwrap_or_else(|error| {
            warn!(%error, "ignoring an invalid previous status");
            empty()
        }))
    }

    /// Describe Rugix Ctrl, logging failures instead of failing the check.
    async fn runtime(&self) -> AppRuntime {
        let version = self
            .rugix
            .version()
            .await
            .inspect_err(|error| {
                warn!(
                    error = format!("{error:#}"),
                    "failed to determine the version of Rugix Ctrl"
                );
            })
            .ok();
        AppRuntime::new("rugix".to_owned()).with_version(version.map(Into::into))
    }

    /// Publish the status unless it changed since it was read.
    ///
    /// Fails with [`StatusConflict`] if it changed, unless this check changed
    /// the device. Then the status replaces the changed one.
    async fn publish(&mut self, status: &AppUpdateStatus) -> anyhow::Result<()> {
        let write = match self
            .hub
            .set_property(STATUS_PROPERTY, status, self.status_version)
            .await?
        {
            PropertyWrite::Conflict if self.changed => {
                self.hub.set_property(STATUS_PROPERTY, status, None).await?
            }
            PropertyWrite::Conflict => return Err(StatusConflict.into()),
            write => write,
        };
        let PropertyWrite::Written { version } = write else {
            bail!("Nexigon Hub did not store the app status");
        };
        // Without a reported version, the next write of this check is
        // unconditional.
        self.status_version = version;
        Ok(())
    }

    /// Emit an app event, logging failures instead of interrupting the check.
    async fn event(
        &mut self,
        app: &str,
        severity: DeviceEventSeverity,
        message: &str,
        version_id: &PackageVersionId,
        error: Option<&str>,
    ) {
        let mut attributes = sidex::types::Map::new();
        attributes.insert("app".into(), app.into());
        attributes.insert("versionId".into(), version_id.to_string().into());
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
#[error("the app status changed concurrently")]
struct StatusConflict;

/// Read the desired state from the configuration property.
fn parse_config(property: Option<&DeviceProperty>) -> anyhow::Result<AppUpdateConfig> {
    let Some(property) = property else {
        return Ok(AppUpdateConfig::new());
    };
    let value = serde_json::to_value(&property.value)?;
    serde_json::from_value(value).context("invalid app configuration")
}

/// Configuration of an app with the policies of the configuration it does not
/// set itself and the defaults the integration applies.
///
/// `gc_keep` is the number of previously active generations to keep if neither
/// sets it.
fn effective_app_config(
    config: &AppUpdateConfig,
    app: &ManagedAppConfig,
    gc_keep: u32,
) -> ManagedAppConfig {
    let mut app = app.clone();
    app.state = Some(app.state.unwrap_or(AppDesiredState::Running));
    app.hold = app.hold.or(config.hold);
    app.window = app.window.or_else(|| config.window.clone());
    app.retry = Some(retry_policy_with_defaults(
        app.retry.as_ref().or(config.retry.as_ref()),
    ));
    app.spread = app.spread.or_else(|| config.spread.clone());
    let keep = app
        .gc
        .as_ref()
        .or(config.gc.as_ref())
        .and_then(|gc| gc.keep)
        .unwrap_or(gc_keep);
    app.gc = Some(AppGcPolicy::new().with_keep(Some(keep)));
    app
}

/// Whether the effective configuration of an app is unchanged.
fn same_app_config(previous: Option<&ManagedAppConfig>, current: &ManagedAppConfig) -> bool {
    previous.is_some_and(|previous| {
        serde_json::to_value(previous).ok() == serde_json::to_value(current).ok()
    })
}

/// Filenames of the bundles to install for an app, in order of preference.
fn bundle_filenames(app: &str, config: &ManagedAppConfig, arch: &str) -> Vec<String> {
    match config.asset.as_deref() {
        Some(asset) => vec![asset.replace("{arch}", arch)],
        None => vec![format!("{app}-{arch}.rugixb"), format!("{app}.rugixb")],
    }
}

/// Record what the device reports about an installed app.
fn observed(app_status: AppStatus, info: &AppInfo) -> AppStatus {
    let generation = info.deployed_generation();
    app_status
        .with_generation(generation)
        .with_current_version_id(
            generation.and_then(|generation| info.version_of(generation).cloned()),
        )
        .with_workload(Some(info.workload()))
}

/// Defer switching a running app to a new version while its maintenance window
/// is closed.
fn window_deferral(
    app_status: &AppStatus,
    config: &ManagedAppConfig,
    runs: bool,
) -> Option<AppStatus> {
    let window = config.window.as_ref().filter(|_| runs)?;
    match window_gate(window, TimeZone::system()) {
        WindowGate::Open => None,
        WindowGate::Closed { opens_at } => {
            Some(blocked(app_status.clone(), "window", None, opens_at))
        }
        WindowGate::Unsynchronized { recheck_at } => Some(blocked(
            app_status.clone(),
            "window",
            Some("The device clock is not synchronized.".to_owned()),
            recheck_at,
        )),
        WindowGate::Invalid(error) => Some(failed(
            app_status.clone(),
            &error.context("invalid maintenance window"),
        )),
    }
}

fn blocked(
    app_status: AppStatus,
    code: &str,
    message: Option<String>,
    next_attempt_at: Option<Timestamp>,
) -> AppStatus {
    app_status
        .with_state(Some(AppUpdateState::Blocked))
        .with_blocked(Some(
            AppUpdateBlocked::new(code.to_owned()).with_message(message.map(Into::into)),
        ))
        .with_next_attempt_at(next_attempt_at.map(Into::into))
}

/// Record a change that cannot proceed, for example because of an invalid path.
fn failed(app_status: AppStatus, error: &anyhow::Error) -> AppStatus {
    let error = format!("{error:#}");
    warn!(%error, "app update failed");
    app_status
        .with_state(Some(AppUpdateState::Failed))
        .with_last_error(Some(error.into()))
}
