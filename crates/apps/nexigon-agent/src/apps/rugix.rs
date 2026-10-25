//! Rugix Apps through the `rugix-ctrl apps` commands.
//!
//! [`RugixApps`] inspects the installed apps and changes them. The agent marks
//! the generations it installs with an origin naming the package version, which
//! [`AppInfo::generation_of`] uses to find the generation of a version.

use std::collections::BTreeMap;

use anyhow::Context;
use jiff::Timestamp;
use nexigon_api::types::properties::AppWorkloadMessage;
use nexigon_api::types::properties::AppWorkloadStatus;
use nexigon_api::types::repositories::PackageVersionAsset;
use nexigon_api::types::repositories::PackageVersionId;
use serde::Deserialize;
use serde::Serialize;
use serde::de::IgnoredAny;

use crate::reconcile::process::run;
use crate::reconcile::process::tool_version;

/// Rugix Ctrl executable.
const RUGIX_CTRL: &str = "rugix-ctrl";

/// Apps managed by Rugix Ctrl.
pub(crate) struct RugixApps;

/// Bundle to install as a new generation.
pub(crate) struct AppBundle<'a> {
    pub url: &'a str,
    /// Hash of the bundle for Rugix Ctrl to verify, if any.
    pub hash: Option<&'a str>,
    pub version_id: &'a PackageVersionId,
    pub asset: &'a PackageVersionAsset,
}

impl RugixApps {
    /// Version of Rugix Ctrl.
    pub(crate) async fn version(&self) -> anyhow::Result<String> {
        Ok(tool_version(&run(RUGIX_CTRL, &["--version"]).await?))
    }

    /// Information about every installed app by name.
    ///
    /// Fails if the apps cannot be listed. An app that cannot be inspected maps
    /// to its error.
    pub(crate) async fn installed(
        &self,
    ) -> anyhow::Result<BTreeMap<String, anyhow::Result<AppInfo>>> {
        let list = run(RUGIX_CTRL, &["apps", "list"]).await?;
        let list: BTreeMap<String, IgnoredAny> =
            serde_json::from_str(&list).context("failed to parse the list of apps")?;
        let mut apps = BTreeMap::new();
        for name in list.into_keys() {
            let info = self.info(&name).await.context("failed to inspect the app");
            apps.insert(name, info);
        }
        Ok(apps)
    }

    /// Information about an installed app.
    pub(crate) async fn info(&self, app: &str) -> anyhow::Result<AppInfo> {
        let info = run(RUGIX_CTRL, &["apps", "info", app]).await?;
        serde_json::from_str(&info)
            .with_context(|| format!("failed to parse the info of app {app}"))
    }

    /// Install a bundle as a new generation without activating it.
    pub(crate) async fn install(&self, bundle: AppBundle<'_>) -> anyhow::Result<()> {
        let origin = serde_json::to_string(&Origin {
            nexigon: NexigonOrigin {
                version_id: bundle.version_id.clone(),
                asset_id: Some(bundle.asset.asset_id.to_string()),
            },
        })
        .context("failed to serialize the origin of the bundle")?;
        let mut args = vec!["apps", "install", "--no-activate", "--origin", &origin];
        if let Some(hash) = bundle.hash {
            args.extend(["--bundle-hash", hash]);
        }
        args.push(bundle.url);
        run(RUGIX_CTRL, &args).await.map(drop)
    }

    /// Activate a generation of an app, which starts its workload.
    ///
    /// If the activation fails, Rugix Ctrl restores the previously active
    /// generation.
    pub(crate) async fn activate(&self, app: &str, generation: u64) -> anyhow::Result<()> {
        run(
            RUGIX_CTRL,
            &["apps", "activate", app, &generation.to_string()],
        )
        .await
        .map(drop)
    }

    /// Deactivate an app, which stops its workload, also after a reboot.
    pub(crate) async fn deactivate(&self, app: &str) -> anyhow::Result<()> {
        run(RUGIX_CTRL, &["apps", "deactivate", app])
            .await
            .map(drop)
    }

    /// Remove an app, including its persistent data.
    pub(crate) async fn remove(&self, app: &str) -> anyhow::Result<()> {
        run(RUGIX_CTRL, &["apps", "remove", app]).await.map(drop)
    }

    /// Remove generations other than the active one and the `keep` most recent
    /// previously active ones, including their stored files.
    pub(crate) async fn collect_garbage(&self, app: &str, keep: u32) -> anyhow::Result<()> {
        run(
            RUGIX_CTRL,
            &["apps", "gc", "--keep", &keep.to_string(), app],
        )
        .await
        .map(drop)
    }
}

/// Output of `rugix-ctrl apps info`.
#[derive(Debug, Deserialize)]
pub(crate) struct AppInfo {
    status: RugixWorkloadStatus,
    lifecycle: Lifecycle,
    generations: Vec<Generation>,
}

impl AppInfo {
    /// Whether a generation of the app is active.
    pub(crate) fn is_active(&self) -> bool {
        matches!(
            self.lifecycle.state,
            LifecycleState::Active | LifecycleState::Starting | LifecycleState::Stopping
        )
    }

