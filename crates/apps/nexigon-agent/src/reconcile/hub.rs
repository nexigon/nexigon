//! Access to Nexigon Hub for the reconciliation loops.
//!
//! [`Hub`] reads and writes the device's properties, resolves version paths,
//! looks up package versions and their assets, and publishes device events.

use std::collections::HashMap;

use anyhow::Context;
use anyhow::anyhow;
use jiff::Timestamp;
use nexigon_api::Action;
use nexigon_api::types::DeviceGroupMarker;
use nexigon_api::types::devices::DeviceEvent;
use nexigon_api::types::devices::DeviceEventSeverity;
use nexigon_api::types::devices::DeviceProperty;
use nexigon_api::types::devices::GetDevicePropertyAction;
use nexigon_api::types::devices::GetDevicePropertyOutput;
use nexigon_api::types::devices::PublishDeviceEventsAction;
use nexigon_api::types::devices::SetDevicePropertyAction;
use nexigon_api::types::devices::SetDevicePropertyOutput;
use nexigon_api::types::repositories::GetPackageVersionDetailsAction;
use nexigon_api::types::repositories::GetPackageVersionDetailsOutput;
use nexigon_api::types::repositories::IssueAssetDownloadUrlAction;
use nexigon_api::types::repositories::PackageVersionAsset;
use nexigon_api::types::repositories::PackageVersionId;
use nexigon_api::types::repositories::RepositoryAssetStatus;
use nexigon_api::types::repositories::ResolvePackageVersionByPathAction;
use nexigon_api::types::repositories::ResolvePackageVersionByPathOutput;
use nexigon_ids::Generate;
use nexigon_ids::ids::DeviceEventId;
use nexigon_ids::ids::DeviceId;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::warn;

use crate::device_api::DeviceExecutor;

/// Nexigon Hub as seen by one device.
pub(crate) struct Hub {
    executor: DeviceExecutor,
    device_id: DeviceId,
    /// Package versions fetched since the cache was last cleared.
    details: HashMap<PackageVersionId, GetPackageVersionDetailsOutput>,
}

/// Result of a conditional property write.
pub(crate) enum PropertyWrite {
    /// The property has been written. Nexigon Hub 2026.3 and earlier do not
    /// report its new version.
    Written { version: Option<u64> },
    /// The property changed or was deleted since the expected version.
    Conflict,
}

impl Hub {
    pub(crate) fn new(executor: DeviceExecutor, device_id: DeviceId) -> Self {
        Self {
            executor,
            device_id,
            details: HashMap::new(),
        }
    }

    pub(crate) fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Forget fetched package versions, which may be deleted or rebased later.
    pub(crate) fn clear_cache(&mut self) {
        self.details.clear();
    }

    /// Read a device property, or return `None` if it does not exist.
    pub(crate) async fn property(&mut self, name: &str) -> anyhow::Result<Option<DeviceProperty>> {
        let output = call(
            &mut self.executor,
            GetDevicePropertyAction::new(self.device_id.clone(), name.to_owned()),
        )
        .await?;
        Ok(match output {
            GetDevicePropertyOutput::Found(property) => Some(property),
            GetDevicePropertyOutput::NotFound => None,
        })
    }

    /// Write a device property.
    ///
    /// With an expected version, the write only succeeds if the property still
    /// has that version.
    pub(crate) async fn set_property(
        &mut self,
        name: &str,
        value: &impl Serialize,
        expected_version: Option<u64>,
    ) -> anyhow::Result<PropertyWrite> {
        let value = serde_json::to_value(value)
            .with_context(|| format!("failed to serialize property {name}"))?;
        let action =
            SetDevicePropertyAction::new(self.device_id.clone(), name.to_owned(), value.into())
                .with_expected_version(expected_version);
        match call(&mut self.executor, action).await? {
            SetDevicePropertyOutput::Set(success) => Ok(PropertyWrite::Written {
                version: success.version,
            }),
            SetDevicePropertyOutput::VersionConflict(_) | SetDevicePropertyOutput::Deleted => {
                Ok(PropertyWrite::Conflict)
            }
        }
    }

    /// Resolve a version path, or return `None` if it does not exist.
    pub(crate) async fn resolve(
        &mut self,
        path: &VersionPath,
    ) -> anyhow::Result<Option<PackageVersionId>> {
        let action = ResolvePackageVersionByPathAction::new(
            path.repository.clone(),
            path.package.clone(),
            path.version.clone(),
        );
        // Version strings that are not valid tags cannot resolve. The future is
        // boxed for the reason given at `call`.
        match Box::pin(self.executor.execute(action))
            .await
            .context("failed to reach Nexigon Hub")?
        {
            Ok(ResolvePackageVersionByPathOutput::Found(found)) => Ok(Some(found.version_id)),
            Ok(ResolvePackageVersionByPathOutput::NotFound) | Err(_) => Ok(None),
        }
    }

