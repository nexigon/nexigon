//! Update systems that install system images on the device.
//!
//! [`SystemUpdater`] is the interface through which the update checks drive an
//! update system. [`Updater`] implements it with Rugix Ctrl or RAUC to determine
//! the running system, install bundles, reboot into them, and commit them.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use futures::FutureExt;
use futures::future::BoxFuture;
use nexigon_api::types::properties::OtaUpdater;
use nexigon_api::types::properties::OtaUpdaterSlot;
use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::reconcile::process::run;
use crate::reconcile::process::tool_version;

/// Path of the build information written by Rugix Bakery.
const RUGIX_BUILD_INFO_PATH: &str = "/etc/rugix/system-build-info.json";
/// Path of the operating system identification file.
const OS_RELEASE_PATH: &str = "/etc/os-release";

/// Update system that installs bundles and boots into them.
///
/// A check commits a newly booted system that awaits its commit, installs a
/// bundle, and reboots into it. If the agent stops or the maintenance window
/// closes between the installation and the reboot, the check deactivates the
/// installed bundle instead. The methods follow these steps of the update
/// checks, so they may change with them; implementations belong to hosts that
/// run the agent in-process, such as simulators.
pub trait SystemUpdater: Send + Sync {
    /// Filename of the bundle for a system in a package version.
    ///
    /// The device installs the bundle of the running system unless the
    /// configuration names another asset.
    fn bundle_filename(&self, system: &str) -> String;

    /// Determine the name and version of the running system.
    fn identity(&self) -> BoxFuture<'_, anyhow::Result<SystemIdentity>>;

    /// Whether a newly booted system awaits its commit.
    fn awaits_commit(&self) -> BoxFuture<'_, anyhow::Result<bool>>;

    /// Commit the newly booted system.
    fn commit(&self) -> BoxFuture<'_, anyhow::Result<()>>;

    /// Install a bundle without rebooting.
    fn install<'a>(&'a self, bundle: Bundle<'a>) -> BoxFuture<'a, anyhow::Result<()>>;

    /// Keep the running system active after installing a bundle.
    fn deactivate(&self) -> BoxFuture<'_, anyhow::Result<()>>;

    /// Describe the update system, its slots, and the boot state.
    fn info(&self) -> BoxFuture<'_, anyhow::Result<OtaUpdater>>;

    /// Reboot into the installed bundle.
    ///
    /// After the reboot has been initiated, the check waits for the agent to
    /// stop. A host running the agent in-process must stop it by resolving the
    /// `shutdown` future of [`run_with_connection`](crate::run_with_connection).
    /// Otherwise, the check reports a rollback after a timeout.
    fn reboot(&self) -> BoxFuture<'_, anyhow::Result<()>>;
}

/// Bundle of a package version to install.
#[derive(Debug, Clone, Copy)]
pub struct Bundle<'a> {
    /// Download URL of the bundle.
    pub url: &'a str,
    /// Hash of the bundle from the asset metadata, if any.
    pub hash: Option<&'a str>,
    /// Image version that the bundle installs, from the `imageVersion` metadata
    /// of the package version, if any.
    ///
    /// After the reboot, the check compares it with the version of
    /// [`SystemUpdater::identity`] to detect a completed update or a rollback.
    pub image_version: Option<&'a str>,
}

/// Name and version of the running system.
#[derive(Debug, Clone)]
pub struct SystemIdentity {
    /// System name, which identifies the bundle in a package version.
    pub name: String,
    /// Version string built into the image.
    pub version: String,
}

/// Update system installing bundles on the device.
#[derive(Debug, Clone)]
pub(crate) enum Updater {
    /// Rugix Ctrl with A/B boot groups.
    Rugix {
        /// Pass the bundle hash from the asset metadata to Rugix Ctrl.
        use_bundle_hash: bool,
    },
    /// RAUC.
    Rauc,
}

impl SystemUpdater for Updater {
    fn bundle_filename(&self, system: &str) -> String {
        match self {
            Self::Rugix { .. } => format!("{system}.rugixb"),
            Self::Rauc => format!("{system}.raucb"),
        }
    }

