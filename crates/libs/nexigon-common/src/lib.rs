use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use clap::Subcommand;

use nexigon_api::types::repositories::AddPackageAssetAction;
use nexigon_api::types::repositories::AddPackageVersionAssetAction;
use nexigon_api::types::repositories::AddTagItem;
use nexigon_api::types::repositories::CreatePackageAction;
use nexigon_api::types::repositories::CreatePackageVersionAction;
use nexigon_api::types::repositories::CreateRepositoryAction;
use nexigon_api::types::repositories::DeleteAssetAction;
use nexigon_api::types::repositories::DeletePackageAction;
use nexigon_api::types::repositories::DeletePackageVersionAction;
use nexigon_api::types::repositories::DeleteRepositoryAction;
use nexigon_api::types::repositories::GetAssetDetailsAction;
use nexigon_api::types::repositories::GetPackageDetailsAction;
use nexigon_api::types::repositories::GetPackageVersionDetailsAction;
use nexigon_api::types::repositories::GetPackageVersionDetailsOutput;
use nexigon_api::types::repositories::GetRepositoryDetailsAction;
use nexigon_api::types::repositories::GetRepositoryS3ConfigAction;
use nexigon_api::types::repositories::IssueAssetDownloadUrlAction;
use nexigon_api::types::repositories::IssueAssetUploadUrlAction;
use nexigon_api::types::repositories::QueryPackageVersionsAction;
use nexigon_api::types::repositories::QueryRepositoryAssetsAction;
use nexigon_api::types::repositories::QueryRepositoryPackagesAction;
use nexigon_api::types::repositories::QueryRepositoryProjectsAction;
use nexigon_api::types::repositories::RemovePackageAssetAction;
use nexigon_api::types::repositories::RemovePackageVersionAssetAction;
use nexigon_api::types::repositories::RemoveTagItem;
use nexigon_api::types::repositories::RepositoryAssetId;
use nexigon_api::types::repositories::RepositoryS3Config;
use nexigon_api::types::repositories::RepositoryVisibility;
use nexigon_api::types::repositories::ResolvePackageByPathAction;
use nexigon_api::types::repositories::ResolvePackageVersionAssetByPathAction;
use nexigon_api::types::repositories::ResolvePackageVersionByPathAction;
use nexigon_api::types::repositories::ResolvePackageVersionByPathOutput;
use nexigon_api::types::repositories::ResolveRepositoryNameAction;
use nexigon_api::types::repositories::ResolveRepositoryNameOutput;
use nexigon_api::types::repositories::SetPackageAssetMetadataAction;
use nexigon_api::types::repositories::SetPackageKindAction;
use nexigon_api::types::repositories::SetPackageMetadataAction;
use nexigon_api::types::repositories::SetPackageNameAction;
use nexigon_api::types::repositories::SetPackageVersionAssetMetadataAction;
use nexigon_api::types::repositories::SetPackageVersionMetadataAction;
use nexigon_api::types::repositories::SetPackageVersionNameAction;
use nexigon_api::types::repositories::SetRepositoryDisplayNameAction;
use nexigon_api::types::repositories::SetRepositoryS3ConfigAction;
use nexigon_api::types::repositories::SetRepositorySlugAction;
use nexigon_api::types::repositories::SetRepositoryVisibilityAction;
use nexigon_api::types::repositories::TagPackageVersionAction;
use nexigon_api::types::repositories::UntagPackageVersionAction;
use nexigon_client::Execute;
use nexigon_ids::ids::OrganizationId;
use nexigon_ids::ids::PackageId;
use nexigon_ids::ids::PackageVersionId;
use nexigon_ids::ids::RepositoryId;

mod repository_upload;
pub mod secure_file;

// ── Value parsing helpers ────────────────────────────────────────────

fn parse_json_object(s: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("invalid JSON: {e}"))?;
    if !value.is_object() {
        return Err("metadata must be a JSON object".to_owned());
    }
    Ok(value)
}

fn json_value_to_map(
    value: &serde_json::Value,
) -> std::collections::HashMap<String, serde_json::Value> {
    value
        .as_object()
        .expect("metadata must be a JSON object")
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

// ── Path parsing ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetPath {
    pub repository: String,
    pub package: String,
    pub tag: String,
    pub filename: String,
}

/// Parse `repository/package/tag/filename`, preserving non-empty nested filename
/// components.
pub fn parse_asset_path(path: &str) -> anyhow::Result<AssetPath> {
    let mut parts_iter = path.split('/');
    let repository = next_path_component(&mut parts_iter, "repository")?.to_owned();
    let package = next_path_component(&mut parts_iter, "package")?.to_owned();
    let tag = next_path_component(&mut parts_iter, "version tag")?.to_owned();
    let filename_parts = parts_iter.collect::<Vec<_>>();
    if filename_parts.is_empty() {
        bail!("missing filename");
    }
    if filename_parts.iter().any(|part| part.is_empty()) {
        bail!("filename components must not be empty");
    }
    let filename = filename_parts.join("/");
    Ok(AssetPath {
        repository,
        package,
        tag,
        filename,
    })
}

