use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use clap::Parser;

use nexigon_api::types::devices;
use nexigon_api::types::projects;
use tokio::net::TcpListener;
use tracing::error;
use tracing::info;

use nexigon_api::types::actor::GetActorAction;
use nexigon_api::with_actions;
use nexigon_client::ClientToken;
use nexigon_client::connect_executor;
use nexigon_common::execute_repositories_cmd;
use nexigon_ids::ids::DeploymentTokenId;
use nexigon_ids::ids::DeviceId;
use nexigon_ids::ids::OrganizationId;
use nexigon_ids::ids::ProjectId;
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
        let config = tokio::task::spawn_blocking(|| -> anyhow::Result<Config> {
            let hub_url = dialoguer::Input::new()
                .with_prompt("Nexigon Hub URL")
                .default("https://eu.nexigon.cloud".to_owned())
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
        nexigon_common::secure_file::write_private(
            &config_path,
            &toml::to_string_pretty(&config).expect("config is valid TOML"),
            !*local,
        )
        .await
        .with_context(|| format!("unable to write config file: {config_path:?}"))?;
        return Ok(());
    }

    let config_path = get_config_path(&args)?;
    let config_contents = nexigon_common::secure_file::read_private(&config_path)
        .await
        .with_context(|| {
            format!("configuration must be a private regular file: {config_path:?}")
        })?;
    let config_contents =
        String::from_utf8(config_contents).context("config is not valid UTF-8")?;
    let config = toml::from_str::<Config>(&config_contents).context("cannot parse config")?;
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
                        devices::IssueDeviceHttpProxyTokenAction::new(device_id.clone())
                            .with_hostname(hostname.clone())
                            .with_port(*port)
                            .with_valid_for_secs(*valid_for),
                    )
                    .await
                    .context("issuing HTTP proxy URL")??;
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
        Cmd::Repositories(cmd) => {
            execute_repositories_cmd(cmd, &mut executor).await?;
        }
        Cmd::Projects(cmd) => match cmd {
            ProjectsCmd::List => {
                let output = executor
                    .execute(projects::QueryProjectsAction::new())
                    .await
                    .context("querying projects")??;
                write_json(&output);
            }
            ProjectsCmd::Info { project } => {
                let output = executor
                    .execute(projects::GetProjectDetailsAction::new(project.clone()))
                    .await
                    .context("getting project details")??;
                write_json(&output);
            }
            ProjectsCmd::Create { organization, name } => {
                let output = executor
                    .execute(projects::CreateProjectAction::new(
                        organization.clone(),
                        name.clone(),
                    ))
                    .await
                    .context("creating project")??;
                write_json(&output);
            }
            ProjectsCmd::Delete { project } => {
                let output = executor
                    .execute(projects::DeleteProjectAction::new(project.clone()))
                    .await
                    .context("deleting project")??;
                write_json(&output);
            }
            ProjectsCmd::Rename { project, name } => {
                let output = executor
                    .execute(projects::SetProjectNameAction::new(
                        project.clone(),
                        name.clone(),
                    ))
                    .await
                    .context("renaming project")??;
                write_json(&output);
            }
            ProjectsCmd::Devices(cmd) => match cmd {
                ProjectDevicesCmd::List { project } => {
                    let output = executor
                        .execute(projects::QueryProjectDevicesAction::new(project.clone()))
                        .await
                        .context("querying project devices")??;
                    write_json(&output);
                }
            },
            ProjectsCmd::Repositories(cmd) => match cmd {
                ProjectRepositoriesCmd::List { project } => {
                    let output = executor
                        .execute(projects::QueryProjectRepositoriesAction::new(
                            project.clone(),
                        ))
                        .await
                        .context("querying project repositories")??;
                    write_json(&output);
                }
                ProjectRepositoriesCmd::Link {
                    project,
                    repository,
                } => {
                    let output = executor
                        .execute(projects::AddProjectRepositoryAction::new(
                            project.clone(),
                            repository.clone(),
                        ))
                        .await
                        .context("linking project repository")??;
                    write_json(&output);
                }
                ProjectRepositoriesCmd::Unlink {
                    project,
                    repository,
                } => {
                    let output = executor
                        .execute(projects::RemoveProjectRepositoryAction::new(
                            project.clone(),
                            repository.clone(),
                        ))
                        .await
                        .context("unlinking project repository")??;
                    write_json(&output);
                }
            },
            ProjectsCmd::Tokens(cmd) => match cmd {
                ProjectTokensCmd::List { project } => {
                    let output = executor
                        .execute(projects::QueryProjectDeploymentTokensAction::new(
                            project.clone(),
                        ))
                        .await
                        .context("querying deployment tokens")??;
                    write_json(&output);
                }
                ProjectTokensCmd::Create {
                    project,
                    name,
                    auto_accept,
                } => {
                    let output = executor
                        .execute(
                            projects::CreateDeploymentTokenAction::new(
                                project.clone(),
                                name.clone(),
                            )
                            .with_flags(deployment_token_flags(*auto_accept)),
                        )
                        .await
                        .context("creating deployment token")??;
                    write_json(&output);
                }
                ProjectTokensCmd::Delete { token } => {
                    let output = executor
                        .execute(projects::DeleteDeploymentTokenAction::new(token.clone()))
                        .await
                        .context("deleting deployment token")??;
                    write_json(&output);
                }
                ProjectTokensCmd::SetFlags { token, auto_accept } => {
                    let flags =
                        projects::DeploymentTokenFlags::new().with_auto_accept(Some(*auto_accept));
                    let output = executor
                        .execute(projects::SetDeploymentTokenFlagsAction::new(
                            token.clone(),
                            flags,
                        ))
                        .await
                        .context("setting deployment token flags")??;
                    write_json(&output);
                }
            },
            ProjectsCmd::Otlp(cmd) => match cmd {
                ProjectOtlpCmd::Get { project } => {
                    let output = executor
                        .execute(projects::GetProjectOtlpConfigAction::new(project.clone()))
                        .await
                        .context("getting project OTLP config")??;
                    write_json(&output);
                }
                ProjectOtlpCmd::Set { project, config } => {
                    let config = serde_json::from_str::<projects::ProjectOtlpConfig>(config)
                        .context("project OTLP config must be valid JSON")?;
                    let output = executor
                        .execute(projects::SetProjectOtlpConfigAction::new(
                            project.clone(),
                            config,
                        ))
                        .await
                        .context("setting project OTLP config")??;
                    write_json(&output);
                }
            },
        },
        Cmd::Devices(cmd) => match cmd {
            DevicesCmd::Info { device } => {
                let output = executor
                    .execute(devices::GetDeviceDetailsAction::new(device.clone()))
                    .await
                    .context("getting device details")??;
                write_json(&output);
            }
            DevicesCmd::Create {
                project,
                fingerprint,
            } => {
                let output = executor
                    .execute(devices::CreateDeviceAction::new(
                        project.clone(),
                        fingerprint.clone(),
                    ))
                    .await
                    .context("creating device")??;
                write_json(&output);
            }
            DevicesCmd::Delete { device } => {
                let output = executor
                    .execute(devices::DeleteDeviceAction::new(device.clone()))
                    .await
                    .context("deleting device")??;
                write_json(&output);
            }
            DevicesCmd::Rename { device, name } => {
                let output = executor
                    .execute(
                        devices::SetDeviceNameAction::new(device.clone()).with_name(name.clone()),
                    )
                    .await
                    .context("renaming device")??;
                write_json(&output);
            }
            DevicesCmd::Properties(cmd) => match cmd {
                DevicePropertiesCmd::List { device } => {
                    let output = executor
                        .execute(devices::QueryDevicePropertiesAction::new(device.clone()))
                        .await
                        .context("querying device properties")??;
                    write_json(&output);
                }
                DevicePropertiesCmd::Set {
                    device,
                    name,
                    value,
                    protected,
                } => {
                    let output = executor
                        .execute(
                            devices::SetDevicePropertyAction::new(
                                device.clone(),
                                name.clone(),
                                serde_json::from_str(value)
                                    .context("device property value must be valid JSON")?,
                            )
                            .with_protected(*protected),
                        )
                        .await
                        .context("unable to set device property")??;
                    write_json(&output);
                }
                DevicePropertiesCmd::Get { device, name } => {
                    let output = executor
                        .execute(devices::GetDevicePropertyAction::new(
                            device.clone(),
                            name.clone(),
                        ))
                        .await
                        .context("unable to get device property")??;
                    write_json(&output);
                }
                DevicePropertiesCmd::Remove { device, name } => {
                    let output = executor
                        .execute(devices::RemoveDevicePropertyAction::new(
                            device.clone(),
                            name.clone(),
                        ))
                        .await
                        .context("unable to remove device property")??;
                    write_json(&output);
                }
            },
            DevicesCmd::Certificates(cmd) => match cmd {
                DeviceCertificatesCmd::Add {
                    device,
                    fingerprint,
                    status,
                } => {
                    let output = executor
                        .execute(
                            devices::AddDeviceCertificateAction::new(
                                device.clone(),
                                fingerprint.clone(),
                            )
                            .with_status(status.clone()),
                        )
                        .await
                        .context("adding device certificate")??;
                    write_json(&output);
                }
                DeviceCertificatesCmd::Delete { certificate } => {
                    let output = executor
                        .execute(devices::DeleteDeviceCertificateAction::new(
                            certificate.clone(),
                        ))
                        .await
                        .context("deleting device certificate")??;
                    write_json(&output);
                }
                DeviceCertificatesCmd::SetStatus {
                    certificate,
                    status,
                } => {
                    let output = executor
                        .execute(devices::SetDeviceCertificateStatusAction::new(
                            certificate.clone(),
                            status.clone(),
                        ))
                        .await
                        .context("setting device certificate status")??;
                    write_json(&output);
                }
            },
            DevicesCmd::Connections(cmd) => match cmd {
                DeviceConnectionsCmd::List {
                    device,
                    limit,
                    active_only,
                } => {
                    let output = executor
                        .execute(
                            devices::QueryDeviceConnectionsAction::new(device.clone())
                                .with_limit(*limit)
                                .with_active_only(*active_only),
                        )
                        .await
                        .context("querying device connections")??;
                    write_json(&output);
                }
            },
            DevicesCmd::Events(cmd) => match cmd {
                DeviceEventsCmd::List { device, limit } => {
                    let output = executor
                        .execute(
                            devices::QueryDeviceEventsAction::new(device.clone())
                                .with_limit(*limit),
                        )
                        .await
                        .context("querying device events")??;
                    write_json(&output);
                }
            },
            DevicesCmd::Commands(cmd) => match cmd {
                DeviceCommandsCmd::List { device } => {
                    let output = executor
                        .execute(devices::QueryDeviceCommandsAction::new(device.clone()))
                        .await
                        .context("querying device commands")??;
                    write_json(&output);
                }
            },
        },
    }
    Ok(())
}