    /// Rugix systems built with Rugix Bakery provide their build information.
    /// Otherwise, `IMAGE_ID` and `IMAGE_VERSION` from `/etc/os-release` apply.
    fn identity(&self) -> BoxFuture<'_, anyhow::Result<SystemIdentity>> {
        async move {
            if matches!(self, Self::Rugix { .. }) && Path::new(RUGIX_BUILD_INFO_PATH).exists() {
                let content = tokio::fs::read_to_string(RUGIX_BUILD_INFO_PATH)
                    .await
                    .context("failed to read Rugix build information")?;
                let info: RugixBuildInfo = serde_json::from_str(&content)
                    .context("failed to parse Rugix build information")?;
                return Ok(SystemIdentity {
                    name: info.name,
                    version: info.release.version,
                });
            }
            let content = tokio::fs::read_to_string(OS_RELEASE_PATH)
                .await
                .context("failed to read /etc/os-release")?;
            let field = |name: &str| {
                os_release_field(&content, name)
                    .with_context(|| format!("{name} is not set in /etc/os-release"))
            };
            Ok(SystemIdentity {
                name: field("IMAGE_ID")?,
                version: field("IMAGE_VERSION")?,
            })
        }
        .boxed()
    }

    /// RAUC systems are marked good by RAUC itself, so they never await a
    /// commit from the agent.
    fn awaits_commit(&self) -> BoxFuture<'_, anyhow::Result<bool>> {
        async move {
            match self {
                Self::Rugix { .. } => {
                    let output = run("rugix-ctrl", &["system", "info"]).await?;
                    let info: RugixSystemInfo = serde_json::from_str(&output)
                        .context("failed to parse Rugix system information")?;
                    Ok(info
                        .boot
                        .is_some_and(|boot| boot.active_group != boot.default_group))
                }
                Self::Rauc => Ok(false),
            }
        }
        .boxed()
    }

    fn commit(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        async move {
            match self {
                Self::Rugix { .. } => run("rugix-ctrl", &["system", "commit"]).await.map(drop),
                Self::Rauc => Ok(()),
            }
        }
        .boxed()
    }

    fn install<'a>(&'a self, bundle: Bundle<'a>) -> BoxFuture<'a, anyhow::Result<()>> {
        async move {
            match self {
                Self::Rugix { use_bundle_hash } => {
                    let mut args = vec!["update", "install", "--reboot", "no"];
                    if let Some(hash) = bundle.hash.filter(|_| *use_bundle_hash) {
                        args.extend(["--bundle-hash", hash]);
                    }
                    args.push(bundle.url);
                    run("rugix-ctrl", &args).await.map(drop)
                }
                Self::Rauc => run("rauc", &["install", bundle.url]).await.map(drop),
            }
        }
        .boxed()
    }

    /// Rugix Ctrl activates an installed bundle only when rebooting into it,
    /// while RAUC marks it as the system to boot next.
    fn deactivate(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        async move {
            match self {
                Self::Rugix { .. } => Ok(()),
                Self::Rauc => run("rauc", &["status", "mark-active", "booted"])
                    .await
                    .map(drop),
            }
        }
        .boxed()
    }

    fn info(&self) -> BoxFuture<'_, anyhow::Result<OtaUpdater>> {
        async move {
            match self {
                Self::Rugix { .. } => rugix_updater(
                    &run("rugix-ctrl", &["--version"]).await?,
                    &run("rugix-ctrl", &["system", "info"]).await?,
                ),
                Self::Rauc => rauc_updater(
                    &run("rauc", &["--version"]).await?,
                    &run("rauc", &["status", "--detailed", "--output-format=json"]).await?,
                ),
            }
        }
        .boxed()
    }

    fn reboot(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        async move {
            match self {
                Self::Rugix { .. } => run("rugix-ctrl", &["system", "reboot", "--spare"])
                    .await
                    .map(drop),
                Self::Rauc => run("systemctl", &["reboot"]).await.map(drop),
            }
        }
        .boxed()
    }
}

#[derive(Deserialize)]
struct RugixBuildInfo {
    name: String,
    release: RugixBuildRelease,
}

#[derive(Deserialize)]
struct RugixBuildRelease {
    version: String,
}