impl std::fmt::Display for AssetPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}/{}/{}",
            self.repository, self.package, self.tag, self.filename
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionPath {
    pub repository: String,
    pub package: String,
    pub tag: String,
}

/// Parse `repository/package/tag`.
pub fn parse_version_path(path: &str) -> anyhow::Result<VersionPath> {
    let mut parts_iter = path.split('/');
    let repository = next_path_component(&mut parts_iter, "repository")?.to_owned();
    let package = next_path_component(&mut parts_iter, "package")?.to_owned();
    let tag = next_path_component(&mut parts_iter, "version tag")?.to_owned();
    if parts_iter.next().is_some() {
        bail!("too many parts in version path");
    }
    Ok(VersionPath {
        repository,
        package,
        tag,
    })
}

impl std::fmt::Display for VersionPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.repository, self.package, self.tag)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackagePath {
    pub repository: String,
    pub package: String,
}

/// Parse `repository/package`.
pub fn parse_package_path(path: &str) -> anyhow::Result<PackagePath> {
    let mut parts_iter = path.split('/');
    let repository = next_path_component(&mut parts_iter, "repository")?.to_owned();
    let package = next_path_component(&mut parts_iter, "package")?.to_owned();
    if parts_iter.next().is_some() {
        bail!("too many parts in package path");
    }
    Ok(PackagePath {
        repository,
        package,
    })
}

impl std::fmt::Display for PackagePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.repository, self.package)
    }
}

fn next_path_component<'a>(
    parts: &mut impl Iterator<Item = &'a str>,
    name: &str,
) -> anyhow::Result<&'a str> {
    let part = parts.next().ok_or_else(|| anyhow!("missing {name}"))?;
    if part.is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(part)
}

fn parse_complete_id<T: FromStr>(value: &str) -> Option<T> {
    value.parse().ok()
}

// ── Resolution helpers ───────────────────────────────────────────────

pub async fn resolve_repository(
    executor: &mut impl Execute,
    repository: &str,
) -> anyhow::Result<RepositoryId> {
    if let Some(repository_id) = parse_complete_id::<RepositoryId>(repository) {
        return Ok(repository_id);
    }
    let output = executor
        .execute(ResolveRepositoryNameAction::new(repository.to_owned()))
        .await??;
    match output {
        ResolveRepositoryNameOutput::Found(id) => Ok(id),
        ResolveRepositoryNameOutput::NotFound => {
            bail!("repository {repository} not found")
        }
    }
}

pub async fn resolve_package(
    executor: &mut impl Execute,
    package: &str,
) -> anyhow::Result<PackageId> {
    if let Some(package_id) = parse_complete_id::<PackageId>(package) {
        return Ok(package_id);
    }
    let path = parse_package_path(package)?;
    let output = executor
        .execute(ResolvePackageByPathAction::new(
            path.repository.clone(),
            path.package.clone(),
        ))
        .await??;
    match output {
        nexigon_api::types::repositories::ResolvePackageByPathOutput::Found(output) => {
            Ok(output.package_id)
        }
        nexigon_api::types::repositories::ResolvePackageByPathOutput::NotFound => {
            bail!(
                "package {} not found in repository {}",
                path.package,
                path.repository
            )
        }
    }
}

pub async fn resolve_asset(
    executor: &mut impl Execute,
    asset: &str,
) -> anyhow::Result<RepositoryAssetId> {
    if let Some(asset_id) = parse_complete_id::<RepositoryAssetId>(asset) {
        return Ok(asset_id);
    }
    let path = parse_asset_path(asset)?;
    let output = executor
        .execute(ResolvePackageVersionAssetByPathAction::new(
            path.repository,
            path.package,
            path.tag,
            path.filename,
        ))
        .await??;
    match output {
        nexigon_api::types::repositories::ResolvePackageVersionAssetByPathOutput::Found(output) => {
            Ok(output.asset_id)
        }
        nexigon_api::types::repositories::ResolvePackageVersionAssetByPathOutput::NotFound => {
            bail!("package version asset {asset} not found")
        }
    }
}