/// CLI arguments.
#[derive(Debug, Parser)]
#[clap(version = nexigon_version::NEXIGON_GIT_VERSION)]
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
    Repositories(nexigon_common::RepositoriesCmd),
    /// Manage projects.
    #[clap(subcommand)]
    Projects(ProjectsCmd),
    /// Manage devices.
    #[clap(subcommand)]
    Devices(DevicesCmd),
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

/// Projects subcommand.
#[derive(Debug, Parser)]
pub enum ProjectsCmd {
    /// List projects.
    List,
    /// Get project details.
    Info {
        /// Project ID.
        project: ProjectId,
    },
    /// Create a project.
    Create {
        /// Organization ID.
        organization: OrganizationId,
        /// Project name.
        name: String,
    },
    /// Delete a project.
    Delete {
        /// Project ID.
        project: ProjectId,
    },
    /// Rename a project.
    Rename {
        /// Project ID.
        project: ProjectId,
        /// New project name.
        name: String,
    },
    /// Manage project devices.
    #[clap(subcommand)]
    Devices(ProjectDevicesCmd),
    /// Manage project repositories.
    #[clap(subcommand)]
    Repositories(ProjectRepositoriesCmd),
    /// Manage project deployment tokens.
    #[clap(subcommand)]
    Tokens(ProjectTokensCmd),
    /// Manage project OTLP config.
    #[clap(subcommand)]
    Otlp(ProjectOtlpCmd),
}

