use std::io::BufRead;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use clap::Subcommand;

use nexigon_api::types::repositories::AddPackageVersionAssetAction;
use nexigon_api::types::repositories::AddTagItem;
use nexigon_api::types::repositories::CreateAssetAction;
use nexigon_api::types::repositories::CreatePackageAction;
use nexigon_api::types::repositories::CreatePackageVersionAction;
use nexigon_api::types::repositories::DeletePackageAction;
use nexigon_api::types::repositories::DeletePackageVersionAction;
use nexigon_api::types::repositories::GetPackageVersionDetailsAction;
use nexigon_api::types::repositories::GetPackageVersionDetailsOutput;
use nexigon_api::types::repositories::IssueAssetDownloadUrlAction;
use nexigon_api::types::repositories::IssueAssetUploadUrlAction;
use nexigon_api::types::repositories::RemovePackageVersionAssetAction;
use nexigon_api::types::repositories::RepositoryAssetId;
use nexigon_api::types::repositories::ResolvePackageByPathAction;
use nexigon_api::types::repositories::ResolvePackageVersionAssetByPathAction;
use nexigon_api::types::repositories::ResolvePackageVersionByPathAction;
use nexigon_api::types::repositories::ResolvePackageVersionByPathOutput;
use nexigon_api::types::repositories::ResolveRepositoryNameAction;
use nexigon_api::types::repositories::ResolveRepositoryNameOutput;
use nexigon_api::types::repositories::TagPackageVersionAction;
use nexigon_client::ClientExecutor;
use nexigon_ids::ids::PackageId;
use nexigon_ids::ids::PackageVersionId;
use nexigon_ids::ids::RepositoryId;

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

pub struct AssetPath {
    pub repository: String,
    pub package: String,
    pub tag: String,
    pub filename: String,
}

pub fn parse_asset_path(path: &str) -> anyhow::Result<AssetPath> {
    let mut parts_iter = path.split('/');
    let repository = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing repository"))?
        .to_owned();
    let package = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing package"))?
        .to_owned();
    let tag = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing version tag"))?
        .to_owned();
    let filename = parts_iter.collect::<Vec<_>>().join("/");
    Ok(AssetPath {
        repository,
        package,
        tag,
        filename,
    })
}

pub struct VersionPath {
    pub repository: String,
    pub package: String,
    pub tag: String,
}

pub fn parse_version_path(path: &str) -> anyhow::Result<VersionPath> {
    let mut parts_iter = path.split('/');
    let repository = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing repository"))?
        .to_owned();
    let package = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing package"))?
        .to_owned();
    let tag = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing version tag"))?
        .to_owned();
    if parts_iter.next().is_some() {
        bail!("too many parts in version path");
    }
    Ok(VersionPath {
        repository,
        package,
        tag,
    })
}

// ── Resolution helpers ───────────────────────────────────────────────