pub async fn resolve_version(
    executor: &mut impl Execute,
    version: &str,
) -> anyhow::Result<PackageVersionId> {
    if let Some(version_id) = parse_complete_id::<PackageVersionId>(version) {
        return Ok(version_id);
    }
    let path = parse_version_path(version)?;
    let output = executor
        .execute(ResolvePackageVersionByPathAction::new(
            path.repository,
            path.package,
            path.tag,
        ))
        .await??;
    match output {
        ResolvePackageVersionByPathOutput::Found(output) => Ok(output.version_id),
        ResolvePackageVersionByPathOutput::NotFound => {
            bail!("package version {version} not found")
        }
    }
}

pub async fn get_version_details(
    executor: &mut impl Execute,
    version_id: PackageVersionId,
) -> anyhow::Result<GetPackageVersionDetailsOutput> {
    Ok(executor
        .execute(GetPackageVersionDetailsAction::new(version_id))
        .await??)
}

// ── Shared repositories CLI ──────────────────────────────────────────

/// Argument describing a tag to add.
#[derive(Debug, Clone)]
pub struct AddTagArg(pub AddTagItem);

impl FromStr for AddTagArg {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split(',');
        let tag = parts
            .next()
            .ok_or_else(|| anyhow!("missing tag"))?
            .to_string();
        let mut locked = false;
        let mut reassign = false;
        for part in parts {
            match part {
                "locked" => locked = true,
                "reassign" => reassign = true,
                _ => bail!("unknown tag option: {part}"),
            }
        }
        Ok(Self(
            AddTagItem::new(tag)
                .with_locked(Some(locked))
                .with_reassign(Some(reassign)),
        ))
    }
}

/// Repository subcommand.
#[derive(Debug, Subcommand)]
pub enum RepositoriesCmd {
    /// Resolve a repository by name.
    Resolve {
        /// Repository name or ID.
        repository: String,
    },
    /// Get repository details.
    Info {
        /// Repository name or ID.
        repository: String,
    },
    /// Create a new repository.
    Create {
        /// Organization ID.
        organization: OrganizationId,
        /// Display name.
        display_name: String,
        /// Repository visibility.
        #[clap(long, value_parser = parse_repository_visibility)]
        visibility: Option<RepositoryVisibility>,
    },
    /// Delete a repository.
    Delete {
        /// Repository name or ID.
        repository: String,
    },
    /// Rename a repository.
    Rename {
        /// Repository name or ID.
        repository: String,
        /// New display name.
        display_name: String,
    },
    /// Set repository visibility.
    SetVisibility {
        /// Repository name or ID.
        repository: String,
        /// New visibility.
        #[clap(value_parser = parse_repository_visibility)]
        visibility: RepositoryVisibility,
    },
    /// Set repository slug.
    SetSlug {
        /// Repository name or ID.
        repository: String,
        /// New slug.
        slug: String,
    },
    /// Request a pre-signed URL for downloading an asset.
    IssueUrl {
        /// Asset ID or path (repository/package/tag/filename).
        asset: String,
        /// Optional filename for the download URL.
        #[clap(long)]
        filename: Option<String>,
    },
    /// Manage linked projects.
    #[clap(subcommand)]
    Projects(RepositoryProjectsCmd),
    /// Manage S3 configuration.
    #[clap(subcommand)]
    S3(RepositoryS3Cmd),
    /// Manage repository assets.
    #[clap(subcommand)]
    Assets(AssetsCmd),
    /// Manage packages.
    #[clap(subcommand)]
    Packages(PackagesCmd),
    /// Manage package versions.
    #[clap(subcommand)]
    Versions(VersionsCmd),
}

/// Repository projects subcommand.
#[derive(Debug, Subcommand)]
pub enum RepositoryProjectsCmd {
    /// List projects linked to a repository.
    List {
        /// Repository name or ID.
        repository: String,
    },
}

/// Repository S3 subcommand.
#[derive(Debug, Subcommand)]
pub enum RepositoryS3Cmd {
    /// Get S3 configuration.
    Get {
        /// Repository name or ID.
        repository: String,
    },
    /// Set S3 configuration from a JSON object.
    Set {
        /// Repository name or ID.
        repository: String,
        /// S3 config JSON.
        config: String,
    },
}

/// Packages subcommand.
#[derive(Debug, Subcommand)]
pub enum PackagesCmd {
    /// List packages in a repository.
    List {
        /// Repository name or ID.
        repository: String,
    },
    /// Get package details.
    Info {
        /// Package path or ID.
        package: String,
    },
    /// Create a new package.
    Create {
        /// Repository name or ID.
        repository: String,
        /// Package name.
        name: String,
        /// Optional package kind.
        #[clap(long)]
        kind: Option<String>,
        /// Optional JSON metadata.
        #[clap(long, value_parser = parse_json_object)]
        metadata: Option<serde_json::Value>,
    },
    /// Delete a package.
    Delete {
        /// Package path or ID.
        package: String,
    },
    /// Rename a package.
    Rename {
        /// Package path or ID.
        package: String,
        /// New package name.
        name: String,
    },
    /// Set package kind.
    SetKind {
        /// Package path or ID.
        package: String,
        /// New package kind. Omit to clear.
        kind: Option<String>,
    },
    /// Set package metadata from a JSON object.
    SetMetadata {
        /// Package path or ID.
        package: String,
        /// Metadata JSON object.
        #[clap(value_parser = parse_json_object)]
        metadata: serde_json::Value,
    },
    /// Manage package assets.
    #[clap(subcommand)]
    Assets(PackageAssetsCmd),
    /// Manage package versions.
    #[clap(subcommand)]
    Versions(PackageVersionsCmd),
}