/// Project devices subcommand.
#[derive(Debug, Parser)]
pub enum ProjectDevicesCmd {
    /// List devices of a project.
    List {
        /// Project ID.
        project: ProjectId,
    },
}

/// Project repositories subcommand.
#[derive(Debug, Parser)]
pub enum ProjectRepositoriesCmd {
    /// List repositories linked to a project.
    List {
        /// Project ID.
        project: ProjectId,
    },
    /// Link a repository to a project.
    Link {
        /// Project ID.
        project: ProjectId,
        /// Repository ID.
        repository: RepositoryId,
    },
    /// Unlink a repository from a project.
    Unlink {
        /// Project ID.
        project: ProjectId,
        /// Repository ID.
        repository: RepositoryId,
    },
}

/// Project deployment tokens subcommand.
#[derive(Debug, Parser)]
pub enum ProjectTokensCmd {
    /// List deployment tokens of a project.
    List {
        /// Project ID.
        project: ProjectId,
    },
    /// Create a deployment token.
    Create {
        /// Project ID.
        project: ProjectId,
        /// Token name.
        name: String,
        /// Automatically accept new devices.
        #[clap(long)]
        auto_accept: Option<bool>,
    },
    /// Delete a deployment token.
    Delete {
        /// Deployment token ID.
        token: DeploymentTokenId,
    },
    /// Set deployment token flags.
    SetFlags {
        /// Deployment token ID.
        token: DeploymentTokenId,
        /// Automatically accept new devices.
        #[clap(long, action = clap::ArgAction::Set)]
        auto_accept: bool,
    },
}

/// Project OTLP subcommand.
#[derive(Debug, Parser)]
pub enum ProjectOtlpCmd {
    /// Get OTLP config.
    Get {
        /// Project ID.
        project: ProjectId,
    },
    /// Set OTLP config from a JSON object.
    Set {
        /// Project ID.
        project: ProjectId,
        /// OTLP config JSON.
        config: String,
    },
}