pub async fn resolve_repository(
    executor: &mut ClientExecutor,
    repository: &str,
) -> anyhow::Result<RepositoryId> {
    if repository.starts_with("repo_") {
        return Ok(repository.parse()?);
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
    executor: &mut ClientExecutor,
    package: &str,
) -> anyhow::Result<PackageId> {
    if package.starts_with("pkg_") {
        return Ok(package.parse()?);
    }
    let mut parts_iter = package.split('/');
    let repository = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing repository"))?;
    let package = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing package"))?;
    if parts_iter.next().is_some() {
        bail!("too many parts in package name");
    }
    let output = executor
        .execute(ResolvePackageByPathAction::new(
            repository.to_owned(),
            package.to_owned(),
        ))
        .await??;
    match output {
        nexigon_api::types::repositories::ResolvePackageByPathOutput::Found(output) => {
            Ok(output.package_id)
        }
        nexigon_api::types::repositories::ResolvePackageByPathOutput::NotFound => {
            bail!("package {package} not found in repository {repository}")
        }
    }
}

pub async fn resolve_asset(
    executor: &mut ClientExecutor,
    asset: &str,
) -> anyhow::Result<RepositoryAssetId> {
    if asset.starts_with("repo_a_") {
        return Ok(asset.parse()?);
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
    executor: &mut ClientExecutor,
    version: &str,
) -> anyhow::Result<PackageVersionId> {
    if version.starts_with("pkg_v") {
        return Ok(version.parse()?);
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
    executor: &mut ClientExecutor,
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
    /// Request a pre-signed URL for downloading an asset.
    IssueUrl {
        /// Asset ID or path (repository/package/tag/filename).
        asset: String,
    },
    /// Manage packages.
    #[clap(subcommand)]
    Packages(PackagesCmd),
    /// Manage package versions.
    #[clap(subcommand)]
    Versions(VersionsCmd),
    /// Manage assets.
    #[clap(subcommand)]
    Assets(AssetsCmd),
}

/// Packages subcommand.
#[derive(Debug, Subcommand)]
pub enum PackagesCmd {
    /// Create a new package.
    Create {
        /// Repository name or ID.
        repository: String,
        /// Package name.
        name: String,
        /// Optional JSON metadata.
        #[clap(long, value_parser = parse_json_object)]
        metadata: Option<serde_json::Value>,
    },
    /// Delete a package.
    Delete {
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
    /// Add tags to a version.
    Tag {
        /// Package version path or ID.
        version: String,
        /// Tags to add.
        #[clap(long = "tag")]
        tags: Vec<AddTagArg>,
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
}

/// Assets subcommand.
#[derive(Debug, Subcommand)]
pub enum AssetsCmd {
    /// Upload an asset to the repository.
    Upload {
        /// Repository name or ID.
        repository: String,
        /// Path to the asset.
        path: PathBuf,
    },
}

/// Execute a [`RepositoriesCmd`].
pub async fn execute_repositories_cmd(
    cmd: &RepositoriesCmd,
    executor: &mut ClientExecutor,
) -> anyhow::Result<()> {
    match cmd {
        RepositoriesCmd::IssueUrl { asset } => {
            let asset_id = resolve_asset(executor, asset).await?;
            let output = executor
                .execute(IssueAssetDownloadUrlAction::new(asset_id))
                .await
                .context("unable to issue asset download URL")??;
            println!("{}", serde_json::to_string(&output).unwrap());
        }
        RepositoriesCmd::Packages(cmd) => match cmd {
            PackagesCmd::Create {
                repository,
                name,
                metadata,
            } => {
                let repository_id = resolve_repository(executor, repository).await?;
                let metadata = metadata.as_ref().map(json_value_to_map);
                let output = executor
                    .execute(
                        CreatePackageAction::new(repository_id.clone(), name.to_owned())
                            .with_metadata(metadata),
                    )
                    .await
                    .context("creating package")??;
                serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
            }
            PackagesCmd::Delete { package } => {
                let package_id = resolve_package(executor, package).await?;
                let output = executor
                    .execute(DeletePackageAction::new(package_id.clone()))
                    .await
                    .context("deleting package")??;
                serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
            }
        },
        RepositoriesCmd::Versions(cmd) => execute_versions_cmd(cmd, executor).await?,
        RepositoriesCmd::Assets(cmd) => match cmd {
            AssetsCmd::Upload { repository, path } => {
                let repository_id = resolve_repository(executor, repository).await?;
                let size = tokio::fs::metadata(path)
                    .await
                    .context("getting asset size")?
                    .len();
                let digest = tokio::task::spawn_blocking({
                    let path = path.to_owned();
                    move || -> Result<si_crypto_hashes::HashDigest, std::io::Error> {
                        let mut hasher = si_crypto_hashes::HashAlgorithm::Sha256.hasher();
                        let mut file = std::io::BufReader::new(std::fs::File::open(&path)?);
                        loop {
                            let buffer = file.fill_buf()?;
                            if buffer.is_empty() {
                                break;
                            }
                            hasher.update(buffer);
                            let consumed = buffer.len();
                            file.consume(consumed);
                        }
                        Ok(hasher.finalize())
                    }
                })
                .await
                .unwrap()
                .unwrap();
                let output = executor
                    .execute(CreateAssetAction::new(repository_id.clone(), size, digest))
                    .await??;
                let asset_id = match &output {
                    nexigon_api::types::repositories::CreateAssetOutput::AssetAlreadyExists(
                        asset_id,
                    ) => asset_id,
                    nexigon_api::types::repositories::CreateAssetOutput::Created(asset_id) => {
                        asset_id
                    }
                };
                let upload_url = executor
                    .execute(IssueAssetUploadUrlAction::new(asset_id.clone()))
                    .await
                    .context("issuing upload URL")?
                    .context("issuing upload URL")?
                    .url;
                reqwest::Client::new()
                    .put(upload_url)
                    .header("Content-Length", size)
                    .body(tokio::fs::read(path).await?)
                    .send()
                    .await
                    .context("uploading asset")?
                    .error_for_status()?;
                serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
            }
        },
    }
    Ok(())
}

/// Execute a [`VersionsCmd`].
pub async fn execute_versions_cmd(
    cmd: &VersionsCmd,
    executor: &mut ClientExecutor,
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
            serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
        }
        VersionsCmd::Info { version } => {
            let version_id = resolve_version(executor, version).await?;
            let output = get_version_details(executor, version_id).await?;
            serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
        }
        VersionsCmd::Create {
            package,
            tags,
            metadata,
        } => {
            let package_id = resolve_package(executor, package).await?;
            let metadata = metadata.as_ref().map(json_value_to_map);
            let output = executor
                .execute(
                    CreatePackageVersionAction::new(package_id.clone())
                        .with_tags(Some(tags.iter().map(|tag| tag.0.clone()).collect()))
                        .with_metadata(metadata),
                )
                .await
                .context("creating package version")??;
            serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
        }
        VersionsCmd::Delete { version } => {
            let version_id = resolve_version(executor, version).await?;
            let output = executor
                .execute(DeletePackageVersionAction::new(version_id.clone()))
                .await
                .context("deleting package version")??;
            serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
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
            serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
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
                serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
            }
            VersionAssetsCmd::Remove { version, filename } => {
                let version_id = resolve_version(executor, version).await?;
                let output = executor
                    .execute(RemovePackageVersionAssetAction::new(
                        version_id.clone(),
                        filename.clone(),
                    ))
                    .await??;
                serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
            }
        },
    }
    Ok(())
}