/// Package assets subcommand.
#[derive(Debug, Subcommand)]
pub enum PackageAssetsCmd {
    /// Add an asset to a package.
    Add {
        /// Package path or ID.
        package: String,
        /// Asset ID.
        asset_id: RepositoryAssetId,
        /// Asset filename.
        filename: String,
        /// Optional JSON metadata.
        #[clap(long, value_parser = parse_json_object)]
        metadata: Option<serde_json::Value>,
    },
    /// Remove an asset from a package.
    Remove {
        /// Package path or ID.
        package: String,
        /// Asset filename.
        filename: String,
    },
    /// Set package asset metadata from a JSON object.
    SetMetadata {
        /// Package path or ID.
        package: String,
        /// Asset filename.
        filename: String,
        /// Metadata JSON object.
        #[clap(value_parser = parse_json_object)]
        metadata: serde_json::Value,
    },
}

/// Package versions nested under packages.
#[derive(Debug, Subcommand)]
pub enum PackageVersionsCmd {
    /// List package versions.
    List {
        /// Package path or ID.
        package: String,
    },
}

/// Package versions subcommand.
#[derive(Debug, Subcommand)]
pub enum VersionsCmd {
    /// Resolve a package version by path (repository/package/tag).
    Resolve {
        /// Version path (repository/package/tag) to resolve.
        version: String,
    },
    /// Get detailed information about a package version.
    Info {
        /// Version ID or path (repository/package/tag).
        version: String,
    },
    /// Create a new package version.
    Create {
        /// Package path or ID.
        package: String,
        /// Optional version name.
        #[clap(long)]
        name: Option<String>,
        /// Tags to add.
        #[clap(long = "tag")]
        tags: Vec<AddTagArg>,
        /// Optional JSON metadata.
        #[clap(long, value_parser = parse_json_object)]
        metadata: Option<serde_json::Value>,
    },
    /// Delete a package version.
    Delete {
        /// Package version path or ID.
        version: String,
    },
    /// Rename a package version.
    Rename {
        /// Package version path or ID.
        version: String,
        /// New version name. Omit to clear.
        name: Option<String>,
    },
    /// Set package version metadata from a JSON object.
    SetMetadata {
        /// Package version path or ID.
        version: String,
        /// Metadata JSON object.
        #[clap(value_parser = parse_json_object)]
        metadata: serde_json::Value,
    },
    /// Add tags to a version.
    Tag {
        /// Package version path or ID.
        version: String,
        /// Tags to add.
        #[clap(long = "tag")]
        tags: Vec<AddTagArg>,
    },
    /// Remove tags from a version.
    Untag {
        /// Package version path or ID.
        version: String,
        /// Tags to remove.
        #[clap(long = "tag")]
        tags: Vec<String>,
    },
    /// Manage the assets of a package version.
    #[clap(subcommand)]
    Assets(VersionAssetsCmd),
}

/// Version assets subcommand.
#[derive(Debug, Subcommand)]
pub enum VersionAssetsCmd {
    /// Add an asset to a package version.
    Add {
        /// Package version path or ID.
        version: String,
        /// Asset ID.
        asset_id: RepositoryAssetId,
        /// Asset filename.
        filename: String,
        /// Optional JSON metadata.
        #[clap(long, value_parser = parse_json_object)]
        metadata: Option<serde_json::Value>,
    },
    /// Remove an asset from a package version.
    Remove {
        /// Package version path or ID.
        version: String,
        /// Asset filename.
        filename: String,
    },
    /// Set version asset metadata from a JSON object.
    SetMetadata {
        /// Package version path or ID.
        version: String,
        /// Asset filename.
        filename: String,
        /// Metadata JSON object.
        #[clap(value_parser = parse_json_object)]
        metadata: serde_json::Value,
    },
}

