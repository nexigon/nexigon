use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use clap::Parser;

use nexigon_api::types::devices::GetDevicePropertyAction;
use nexigon_api::types::devices::RemoveDevicePropertyAction;
use nexigon_api::types::devices::SetDevicePropertyAction;
use tokio::net::TcpListener;
use tracing::error;
use tracing::info;

use nexigon_api::types::actor::GetActorAction;
use nexigon_api::types::devices::IssueDeviceHttpProxyTokenAction;
use nexigon_api::with_actions;
use nexigon_client::ClientToken;
use nexigon_client::connect_executor;
use nexigon_common::execute_repositories_cmd;
use nexigon_ids::ids::DeviceId;
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
        Cmd::Devices(cmd) => match cmd {
            DevicesCmd::Properties(cmd) => match cmd {
                DevicePropertiesCmd::Set {
                    device,
                    name,
                    value,
                    protected,
                } => {
                    executor
                        .execute(
                            SetDevicePropertyAction::new(
                                device.clone(),
                                name.clone(),
                                serde_json::from_str(value)
                                    .context("device property value must be valid JSON")?,
                            )
                            .with_protected(*protected),
                        )
                        .await
                        .context("unable to set device property")??;
                }
                DevicePropertiesCmd::Get { device, name } => {
                    let output = executor
                        .execute(GetDevicePropertyAction::new(device.clone(), name.clone()))
                        .await
                        .context("unable to get device property")??;
                    serde_json::to_writer(std::io::stdout(), &output).unwrap();
                }
                DevicePropertiesCmd::Remove { device, name } => {
                    let output = executor
                        .execute(RemoveDevicePropertyAction::new(
                            device.clone(),
                            name.clone(),
                        ))
                        .await
                        .context("unable to remove device property")??;
                    serde_json::to_writer(std::io::stdout(), &output).unwrap();
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

/// Devices subcommand.
#[derive(Debug, Parser)]
pub enum DevicesCmd {
    /// Properties subcommand.
    #[clap(subcommand)]
    Properties(DevicePropertiesCmd),
}

/// Device properties subcommand.
#[derive(Debug, Parser)]
pub enum DevicePropertiesCmd {
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
    Get {
        /// Device ID.
        device: DeviceId,
        /// Name of the property.
        name: String,
    },
    Remove {
        /// Device ID.
        device: DeviceId,
        /// Name of the property.
        name: String,
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
