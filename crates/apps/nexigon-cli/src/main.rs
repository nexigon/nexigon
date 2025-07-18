use std::io::BufRead;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use clap::Parser;
use clap::Subcommand;

use tokio::net::TcpListener;
use tracing::error;
use tracing::info;

use nexigon_api::types::actor::GetActorAction;
use nexigon_api::types::devices::IssueDeviceHttpProxyTokenAction;
use nexigon_api::types::repositories::AddPackageVersionAssetAction;
use nexigon_api::types::repositories::AddTagItem;
use nexigon_api::types::repositories::CreateAssetAction;
use nexigon_api::types::repositories::CreatePackageAction;
use nexigon_api::types::repositories::CreatePackageVersionAction;
use nexigon_api::types::repositories::DeletePackageAction;
use nexigon_api::types::repositories::DeletePackageVersionAction;
use nexigon_api::types::repositories::GetPackageVersionDetailsAction;
use nexigon_api::types::repositories::IssueAssetUploadUrlAction;
use nexigon_api::types::repositories::RemovePackageVersionAssetAction;
use nexigon_api::types::repositories::ResolvePackageByPathAction;
use nexigon_api::types::repositories::ResolvePackageVersionAssetByPathAction;
use nexigon_api::types::repositories::ResolvePackageVersionByPathAction;
use nexigon_api::types::repositories::ResolvePackageVersionByPathOutput;
use nexigon_api::types::repositories::ResolveRepositoryNameAction;
use nexigon_api::types::repositories::ResolveRepositoryNameOutput;
use nexigon_api::types::repositories::TagPackageVersionAction;
use nexigon_api::with_actions;
use nexigon_client::ClientExecutor;
use nexigon_client::ClientToken;
use nexigon_client::connect_executor;
use nexigon_ids::ids::DeviceId;
use nexigon_ids::ids::PackageId;
use nexigon_ids::ids::PackageVersionId;
use nexigon_ids::ids::RepositoryAssetId;
use nexigon_ids::ids::RepositoryId;
use nexigon_multiplex::ConnectionRef;
use nexigon_multiplex::OpenError;

use crate::config::Config;