#[derive(Deserialize)]
struct RugixSystemInfo {
    boot: Option<RugixBootInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RugixBootInfo {
    active_group: Option<String>,
    default_group: Option<String>,
    #[serde(default)]
    groups: BTreeMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct RaucStatus {
    boot_primary: Option<String>,
    /// Slots as single-entry maps from the slot name to the slot.
    slots: Vec<BTreeMap<String, RaucSlot>>,
}

#[derive(Deserialize)]
struct RaucSlot {
    bootname: Option<String>,
    state: Option<String>,
    boot_status: Option<String>,
    slot_status: Option<RaucSlotStatus>,
}

#[derive(Deserialize)]
struct RaucSlotStatus {
    bundle: Option<RaucBundle>,
}

#[derive(Deserialize)]
struct RaucBundle {
    version: Option<String>,
}

/// Describe Rugix Ctrl from its version and `rugix-ctrl system info`.
///
/// Rugix Ctrl boots boot groups, which serve as the slots.
fn rugix_updater(version: &str, system_info: &str) -> anyhow::Result<OtaUpdater> {
    let info: RugixSystemInfo =
        serde_json::from_str(system_info).context("failed to parse Rugix system information")?;
    let mut updater = OtaUpdater::new("rugix").with_version(Some(tool_version(version).into()));
    if let Some(boot) = info.boot {
        let slots = boot
            .groups
            .keys()
            .map(|group| {
                let booted = boot.active_group.as_ref() == Some(group);
                OtaUpdaterSlot::new(group.clone())
                    .with_state(Some(if booted { "booted" } else { "inactive" }.into()))
            })
            .collect::<Vec<_>>();
        updater = updater
            .with_awaiting_commit(Some(boot.active_group != boot.default_group))
            .with_booted_slot(boot.active_group.map(Into::into))
            .with_primary_slot(boot.default_group.map(Into::into))
            .with_slots(Some(slots.into()));
    }
    Ok(updater)
}

/// Describe RAUC from its version and `rauc status --detailed`.
///
/// Only slots with a boot name hold bootable systems.
fn rauc_updater(version: &str, status: &str) -> anyhow::Result<OtaUpdater> {
    let status: RaucStatus =
        serde_json::from_str(status).context("failed to parse the RAUC status")?;
    let slots = status
        .slots
        .into_iter()
        .flatten()
        .filter(|(_, slot)| slot.bootname.is_some())
        .collect::<BTreeMap<_, _>>();
    let booted_slot = slots
        .iter()
        .find(|(_, slot)| slot.state.as_deref() == Some("booted"))
        .map(|(name, _)| name.clone());
    let slots = slots
        .into_iter()
        .map(|(name, slot)| {
            OtaUpdaterSlot::new(name)
                .with_state(slot.state.map(Into::into))
                .with_health(slot.boot_status.map(Into::into))
                .with_version(
                    slot.slot_status
                        .and_then(|status| status.bundle)
                        .and_then(|bundle| bundle.version)
                        .map(Into::into),
                )
        })
        .collect::<Vec<_>>();
    Ok(OtaUpdater::new("rauc")
        .with_version(Some(tool_version(version).into()))
        .with_booted_slot(booted_slot.map(Into::into))
        .with_primary_slot(status.boot_primary.map(Into::into))
        .with_slots(Some(slots.into())))
}

/// Read a field from the contents of an os-release file.
fn os_release_field(content: &str, name: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let value = line.trim().strip_prefix(name)?.strip_prefix('=')?;
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value);
        Some(value.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boot groups serve as slots, and differing groups await a commit.
    #[test]
    fn rugix_updater_from_system_info() {
        let info = r#"{"slots":{"system-a":{"active":true},"system-b":{"active":false}},
            "boot":{"bootFlow":"grub","activeGroup":"b","defaultGroup":"a",
            "groups":{"a":{},"b":{}}},"state":{"status":"Active"}}"#;
        let updater = rugix_updater("rugix-ctrl v1.3.0\n", info).unwrap();
        assert_eq!(updater.version.as_deref(), Some("1.3.0"));
        assert_eq!(updater.booted_slot.as_deref(), Some("b"));
        assert_eq!(updater.primary_slot.as_deref(), Some("a"));
        assert_eq!(updater.awaiting_commit, Some(true));
        let states = updater
            .slots
            .unwrap()
            .iter()
            .map(|slot| {
                (
                    slot.name.to_string(),
                    slot.state.as_deref().unwrap().to_owned(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            [
                ("a".into(), "inactive".into()),
                ("b".into(), "booted".into())
            ]
        );
    }

    /// Only bootable slots are reported, with their health and bundle version.
    #[test]
    fn rauc_updater_from_status() {
        let status = r#"{"compatible":"demo","booted":"A","boot_primary":"rootfs.1",
            "slots":[
            {"rootfs.1":{"bootname":"B","state":"inactive","boot_status":"good",
                "slot_status":{"bundle":{"compatible":"demo","version":"2.0"}}}},
            {"efi.0":{"bootname":null,"state":"inactive","boot_status":null}},
            {"rootfs.0":{"bootname":"A","state":"booted","boot_status":"good",
                "slot_status":{"bundle":{"compatible":null}}}}]}"#;
        let updater = rauc_updater("rauc 1.15.2\n", status).unwrap();
        assert_eq!(updater.version.as_deref(), Some("1.15.2"));
        assert_eq!(updater.booted_slot.as_deref(), Some("rootfs.0"));
        assert_eq!(updater.primary_slot.as_deref(), Some("rootfs.1"));
        assert_eq!(updater.awaiting_commit, None);
        let slots = updater.slots.unwrap();
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[1].name.as_str(), "rootfs.1");
        assert_eq!(slots[1].version.as_deref(), Some("2.0"));
        assert_eq!(slots[1].health.as_deref(), Some("good"));
    }

    /// Values may be quoted or unquoted, and prefixes of other names do not match.
    #[test]
    fn os_release_fields() {
        let content = "IMAGE_ID_LIKE=other\nIMAGE_ID=\"gateway-os\"\nIMAGE_VERSION=2.1\n";
        assert_eq!(
            os_release_field(content, "IMAGE_ID").as_deref(),
            Some("gateway-os")
        );
        assert_eq!(
            os_release_field(content, "IMAGE_VERSION").as_deref(),
            Some("2.1")
        );
        assert_eq!(os_release_field(content, "VERSION_ID"), None);
    }
}