    pub(crate) async fn version_details(
        &mut self,
        version_id: &PackageVersionId,
    ) -> anyhow::Result<&GetPackageVersionDetailsOutput> {
        if !self.details.contains_key(version_id) {
            let details = call(
                &mut self.executor,
                GetPackageVersionDetailsAction::new(version_id.clone()),
            )
            .await?;
            self.details.insert(version_id.clone(), details);
        }
        Ok(&self.details[version_id])
    }

    /// Asset of a version with the given filename, if it has been uploaded.
    pub(crate) async fn available_asset(
        &mut self,
        version_id: &PackageVersionId,
        filename: &str,
    ) -> anyhow::Result<Option<PackageVersionAsset>> {
        Ok(self
            .version_details(version_id)
            .await?
            .assets
            .iter()
            .find(|asset| {
                *asset.filename == *filename
                    && matches!(asset.status, RepositoryAssetStatus::Available)
            })
            .cloned())
    }

    /// Issue a URL from which the device can download an asset.
    pub(crate) async fn download_url(
        &mut self,
        asset: &PackageVersionAsset,
    ) -> anyhow::Result<String> {
        let action = IssueAssetDownloadUrlAction::new(asset.asset_id.clone())
            .with_filename(Some(asset.filename.clone()));
        Ok(call(&mut self.executor, action).await?.url.to_string())
    }

    /// Publish a device event, logging failures instead of failing the caller.
    pub(crate) async fn event(
        &mut self,
        category: &str,
        severity: DeviceEventSeverity,
        message: &str,
        attributes: sidex::types::Map<sidex::types::Str, sidex::types::JsonValue>,
    ) {
        let event = DeviceEvent::new(
            DeviceEventId::generate(),
            severity,
            message.into(),
            attributes,
            Timestamp::now().into(),
        )
        .with_category(Some(category.into()));
        let action = PublishDeviceEventsAction::new(self.device_id.clone(), vec![event].into());
        if let Err(error) = call(&mut self.executor, action).await {
            warn!(?error, "failed to publish a device event");
        }
    }
}

/// Repository, package, and tag or ID of a version.
#[derive(Debug, Clone)]
pub(crate) struct VersionPath {
    pub repository: String,
    pub package: String,
    pub version: String,
}

impl VersionPath {
    pub(crate) fn parse(path: &str) -> anyhow::Result<Self> {
        let mut parts = path.split('/');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(repository), Some(package), Some(version), None)
                if !repository.is_empty() && !package.is_empty() && !version.is_empty() =>
            {
                Ok(Self {
                    repository: repository.to_owned(),
                    package: package.to_owned(),
                    version: version.to_owned(),
                })
            }
            _ => Err(anyhow!(
                "invalid version path {path:?}, expected <repository>/<package>/<version>"
            )),
        }
    }

    pub(crate) fn with_version(&self, version: &str) -> Self {
        Self {
            version: version.to_owned(),
            ..self.clone()
        }
    }
}

impl std::fmt::Display for VersionPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.repository, self.package, self.version)
    }
}

/// Hash of a Rugix bundle from the metadata of its asset, if any.
pub(crate) fn rugix_bundle_hash(asset: &PackageVersionAsset) -> Option<String> {
    metadata::<AssetMetadata>(&asset.metadata)?
        .rugix?
        .bundle_hash
}

/// Read the metadata of a version or asset, or `None` if it does not match.
pub(crate) fn metadata<T: DeserializeOwned>(metadata: &impl Serialize) -> Option<T> {
    serde_json::to_value(metadata)
        .and_then(serde_json::from_value)
        .ok()
}

/// Metadata of a bundle asset.
#[derive(Deserialize)]
struct AssetMetadata {
    rugix: Option<RugixAssetMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RugixAssetMetadata {
    /// Hash of the bundle for Rugix Ctrl to verify.
    bundle_hash: Option<String>,
}

/// Execute an action, failing if Nexigon Hub is unreachable or rejects it.
///
/// Action futures are large. Boxing them keeps the futures of checks small,
/// which would otherwise overflow the stack of unoptimized builds.
async fn call<A: Action + DeviceGroupMarker>(
    executor: &mut DeviceExecutor,
    action: A,
) -> anyhow::Result<A::Output> {
    Box::pin(executor.execute(action))
        .await
        .context("failed to reach Nexigon Hub")?
        .map_err(|error| anyhow!("Nexigon Hub rejected the request: {}", error.message))
}