pub mod config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _logging_guard = si_observability::Initializer::new("NEXIGON")
        .apply(&args.logging)
        .init();
    info!("starting Nexigon CLI");

    if let Cmd::Configure { local } = &args.cmd {
        let config_path = if *local {
            let current_dir =
                std::env::current_dir().context("unable to determine current working directory")?;
            current_dir.join(".nexigon-cli.toml")
        } else {
            std::env::home_dir()
                .ok_or_else(|| anyhow!("unable to determine home directory"))?
                .join(".nexigon/cli.toml")
        };
        if let Some(parent) = config_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let config = tokio::task::spawn_blocking(|| -> anyhow::Result<Config> {
            let hub_url = dialoguer::Input::new()
                .with_prompt("Nexigon Hub URL")
                .default("https://demo.nexigon.dev".to_owned())
                .interact()?;
            let token = dialoguer::Password::new()
                .with_prompt("User Access Token")
                .interact()?;
            Ok(Config {
                hub_url,
                token: token.parse()?,
            })
        })
        .await??;
        tokio::fs::write(
            &config_path,
            &toml::to_string_pretty(&config).expect("config is valid TOML"),
        )
        .await
        .with_context(|| format!("unable to write config file: {config_path:?}"))?;
        return Ok(());
    }

    let config_path = get_config_path(&args)?;
    let config = toml::from_str::<Config>(
        &tokio::fs::read_to_string(&config_path)
            .await
            .context("cannot read config")?,
    )
    .context("cannot parse config")?;
    nexigon_client::install_crypto_provider();
    let connection = nexigon_client::ClientBuilder::new(
        config.hub_url.parse().unwrap(),
        ClientToken::UserToken(config.token.clone()),
    )
    .connect()
    .await
    .unwrap();
    let mut connection_ref = connection.make_ref();
    let join_handle = connection.spawn();
    let mut executor = connect_executor(&mut connection_ref).await.unwrap();
    let _actor = match executor
        .execute(GetActorAction::new())
        .await
        .unwrap()
        .unwrap()
        .actor
    {
        nexigon_api::types::actor::Actor::UserToken(actor) => {
            info!(user_id = %actor.user_id);
            actor
        }
        _ => {
            bail!("received unexpected actor type");
        }
    };
    match &args.cmd {
        Cmd::Configure { .. } => {
            unreachable!()
        }
        Cmd::Forward { device, forward } => {
            for forward in forward {
                tokio::spawn(forward_tcp(
                    connection_ref.clone(),
                    device.clone(),
                    forward.clone(),
                ));
            }
            join_handle.await.unwrap();
        }
        Cmd::HttpProxy(cmd) => match cmd {
            HttpProxyCmd::IssueUrl {
                device_id,
                hostname,
                port,
                valid_for,
            } => {
                let output = executor
                    .execute(
                        IssueDeviceHttpProxyTokenAction::new(device_id.clone())
                            .with_hostname(hostname.clone())
                            .with_port(*port)
                            .with_valid_for_secs(*valid_for),
                    )
                    .await
                    .context("issuing HTTP proxy URL")?;
                println!("{}", serde_json::to_string(&output).unwrap())
            }
        },
        Cmd::Actions(cmd) => match cmd {
            ActionsCmd::Execute { name, input } => {
                use nexigon_api::types::*;
                macro_rules! invoke_action {
                    ($(($name:literal, $variant:ident, $input:path, $output:path),)*) => {
                        match name.as_str() {
                            $(
                                $name => {
                                    let action = serde_json::from_str::<$input>(input).context("parsing action input")?;
                                    let output = executor.execute(action).await?;
                                    println!("{}", serde_json::to_string(&output).unwrap());
                                },
                            )*
                            _ => {
                                bail!("unknown action: {name}");
                            }
                        }
                    };
                }
                with_actions!(invoke_action)
            }
        },
        Cmd::Repositories(cmd) => match cmd {
            RepositoriesCmd::Packages(cmd) => match cmd {
                PackagesCmd::Create { repository, name } => {
                    let repository_id = resolve_repository(&mut executor, repository).await?;
                    let output = executor
                        .execute(CreatePackageAction::new(
                            repository_id.clone(),
                            name.to_owned(),
                        ))
                        .await
                        .context("creating package")?;
                    serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
                }
                PackagesCmd::Delete { package } => {
                    let package_id = resolve_package(&mut executor, package).await?;
                    let output = executor
                        .execute(DeletePackageAction::new(package_id.clone()))
                        .await
                        .context("deleting package")?;
                    serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
                }
            },
            RepositoriesCmd::Versions(cmd) => match cmd {
                PackageVersionsCmd::Info { version } => {
                    let version_id = resolve_version(&mut executor, version).await?;
                    let output = executor
                        .execute(GetPackageVersionDetailsAction::new(version_id))
                        .await
                        .context("getting package version info")?;
                    serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
                }
                PackageVersionsCmd::Resolve { version } => {
                    let path = parse_version_path(version)?;
                    let output = executor
                        .execute(ResolvePackageVersionByPathAction::new(
                            path.repository,
                            path.package,
                            path.tag,
                        ))
                        .await
                        .context("getting package version info")?;
                    serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
                }
                PackageVersionsCmd::Create { package, tags } => {
                    let package_id = resolve_package(&mut executor, package).await?;
                    let output = executor
                        .execute(
                            CreatePackageVersionAction::new(package_id.clone())
                                .with_tags(Some(tags.iter().map(|tag| tag.0.clone()).collect())),
                        )
                        .await
                        .context("creating package version")?;
                    serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
                }
                PackageVersionsCmd::Delete { version } => {
                    let version_id = resolve_version(&mut executor, version).await?;
                    let output = executor
                        .execute(DeletePackageVersionAction::new(version_id.clone()))
                        .await
                        .context("deleting package version")?;
                    serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
                }
                PackageVersionsCmd::Tag { version, tags } => {
                    let version_id = resolve_version(&mut executor, version).await?;
                    let output = executor
                        .execute(TagPackageVersionAction::new(
                            version_id.clone(),
                            tags.iter().map(|tag| tag.0.clone()).collect(),
                        ))
                        .await
                        .context("adding package version tags")?;
                    serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
                }
                PackageVersionsCmd::Assets(cmd) => match cmd {
                    VersionAssetsCommand::Add {
                        version,
                        asset_id,
                        filename,
                    } => {
                        let version_id = resolve_version(&mut executor, version).await?;
                        let output = executor
                            .execute(AddPackageVersionAssetAction::new(
                                version_id.clone(),
                                asset_id.clone(),
                                filename.to_owned(),
                            ))
                            .await
                            .unwrap();
                        serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
                    }
                    VersionAssetsCommand::Remove { version, filename } => {
                        let version_id = resolve_version(&mut executor, version).await?;
                        let output = executor
                            .execute(RemovePackageVersionAssetAction::new(
                                version_id.clone(),
                                filename.clone(),
                            ))
                            .await
                            .unwrap();
                        serde_json::to_writer_pretty(std::io::stdout(), &output).unwrap();
                    }
                },
            },
            RepositoriesCmd::Assets(cmd) => {
                match cmd {
                    AssetsCmd::Upload { repository, path } => {
                        let repository_id = resolve_repository(&mut executor, repository).await?;
                        // Size of the asset.
                        let size = tokio::fs::metadata(path)
                            .await
                            .context("getting asset size")?
                            .len();
                        // Hash of the asset.
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
                        // Try to create the asset.
                        let output = executor
                            .execute(CreateAssetAction::new(repository_id.clone(), size, digest))
                            .await
                            .unwrap()
                            .unwrap();
                        let asset_id = match &output {
                            nexigon_api::types::repositories::CreateAssetOutput::AssetAlreadyExists(asset_id) => asset_id,
                            nexigon_api::types::repositories::CreateAssetOutput::Created(asset_id) => asset_id,
                        };
                        // Issue upload URL.
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
                }
            }
        },
    }
    Ok(())
}