/// Devices subcommand.
#[derive(Debug, Parser)]
pub enum DevicesCmd {
    /// Get device details.
    Info {
        /// Device ID.
        device: DeviceId,
    },
    /// Create a device.
    Create {
        /// Project ID.
        project: ProjectId,
        /// Device fingerprint.
        fingerprint: devices::DeviceFingerprint,
    },
    /// Delete a device.
    Delete {
        /// Device ID.
        device: DeviceId,
    },
    /// Rename a device.
    Rename {
        /// Device ID.
        device: DeviceId,
        /// New device name.
        name: Option<String>,
    },
    /// Properties subcommand.
    #[clap(subcommand)]
    Properties(DevicePropertiesCmd),
    /// Manage device certificates.
    #[clap(subcommand)]
    Certificates(DeviceCertificatesCmd),
    /// Manage device connections.
    #[clap(subcommand)]
    Connections(DeviceConnectionsCmd),
    /// Manage device events.
    #[clap(subcommand)]
    Events(DeviceEventsCmd),
    /// Manage on-demand device commands.
    #[clap(subcommand)]
    Commands(DeviceCommandsCmd),
}

/// Device properties subcommand.
#[derive(Debug, Parser)]
pub enum DevicePropertiesCmd {
    /// List device properties.
    List {
        /// Device ID.
        device: DeviceId,
    },
    /// Set a device property.
    Set {
        /// Device ID.
        device: DeviceId,
        /// Name of the property.
        name: String,
        /// Value of the property.
        value: String,
        /// Indicates whether the property should be protected.
        #[clap(long)]
        protected: Option<bool>,
    },
    /// Get a device property.
    Get {
        /// Device ID.
        device: DeviceId,
        /// Name of the property.
        name: String,
    },
    /// Remove a device property.
    Remove {
        /// Device ID.
        device: DeviceId,
        /// Name of the property.
        name: String,
    },
}

/// Device certificates subcommand.
#[derive(Debug, Parser)]
pub enum DeviceCertificatesCmd {
    /// Add a device certificate.
    Add {
        /// Device ID.
        device: DeviceId,
        /// Certificate fingerprint.
        fingerprint: devices::CertificateFingerprint,
        /// Initial certificate status.
        #[clap(long, value_parser = parse_device_certificate_status)]
        status: Option<devices::DeviceCertificateStatus>,
    },
    /// Delete a device certificate.
    Delete {
        /// Device certificate ID.
        certificate: devices::DeviceCertificateId,
    },
    /// Set a device certificate status.
    SetStatus {
        /// Device certificate ID.
        certificate: devices::DeviceCertificateId,
        /// New certificate status.
        #[clap(value_parser = parse_device_certificate_status)]
        status: devices::DeviceCertificateStatus,
    },
}

/// Device connections subcommand.
#[derive(Debug, Parser)]
pub enum DeviceConnectionsCmd {
    /// List device connections.
    List {
        /// Device ID.
        device: DeviceId,
        /// Limit the number of returned connections.
        #[clap(long)]
        limit: Option<u32>,
        /// Include only active connections.
        #[clap(long)]
        active_only: Option<bool>,
    },
}

/// Device events subcommand.
#[derive(Debug, Parser)]
pub enum DeviceEventsCmd {
    /// List device events.
    List {
        /// Device ID.
        device: DeviceId,
        /// Limit the number of returned events.
        #[clap(long)]
        limit: Option<u32>,
    },
}

/// Device commands subcommand.
#[derive(Debug, Parser)]
pub enum DeviceCommandsCmd {
    /// List on-demand commands for a device.
    List {
        /// Device ID.
        device: DeviceId,
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

fn deployment_token_flags(auto_accept: Option<bool>) -> Option<projects::DeploymentTokenFlags> {
    auto_accept.map(|auto_accept| {
        projects::DeploymentTokenFlags::new().with_auto_accept(Some(auto_accept))
    })
}

fn parse_device_certificate_status(
    status: &str,
) -> Result<devices::DeviceCertificateStatus, String> {
    match status {
        "pending" => Ok(devices::DeviceCertificateStatus::Pending),
        "active" => Ok(devices::DeviceCertificateStatus::Active),
        "rejected" => Ok(devices::DeviceCertificateStatus::Rejected),
        "revoked" => Ok(devices::DeviceCertificateStatus::Revoked),
        "conflict" => Ok(devices::DeviceCertificateStatus::Conflict),
        _ => Err("expected one of pending, active, rejected, revoked, conflict".to_owned()),
    }
}

fn write_json<T: serde::Serialize>(output: &T) {
    serde_json::to_writer_pretty(std::io::stdout(), output).unwrap();
}