/// Assets subcommand.
#[derive(Debug, Subcommand)]
pub enum AssetsCmd {
    /// List assets in a repository.
    List {
        /// Repository name or ID.
        repository: String,
    },
    /// Get asset details.
    Info {
        /// Asset ID or path (repository/package/tag/filename).
        asset: String,
    },
    /// Upload an asset to the repository.
    Upload {
        /// Repository name or ID.
        repository: String,
        /// Path to the asset.
        path: PathBuf,
    },
    /// Delete an asset.
    Delete {
        /// Asset ID or path (repository/package/tag/filename).
        asset: String,
    },
    /// Request a pre-signed URL for uploading an asset.
    IssueUploadUrl {
        /// Asset ID or path (repository/package/tag/filename).
        asset: String,
    },
}

/// Execute a [`RepositoriesCmd`].
pub async fn execute_repositories_cmd(
    cmd: &RepositoriesCmd,
    executor: &mut impl Execute,
) -> anyhow::Result<()> {
    match cmd {
        RepositoriesCmd::Resolve { repository } => {
            let output = executor
                .execute(ResolveRepositoryNameAction::new(repository.clone()))
                .await
                .context("resolving repository")??;
            write_json(&output);
        }
        RepositoriesCmd::Info { repository } => {
            let repository_id = resolve_repository(executor, repository).await?;
            let output = executor
                .execute(GetRepositoryDetailsAction::new(repository_id))
                .await
                .context("getting repository details")??;
            write_json(&output);
        }
        RepositoriesCmd::Create {
            organization,
            display_name,
            visibility,
        } => {
            let output = executor
                .execute(
                    CreateRepositoryAction::new(organization.clone(), display_name.clone())
                        .with_visibility(visibility.clone()),
                )
                .await
                .context("creating repository")??;
            write_json(&output);
        }
        RepositoriesCmd::Delete { repository } => {
            let repository_id = resolve_repository(executor, repository).await?;
            let output = executor
                .execute(DeleteRepositoryAction::new(repository_id))
                .await
                .context("deleting repository")??;
            write_json(&output);
        }
        RepositoriesCmd::Rename {
            repository,
            display_name,
        } => {
            let repository_id = resolve_repository(executor, repository).await?;
            let output = executor
                .execute(SetRepositoryDisplayNameAction::new(
                    repository_id,
                    display_name.clone(),
                ))
                .await
                .context("renaming repository")??;
            write_json(&output);
        }
        RepositoriesCmd::SetVisibility {
            repository,
            visibility,
        } => {
            let repository_id = resolve_repository(executor, repository).await?;
            let output = executor
                .execute(SetRepositoryVisibilityAction::new(
                    repository_id,
                    visibility.clone(),
                ))
                .await
                .context("setting repository visibility")??;
            write_json(&output);
        }
        RepositoriesCmd::SetSlug { repository, slug } => {
            let repository_id = resolve_repository(executor, repository).await?;
            let output = executor
                .execute(SetRepositorySlugAction::new(repository_id, slug.clone()))
                .await
                .context("setting repository slug")??;
            write_json(&output);
        }
        RepositoriesCmd::IssueUrl { asset, filename } => {
            let asset_id = resolve_asset(executor, asset).await?;
            let output = executor
                .execute(IssueAssetDownloadUrlAction::new(asset_id).with_filename(filename.clone()))
                .await
                .context("unable to issue asset download URL")??;
            write_json(&output);
        }
        RepositoriesCmd::Projects(cmd) => match cmd {
            RepositoryProjectsCmd::List { repository } => {
                let repository_id = resolve_repository(executor, repository).await?;
                let output = executor
                    .execute(QueryRepositoryProjectsAction::new(repository_id))
                    .await
                    .context("querying repository projects")??;
                write_json(&output);
            }
        },
        RepositoriesCmd::S3(cmd) => match cmd {
            RepositoryS3Cmd::Get { repository } => {
                let repository_id = resolve_repository(executor, repository).await?;
                let output = executor
                    .execute(GetRepositoryS3ConfigAction::new(repository_id))
                    .await
                    .context("getting repository S3 config")??;
                write_json(&output);
            }
            RepositoryS3Cmd::Set { repository, config } => {
                let repository_id = resolve_repository(executor, repository).await?;
                let config = serde_json::from_str::<RepositoryS3Config>(config)
                    .context("repository S3 config must be valid JSON")?;
                let output = executor
                    .execute(SetRepositoryS3ConfigAction::new(repository_id, config))
                    .await
                    .context("setting repository S3 config")??;
                write_json(&output);
            }
        },
        RepositoriesCmd::Packages(cmd) => match cmd {
            PackagesCmd::List { repository } => {
                let repository_id = resolve_repository(executor, repository).await?;
                let output = executor
                    .execute(QueryRepositoryPackagesAction::new(repository_id))
                    .await
                    .context("querying repository packages")??;
                write_json(&output);
            }
            PackagesCmd::Info { package } => {
                let package_id = resolve_package(executor, package).await?;
                let output = executor
                    .execute(GetPackageDetailsAction::new(package_id))
                    .await
                    .context("getting package details")??;
                write_json(&output);
            }
            PackagesCmd::Create {
                repository,
                name,
                kind,
                metadata,
            } => {
                let repository_id = resolve_repository(executor, repository).await?;
                let metadata = metadata.as_ref().map(json_value_to_map);
                let output = executor
                    .execute(
                        CreatePackageAction::new(repository_id.clone(), name.to_owned())
                            .with_kind(kind.clone())
                            .with_metadata(metadata),
                    )
                    .await
                    .context("creating package")??;
                write_json(&output);
            }
            PackagesCmd::Delete { package } => {
                let package_id = resolve_package(executor, package).await?;
                let output = executor
                    .execute(DeletePackageAction::new(package_id.clone()))
                    .await
                    .context("deleting package")??;
                write_json(&output);
            }
            PackagesCmd::Rename { package, name } => {
                let package_id = resolve_package(executor, package).await?;
                let output = executor
                    .execute(SetPackageNameAction::new(package_id, name.clone()))
                    .await
                    .context("renaming package")??;
                write_json(&output);
            }
            PackagesCmd::SetKind { package, kind } => {
                let package_id = resolve_package(executor, package).await?;
                let output = executor
                    .execute(SetPackageKindAction::new(package_id).with_kind(kind.clone()))
                    .await
                    .context("setting package kind")??;
                write_json(&output);
            }
            PackagesCmd::SetMetadata { package, metadata } => {
                let package_id = resolve_package(executor, package).await?;
                let output = executor
                    .execute(SetPackageMetadataAction::new(
                        package_id,
                        json_value_to_map(metadata),
                    ))
                    .await
                    .context("setting package metadata")??;
                write_json(&output);
            }
            PackagesCmd::Assets(cmd) => match cmd {
                PackageAssetsCmd::Add {
                    package,
                    asset_id,
                    filename,
                    metadata,
                } => {
                    let package_id = resolve_package(executor, package).await?;
                    let metadata = metadata.as_ref().map(json_value_to_map);
                    let output = executor
                        .execute(
                            AddPackageAssetAction::new(
                                package_id,
                                asset_id.clone(),
                                filename.clone(),
                            )
                            .with_metadata(metadata),
                        )
                        .await
                        .context("adding package asset")??;
                    write_json(&output);
                }
                PackageAssetsCmd::Remove { package, filename } => {
                    let package_id = resolve_package(executor, package).await?;
                    let output = executor
                        .execute(RemovePackageAssetAction::new(package_id, filename.clone()))
                        .await
                        .context("removing package asset")??;
                    write_json(&output);
                }
                PackageAssetsCmd::SetMetadata {
                    package,
                    filename,
                    metadata,
                } => {
                    let package_id = resolve_package(executor, package).await?;
                    let output = executor
                        .execute(SetPackageAssetMetadataAction::new(
                            package_id,
                            filename.clone(),
                            json_value_to_map(metadata),
                        ))
                        .await
                        .context("setting package asset metadata")??;
                    write_json(&output);
                }
            },
            PackagesCmd::Versions(cmd) => match cmd {
                PackageVersionsCmd::List { package } => {
                    let package_id = resolve_package(executor, package).await?;
                    let output = executor
                        .execute(QueryPackageVersionsAction::new(package_id))
                        .await
                        .context("querying package versions")??;
                    write_json(&output);
                }
            },
        },
        RepositoriesCmd::Versions(cmd) => execute_versions_cmd(cmd, executor).await?,
        RepositoriesCmd::Assets(cmd) => match cmd {
            AssetsCmd::List { repository } => {
                let repository_id = resolve_repository(executor, repository).await?;
                let output = executor
                    .execute(QueryRepositoryAssetsAction::new(repository_id))
                    .await
                    .context("querying repository assets")??;
                write_json(&output);
            }
            AssetsCmd::Info { asset } => {
                let asset_id = resolve_asset(executor, asset).await?;
                let output = executor
                    .execute(GetAssetDetailsAction::new(asset_id))
                    .await
                    .context("getting asset details")??;
                write_json(&output);
            }
            AssetsCmd::Upload { repository, path } => {
                let repository_id = resolve_repository(executor, repository).await?;
                let output =
                    repository_upload::upload_repository_asset(executor, repository_id, path)
                        .await?;
                write_json(&output);
            }
            AssetsCmd::Delete { asset } => {
                let asset_id = resolve_asset(executor, asset).await?;
                let output = executor
                    .execute(DeleteAssetAction::new(asset_id))
                    .await
                    .context("deleting asset")??;
                write_json(&output);
            }
            AssetsCmd::IssueUploadUrl { asset } => {
                let asset_id = resolve_asset(executor, asset).await?;
                let output = executor
                    .execute(IssueAssetUploadUrlAction::new(asset_id))
                    .await
                    .context("issuing asset upload URL")??;
                write_json(&output);
            }
        },
    }
    Ok(())
}