/// CLI arguments.
#[derive(Debug, Parser)]
pub struct Args {
    /// Logging arguments.
    #[clap(flatten)]
    logging: si_observability::clap4::LoggingArgs,
    /// Configuration file.
    #[clap(long)]
    config: Option<PathBuf>,
    //// Command.
    #[clap(subcommand)]
    cmd: Cmd,
}

/// CLI command.
#[derive(Debug, Parser)]
enum Cmd {
    /// Configure the CLI.
    Configure {
        /// Create a configuration file in the current directory.
        #[clap(long)]
        local: bool,
    },
    /// Forward command.
    Forward {
        /// Device id.
        device: DeviceId,
        /// Forward settings.
        forward: Vec<ForwardPorts>,
    },
    /// HTTP reverse proxy command.
    #[clap(subcommand)]
    HttpProxy(HttpProxyCmd),
    /// Raw actions API access.
    #[clap(subcommand)]
    Actions(ActionsCmd),
    /// Manage repositories.
    #[clap(subcommand)]
    Repositories(RepositoriesCmd),
}

/// HTTP reverse proxy command.
#[derive(Debug, Parser)]
enum HttpProxyCmd {
    /// Issue a URL.
    IssueUrl {
        /// Device to issue the URL for.
        device_id: DeviceId,
        /// Proxy domain.
        #[clap(long)]
        hostname: Option<String>,
        /// Proxy port.
        #[clap(long)]
        port: Option<u16>,
        /// Validity period.
        #[clap(long)]
        valid_for: Option<u32>,
    },
}

/// Actions command.
#[derive(Debug, Parser)]
enum ActionsCmd {
    /// Execute an action.
    Execute {
        /// Action to execute.
        name: String,
        /// Input to the action.
        input: String,
    },
}

/// Repository subcommand.
#[derive(Debug, Parser)]
enum RepositoriesCmd {
    /// Manage packages.
    #[clap(subcommand)]
    Packages(PackagesCmd),
    /// Manage package versions.
    #[clap(subcommand)]
    Versions(PackageVersionsCmd),
    /// Manage assets.
    #[clap(subcommand)]
    Assets(AssetsCmd),
}

