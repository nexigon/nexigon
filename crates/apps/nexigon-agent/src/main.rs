use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use futures::StreamExt;
use tokio::net::TcpStream;
use tracing::info;

use nexigon_api::types::actor::GetActorAction;
use nexigon_api::types::datetime::Timestamp;
use nexigon_api::types::devices::DeviceEvent;
use nexigon_api::types::devices::DeviceEventSeverity;
use nexigon_api::types::devices::IssueDeviceTokenAction;
use nexigon_api::types::devices::PublishDeviceEventsAction;
use nexigon_api::types::devices::SetDeviceMetadataAction;
use nexigon_client::ClientIdentity;
use nexigon_client::ClientToken;
use nexigon_client::connect_executor;
use nexigon_ids::Generate;
use nexigon_ids::ids::DeviceEventId;
use nexigon_ids::ids::DeviceFingerprint;
use nexigon_multiplex::ConnectionEvent;

use crate::config::Config;

pub mod config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _logging_guard = si_observability::Initializer::new("NEXIGON")
        .apply(&args.logging)
        .init();
    info!("starting Nexigon Agent");
    let config_path = args
        .config
        .canonicalize()
        .context("cannot canonicalize config path")?;
    let Some(config_dir) = config_path.parent() else {
        bail!("config path has no parent");
    };
    let config = toml::from_str::<Config>(
        &tokio::fs::read_to_string(&args.config)
            .await
            .context("cannot read config")?,
    )
    .context("cannot parse config")?;
    nexigon_client::install_crypto_provider();
    let cert = tokio::fs::read_to_string(config_dir.join(config.ssl_cert.unwrap()))
        .await
        .context("cannot read certificate")?;
    let key = tokio::fs::read_to_string(config_dir.join(config.ssl_key.unwrap()))
        .await
        .context("cannot read private key")?;
    let identity = ClientIdentity::from_pem(&cert, &key).context("cannot parse identity")?;
    let mut connection = nexigon_client::ClientBuilder::new(
        config.hub_url.parse().context("cannot parse hub URL")?,
        ClientToken::DeploymentToken(config.token.clone()),
    )
    .with_identity(Some(identity))
    .with_device_fingerprint(Some(DeviceFingerprint::from_data(b"xyz")))
    .with_register_connection(matches!(args.cmd, Cmd::Run))
    .dangerous_with_disable_tls(config.dangerous_disable_tls.unwrap_or(false))
    .connect()
    .await
    .context("cannot connect to Nexigon Hub")?;
    let mut connection_ref = connection.make_ref();
    let connection_handle = tokio::spawn(async move {
        while let Some(event) = connection.next().await {
            match event {
                Ok(ConnectionEvent::RequestChannel(request)) => {
                    info!("channel request: {request:?}");
                    let endpoint = std::str::from_utf8(request.endpoint())
                        .context("invalid UTF-8 in endpoint")?;
                    // TODO: Handle other endpoints and errors.
                    let port: u16 = endpoint
                        .strip_prefix("forward/tcp/")
                        .context("invalid endpoint")?
                        .parse()
                        .context("invalid port")?;
                    request.accept(move |mut channel| {
                        tokio::spawn(async move {
                            let mut tcp = TcpStream::connect(SocketAddr::new(
                                Ipv4Addr::LOCALHOST.into(),
                                port,
                            ))
                            .await
                            .unwrap();
                            tokio::io::copy_bidirectional(&mut channel, &mut tcp)
                                .await
                                .unwrap();
                        });
                    });
                }
                Ok(ConnectionEvent::Connected) => { /* ignore */ }
                Ok(ConnectionEvent::Closed) => {
                    info!("connection closed");
                    break;
                }
                Err(error) => {
                    bail!("connection error: {error}");
                }
            }
        }
        anyhow::Result::Ok(())
    });
    let mut executor = connect_executor(&mut connection_ref).await.unwrap();
    let actor = match executor.execute(GetActorAction::new()).await.unwrap().actor {
        nexigon_api::types::actor::Actor::Device(actor) => {
            info!(device_id = %actor.device_id);
            actor
        }
        _ => {
            bail!("received unexpected actor type");
        }
    };
    match &args.cmd {
        Cmd::Run => {
            connection_handle.await??;
        }
        Cmd::Device(cmd) => match cmd {
            DeviceCmd::Id => {
                println!("{}", actor.device_id);
            }
            DeviceCmd::Tokens(cmd) => match cmd {
                TokensCmd::Issue { valid_for, claims } => {
                    let claims = claims
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .context("claims must be valid JSON")?
                        .unwrap_or(serde_json::Value::Null);
                    let output = executor
                        .execute(
                            IssueDeviceTokenAction::new(actor.device_id.clone())
                                .with_claims(Some(claims))
                                .with_valid_for_secs(*valid_for),
                        )
                        .await
                        .context("unable to issue device token")?;
                    println!("{}", serde_json::to_string(&output).unwrap());
                }
            },
            DeviceCmd::Metadata(cmd) => match cmd {
                MetadataCmd::Set { metadata } => {
                    executor
                        .execute(SetDeviceMetadataAction::new(
                            actor.device_id.clone(),
                            serde_json::from_str(metadata)
                                .context("device metadata must be valid JSON")?,
                        ))
                        .await
                        .context("unable to set device metadata")?;
                }
            },
        },
        Cmd::Events(cmd) => match cmd {
            EventsCmd::Emit {
                severity,
                category,
                attributes,
                body,
            } => {
                let publish_events = PublishDeviceEventsAction::new(
                    actor.device_id.clone(),
                    vec![
                        DeviceEvent::new(
                            DeviceEventId::generate(),
                            severity.clone(),
                            serde_json::from_str(body).context("unable to parse event body")?,
                            {
                                let mut map = HashMap::new();
                                for attribute in attributes {
                                    let Some((key, value)) = attribute.split_once('=') else {
                                        bail!("invalid attribute: {attribute}")
                                    };
                                    map.insert(key.to_owned(), serde_json::from_str(value)?);
                                }
                                map
                            },
                            Timestamp::now(),
                        )
                        .with_category(category.clone()),
                    ],
                );
                executor
                    .execute(publish_events)
                    .await
                    .context("unable to emit event")?;
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
    config: PathBuf,
    /// Command.
    #[clap(subcommand)]
    cmd: Cmd,
}

/// CLI command.
#[derive(Debug, Parser)]
enum Cmd {
    /// Run the agent.
    Run,
    /// Device subcommand.
    #[clap(subcommand)]
    Device(DeviceCmd),
    /// Events subcommand.
    #[clap(subcommand)]
    Events(EventsCmd),
}

/// Device subcommand.
#[derive(Debug, Parser)]
enum DeviceCmd {
    /// Output the device id on stdout.
    Id,
    /// Tokens subcommand.
    #[clap(subcommand)]
    Tokens(TokensCmd),
    /// Metadata subcommand.
    #[clap(subcommand)]
    Metadata(MetadataCmd),
}

/// Tokens subcommand.
#[derive(Debug, Parser)]
enum TokensCmd {
    /// Issue a device token.
    Issue {
        /// Seconds for which the token should be valid.
        #[clap(long)]
        valid_for: Option<u32>,
        /// Additional JSON claims to attach to the token.
        #[clap(long)]
        claims: Option<String>,
    },
}

/// Metadata subcommand.
#[derive(Debug, Parser)]
enum MetadataCmd {
    /// Set the device's metadata.
    Set {
        /// Metadata JSON string.
        metadata: String,
    },
}

/// Events subcommand.
#[derive(Debug, Parser)]
enum EventsCmd {
    /// Emit an events.
    Emit {
        /// Event severity.
        #[clap(long, default_value = "info")]
        severity: DeviceEventSeverity,
        /// Event category.
        #[clap(long)]
        category: Option<String>,
        /// Event attribute.
        #[clap(long = "attribute")]
        attributes: Vec<String>,
        /// Event body.
        body: String,
    },
}