/// Execute a [`VersionsCmd`].
pub async fn execute_versions_cmd(
    cmd: &VersionsCmd,
    executor: &mut impl Execute,
) -> anyhow::Result<()> {
    match cmd {
        VersionsCmd::Resolve { version } => {
            let path = parse_version_path(version)?;
            let output = executor
                .execute(ResolvePackageVersionByPathAction::new(
                    path.repository,
                    path.package,
                    path.tag,
                ))
                .await
                .context("resolving package version")??;
            write_json(&output);
        }
        VersionsCmd::Info { version } => {
            let version_id = resolve_version(executor, version).await?;
            let output = get_version_details(executor, version_id).await?;
            write_json(&output);
        }
        VersionsCmd::Create {
            package,
            name,
            tags,
            metadata,
        } => {
            let package_id = resolve_package(executor, package).await?;
            let metadata = metadata.as_ref().map(json_value_to_map);
            let output = executor
                .execute(
                    CreatePackageVersionAction::new(package_id.clone())
                        .with_name(name.clone())
                        .with_tags(Some(tags.iter().map(|tag| tag.0.clone()).collect()))
                        .with_metadata(metadata),
                )
                .await
                .context("creating package version")??;
            write_json(&output);
        }
        VersionsCmd::Delete { version } => {
            let version_id = resolve_version(executor, version).await?;
            let output = executor
                .execute(DeletePackageVersionAction::new(version_id.clone()))
                .await
                .context("deleting package version")??;
            write_json(&output);
        }
        VersionsCmd::Rename { version, name } => {
            let version_id = resolve_version(executor, version).await?;
            let output = executor
                .execute(
                    SetPackageVersionNameAction::new(version_id.clone()).with_name(name.clone()),
                )
                .await
                .context("renaming package version")??;
            write_json(&output);
        }
        VersionsCmd::SetMetadata { version, metadata } => {
            let version_id = resolve_version(executor, version).await?;
            let output = executor
                .execute(SetPackageVersionMetadataAction::new(
                    version_id.clone(),
                    json_value_to_map(metadata),
                ))
                .await
                .context("setting package version metadata")??;
            write_json(&output);
        }
        VersionsCmd::Tag { version, tags } => {
            let version_id = resolve_version(executor, version).await?;
            let output = executor
                .execute(TagPackageVersionAction::new(
                    version_id.clone(),
                    tags.iter().map(|tag| tag.0.clone()).collect(),
                ))
                .await
                .context("adding package version tags")??;
            write_json(&output);
        }
        VersionsCmd::Untag { version, tags } => {
            let version_id = resolve_version(executor, version).await?;
            let output = executor
                .execute(UntagPackageVersionAction::new(
                    version_id.clone(),
                    tags.iter().cloned().map(RemoveTagItem::new).collect(),
                ))
                .await
                .context("removing package version tags")??;
            write_json(&output);
        }
        VersionsCmd::Assets(cmd) => match cmd {
            VersionAssetsCmd::Add {
                version,
                asset_id,
                filename,
                metadata,
            } => {
                let version_id = resolve_version(executor, version).await?;
                let metadata = metadata.as_ref().map(json_value_to_map);
                let output = executor
                    .execute(
                        AddPackageVersionAssetAction::new(
                            version_id.clone(),
                            asset_id.clone(),
                            filename.to_owned(),
                        )
                        .with_metadata(metadata),
                    )
                    .await??;
                write_json(&output);
            }
            VersionAssetsCmd::Remove { version, filename } => {
                let version_id = resolve_version(executor, version).await?;
                let output = executor
                    .execute(RemovePackageVersionAssetAction::new(
                        version_id.clone(),
                        filename.clone(),
                    ))
                    .await??;
                write_json(&output);
            }
            VersionAssetsCmd::SetMetadata {
                version,
                filename,
                metadata,
            } => {
                let version_id = resolve_version(executor, version).await?;
                let output = executor
                    .execute(SetPackageVersionAssetMetadataAction::new(
                        version_id.clone(),
                        filename.clone(),
                        json_value_to_map(metadata),
                    ))
                    .await
                    .context("setting package version asset metadata")??;
                write_json(&output);
            }
        },
    }
    Ok(())
}