/// Packages subcommand.
#[derive(Debug, Parser)]
pub enum PackagesCmd {
    /// Create a new package.
    Create {
        /// Repository name or ID.
        repository: String,
        /// Package name.
        name: String,
    },
    /// Delete a package.
    Delete {
        /// Package path or ID.
        package: String,
    },
}

/// Argument describing a tag to add.
#[derive(Debug, Clone)]
pub struct AddTagArg(AddTagItem);

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

#[derive(Debug, Subcommand)]
pub enum PackageVersionsCmd {
    /// Resolve a package version path.
    Resolve {
        /// Package version path.
        version: String,
    },
    /// Create a new package version.
    Create {
        /// Package path or ID.
        package: String,
        /// Tags to add.
        #[clap(long = "tag")]
        tags: Vec<AddTagArg>,
    },
    /// Delete a package version.
    Delete {
        /// Package version path or ID.
        version: String,
    },
    /// Get information about a package version.
    Info { version: String },
    /// Manage the assets of a package version.
    #[clap(subcommand)]
    Assets(VersionAssetsCommand),
    /// Add tags to a version.
    Tag {
        /// Package version path or ID.
        version: String,
        /// Tags to add.
        #[clap(long = "tag")]
        tags: Vec<AddTagArg>,
    },
}

#[derive(Debug, Subcommand)]
pub enum VersionAssetsCommand {
    /// Add an asset to a package version.
    Add {
        /// Package version path or ID.
        version: String,
        /// Asset ID.
        asset_id: RepositoryAssetId,
        /// Asset Filename.
        filename: String,
    },
    /// Remove an asset from a package version.
    Remove {
        /// Package version path or ID.
        version: String,
        /// Asset filename.
        filename: String,
    },
}

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

/// Forward ports.
#[derive(Debug, Clone)]
pub struct ForwardPorts {
    /// Local port.
    local: u16,
    /// Remote port.
    remote: u16,
}

impl std::str::FromStr for ForwardPorts {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split(':');
        let local = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing local port"))?
            .parse()?;
        let remote = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing remote port"))?
            .parse()?;
        Ok(Self { local, remote })
    }
}

/// Get the configuration path.
pub fn get_config_path(args: &Args) -> anyhow::Result<PathBuf> {
    if let Some(config_path) = &args.config {
        return Ok(config_path.clone());
    }
    let current_dir =
        std::env::current_dir().context("unable to determine current working directory")?;
    let local_config = current_dir.join(".nexigon-cli.toml");
    if local_config.exists() {
        return Ok(local_config);
    }
    if let Some(home_dir) = std::env::home_dir() {
        let home_config = home_dir.join(".nexigon/cli.toml");
        if home_config.exists() {
            return Ok(home_config);
        }
    }
    bail!("unable to find configuration file")
}

/// Forward a local TCP port to a remote device.
pub async fn forward_tcp(connection: ConnectionRef, device: DeviceId, forward: ForwardPorts) {
    let endpoint = format!("device/{}/proxy/forward/tcp/{}", device, forward.remote);
    info!("forward port {} to endpoint {endpoint}", forward.local);
    let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), forward.local))
        .await
        .unwrap();
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut connection = connection.clone();
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            let open_future = connection.open(endpoint.as_bytes());
            let mut channel = match open_future.await {
                Ok(channel) => channel,
                Err(error) => {
                    error!("error opening channel: {error}");
                    if let OpenError::Rejected(rejection) = &error {
                        let reason = std::str::from_utf8(rejection.reason()).unwrap();
                        println!("reason: {reason}");
                    }
                    return;
                }
            };
            tokio::io::copy_bidirectional(&mut socket, &mut channel)
                .await
                .unwrap();
        });
    }
}

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

pub struct VersionPath {
    repository: String,
    package: String,
    tag: String,
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
        bail!("too many parts in package name");
    }
    Ok(VersionPath {
        repository,
        package,
        tag,
    })
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

pub struct AssetPath {
    repository: String,
    package: String,
    tag: String,
    filename: String,
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
    let filename = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing filename"))?
        .to_owned();
    if parts_iter.next().is_some() {
        bail!("too many parts in package name");
    }
    Ok(AssetPath {
        repository,
        package,
        tag,
        filename,
    })
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