    /// Number of the active generation, if the app is active.
    pub(crate) fn active_generation(&self) -> Option<u64> {
        self.generations
            .iter()
            .find(|generation| generation.active)
            .filter(|_| self.is_active())
            .map(|generation| generation.number)
    }

    /// Newest complete generation installed from a package version.
    pub(crate) fn generation_of(&self, version_id: &PackageVersionId) -> Option<u64> {
        self.generations
            .iter()
            .filter(|generation| generation.complete && generation.version_id() == Some(version_id))
            .map(|generation| generation.number)
            .max()
    }

    /// Generation that runs, or that Rugix Ctrl activates when the app starts.
    pub(crate) fn deployed_generation(&self) -> Option<u64> {
        self.active_generation().or_else(|| {
            self.generations
                .iter()
                .filter(|generation| generation.complete)
                .filter_map(|generation| Some((generation.last_activated?, generation.number)))
                .max()
                .map(|(_, number)| number)
        })
    }

    /// Package version that a generation was installed from, if the agent
    /// installed it.
    pub(crate) fn version_of(&self, generation: u64) -> Option<&PackageVersionId> {
        self.generations
            .iter()
            .find(|candidate| candidate.number == generation)?
            .version_id()
    }

    /// Live status of the app's workload.
    pub(crate) fn workload(&self) -> AppWorkloadStatus {
        match &self.status {
            RugixWorkloadStatus::Running => AppWorkloadStatus::Running,
            RugixWorkloadStatus::Unhealthy { message } => {
                AppWorkloadStatus::Unhealthy(AppWorkloadMessage::new(message.clone()))
            }
            RugixWorkloadStatus::Stopped => AppWorkloadStatus::Stopped,
            RugixWorkloadStatus::Failed { message } => {
                AppWorkloadStatus::Failed(AppWorkloadMessage::new(message.clone()))
            }
            RugixWorkloadStatus::Unknown => AppWorkloadStatus::Unknown,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
enum RugixWorkloadStatus {
    Running,
    Unhealthy {
        message: String,
    },
    Stopped,
    Failed {
        message: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct Lifecycle {
    state: LifecycleState,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LifecycleState {
    Active,
    Starting,
    Stopping,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Generation {
    number: u64,
    complete: bool,
    active: bool,
    last_activated: Option<Timestamp>,
    origin: Option<GenerationOrigin>,
}

impl Generation {
    fn version_id(&self) -> Option<&PackageVersionId> {
        match self.origin.as_ref()? {
            GenerationOrigin::Nexigon(origin) => Some(&origin.nexigon.version_id),
            GenerationOrigin::Other(_) => None,
        }
    }
}

/// Origin of a generation, which another tool may have installed.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GenerationOrigin {
    Nexigon(Origin),
    Other(IgnoredAny),
}

/// Origin that the agent records with the generations it installs.
#[derive(Debug, Serialize, Deserialize)]
struct Origin {
    nexigon: NexigonOrigin,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NexigonOrigin {
    version_id: PackageVersionId,
    asset_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generations are found by the package version in their origin, the active
    /// generation only counts while the app is active, and a deactivated app
    /// deploys its most recently activated generation.
    #[test]
    fn generations_from_app_info() {
        let version =
            |name: &str| -> PackageVersionId { format!("pkg_v_{name:1>22}").parse().unwrap() };
        let info = |lifecycle: &str| -> AppInfo {
            serde_json::from_str(&format!(
                r#"{{"name":"controller","status":{{"state":"stopped"}},
                "lifecycle":{lifecycle},"generations":[
                {{"number":1,"complete":true,"active":false,
                    "lastActivated":"2026-10-20T10:00:00.5Z",
                    "origin":{{"nexigon":{{"versionId":"{v1}"}}}}}},
                {{"number":2,"complete":true,"active":true,
                    "lastActivated":"2026-10-21T10:00:00Z",
                    "origin":{{"nexigon":{{"versionId":"{v2}","assetId":"a"}}}}}},
                {{"number":3,"complete":true,"active":false,"origin":"manual"}},
                {{"number":4,"complete":false,"active":false,
                    "origin":{{"nexigon":{{"versionId":"{v1}"}}}}}}]}}"#,
                v1 = version("a"),
                v2 = version("b"),
            ))
            .unwrap()
        };

        let active = info(r#"{"state":"active","generation":2}"#);
        assert!(active.is_active());
        assert_eq!(active.active_generation(), Some(2));
        assert_eq!(active.generation_of(&version("a")), Some(1));
        assert_eq!(active.generation_of(&version("c")), None);
        assert_eq!(active.version_of(2), Some(&version("b")));
        assert_eq!(active.version_of(3), None);
        assert!(matches!(active.workload(), AppWorkloadStatus::Stopped));

        let inactive = info(r#"{"state":"inactive"}"#);
        assert_eq!(inactive.active_generation(), None);
        assert_eq!(inactive.deployed_generation(), Some(2));
    }
}