fn parse_repository_visibility(visibility: &str) -> Result<RepositoryVisibility, String> {
    match visibility {
        "public" => Ok(RepositoryVisibility::Public),
        "private" => Ok(RepositoryVisibility::Private),
        _ => Err("expected one of public, private".to_owned()),
    }
}

fn write_json<T: serde::Serialize>(output: &T) {
    serde_json::to_writer_pretty(std::io::stdout(), output).unwrap();
}

#[cfg(test)]
mod path_tests {
    use nexigon_ids::Generate;
    use nexigon_ids::ids::PackageId;
    use nexigon_ids::ids::PackageVersionId;
    use nexigon_ids::ids::RepositoryAssetId;
    use nexigon_ids::ids::RepositoryId;
    use proptest::prelude::*;

    use super::parse_asset_path;
    use super::parse_complete_id;
    use super::parse_package_path;
    use super::parse_version_path;

    #[test]
    fn rejects_empty_or_ambiguous_package_components() {
        for path in [
            "",
            "repository",
            "/package",
            "repository/",
            "repository//package",
            "repository/package/",
            "repository/package/extra",
        ] {
            assert!(parse_package_path(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn rejects_empty_or_ambiguous_version_components() {
        for path in [
            "",
            "repository",
            "repository/package",
            "/package/tag",
            "repository//tag",
            "repository/package/",
            "repository/package/tag/",
            "repository/package/tag/extra",
        ] {
            assert!(parse_version_path(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn rejects_empty_or_ambiguous_asset_components() {
        for path in [
            "",
            "repository",
            "repository/package",
            "repository/package/tag",
            "/package/tag/file",
            "repository//tag/file",
            "repository/package//file",
            "repository/package/tag/",
            "repository/package/tag//file",
            "repository/package/tag/file/",
            "repository/package/tag/nested//file",
        ] {
            assert!(parse_asset_path(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn id_prefixes_are_names_until_the_complete_value_is_a_valid_id() {
        assert!(parse_complete_id::<RepositoryId>("repo_not-an-id").is_none());
        assert!(parse_complete_id::<PackageId>("pkg_not-an-id/package").is_none());
        assert!(
            parse_complete_id::<RepositoryAssetId>("repo_a_not-an-id/package/tag/file").is_none()
        );
        assert!(parse_complete_id::<PackageVersionId>("pkg_v_not-an-id/package/tag").is_none());

        assert_eq!(
            parse_package_path("pkg_not-an-id/package")
                .expect("reserved-prefix repository slug must remain a path")
                .to_string(),
            "pkg_not-an-id/package"
        );
        assert_eq!(
            parse_asset_path("repo_a_not-an-id/package/tag/nested/file")
                .expect("reserved-prefix repository slug must remain a path")
                .to_string(),
            "repo_a_not-an-id/package/tag/nested/file"
        );
        assert_eq!(
            parse_version_path("pkg_v_not-an-id/package/tag")
                .expect("reserved-prefix repository slug must remain a path")
                .to_string(),
            "pkg_v_not-an-id/package/tag"
        );
    }

    #[test]
    fn complete_valid_ids_are_still_recognized() {
        assert!(parse_complete_id::<RepositoryId>(&RepositoryId::generate().to_string()).is_some());
        assert!(parse_complete_id::<PackageId>(&PackageId::generate().to_string()).is_some());
        assert!(
            parse_complete_id::<RepositoryAssetId>(&RepositoryAssetId::generate().to_string())
                .is_some()
        );
        assert!(
            parse_complete_id::<PackageVersionId>(&PackageVersionId::generate().to_string())
                .is_some()
        );
    }

    proptest! {
        #[test]
        fn package_paths_round_trip(
            repository in "[a-z][a-z0-9_.-]{0,15}",
            package in "[a-z][a-z0-9_.-]{0,15}",
        ) {
            let input = format!("{repository}/{package}");
            let parsed = parse_package_path(&input).expect("generated path is valid");
            prop_assert_eq!(parsed.to_string(), input);
        }

        #[test]
        fn version_paths_round_trip(
            repository in "[a-z][a-z0-9_.-]{0,15}",
            package in "[a-z][a-z0-9_.-]{0,15}",
            tag in "[a-z][a-z0-9_.-]{0,15}",
        ) {
            let input = format!("{repository}/{package}/{tag}");
            let parsed = parse_version_path(&input).expect("generated path is valid");
            prop_assert_eq!(parsed.to_string(), input);
        }

        #[test]
        fn nested_asset_paths_round_trip(
            repository in "[a-z][a-z0-9_.-]{0,15}",
            package in "[a-z][a-z0-9_.-]{0,15}",
            tag in "[a-z][a-z0-9_.-]{0,15}",
            filename in prop::collection::vec("[a-z][a-z0-9_.-]{0,15}", 1..5),
        ) {
            let input = format!("{repository}/{package}/{tag}/{}", filename.join("/"));
            let parsed = parse_asset_path(&input).expect("generated path is valid");
            prop_assert_eq!(parsed.to_string(), input);
        }
    }
}
