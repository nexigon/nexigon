use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use futures::StreamExt;
use nexigon_api::types::devices::GetDevicePropertyAction;
use nexigon_api::types::devices::SetDevicePropertyAction;
use nexigon_api::types::repositories::IssueAssetDownloadUrlAction;
use nexigon_common::resolve_asset;
use tokio::net::TcpStream;
use tracing::debug;
use tracing::info;

use nexigon_api::types::actor::GetActorAction;
use nexigon_api::types::datetime::Timestamp;
use nexigon_api::types::devices::DeviceEvent;
use nexigon_api::types::devices::DeviceEventSeverity;
use nexigon_api::types::devices::IssueDeviceTokenAction;
use nexigon_api::types::devices::PublishDeviceEventsAction;
use nexigon_client::ClientIdentity;
use nexigon_client::ClientToken;
use nexigon_client::connect_executor;
use nexigon_ids::Generate;
use nexigon_ids::ids::DeviceEventId;
use nexigon_ids::ids::DeviceFingerprint;
use nexigon_multiplex::ConnectionEvent;

use crate::config::Config;
use crate::system_info::get_system_info;

pub mod config;
pub mod system_info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _logging_guard = si_observability::Initializer::new("NEXIGON")
        .apply(&args.logging)
        .init();
    info!("starting Nexigon Agent");
    let config_path = args
        .config
        .as_deref()
        .unwrap_or(Path::new("/etc/nexigon/agent.toml"))
        .canonicalize()
        .context("cannot canonicalize config path")?;
    let Some(config_dir) = config_path.parent() else {
        bail!("config path has no parent");
    };
    let config = toml::from_str::<Config>(
        &tokio::fs::read_to_string(&config_path)
            .await
            .context("cannot read config")?,
    )
    .context("cannot parse config")?;
    nexigon_client::install_crypto_provider();
    let cert_path = config_dir.join(
        config
            .ssl_cert
            .as_deref()
            .unwrap_or(Path::new("/etc/nexigon/agent/ssl/cert.pem")),
    );
    let key_path = config_dir.join(
        config
            .ssl_key
            .as_deref()
            .unwrap_or(Path::new("/etc/nexigon/agent/ssl/key.pem")),
    );
    if !cert_path.exists() {
        if key_path.exists() {
            bail!("found SSL key but certificate is missing");
        }
        info!(?cert_path, "generating SSL certificate and key");
        let (certificate, key) = nexigon_cert::generate_self_signed_certificate();
        if let Some(parent) = cert_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        if let Some(parent) = key_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::write(&cert_path, certificate.to_pem()).await?;
        tokio::fs::write(&key_path, key).await?;
    }
    let fingerprint_data =
        tokio::process::Command::new(config_dir.join(&config.fingerprint_script))
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .output()
            .await
            .context("running fingerprint script")?
            .stdout;
    let fingerprint = DeviceFingerprint::from_data(&fingerprint_data);
    let cert = tokio::fs::read_to_string(&cert_path)
        .await
        .context("cannot read certificate")?;
    let key = tokio::fs::read_to_string(&key_path)
        .await
        .context("cannot read private key")?;
    let identity = ClientIdentity::from_pem(&cert, &key).context("cannot parse identity")?;
    let mut connection = nexigon_client::ClientBuilder::new(
        config.hub_url.parse().context("cannot parse hub URL")?,
        ClientToken::DeploymentToken(config.token.clone()),
    )
    .with_identity(Some(identity))
    .with_device_fingerprint(Some(fingerprint))
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
                    debug!("channel request: {request:?}");
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
                            if let Err(error) =
                                tokio::io::copy_bidirectional(&mut channel, &mut tcp).await
                            {
                                debug!("forwarding error: {error}");
                            }
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
    let (actor, device_id) = match executor
        .execute(GetActorAction::new())
        .await
        .unwrap()
        .unwrap()
        .actor
    {
        nexigon_api::types::actor::Actor::Device(actor) => {
            info!(device_id = %actor.device_id);
            let device_id = actor.device_id.clone();
            (actor, device_id)
        }
        _ => {
            bail!("received unexpected actor type");
        }
    };
    match &args.cmd {
        Cmd::Run => {
            tokio::spawn(async move {
                loop {
                    let system_info = get_system_info(&config);
                    executor
                        .execute(SetDevicePropertyAction::new(
                            device_id.clone(),
                            "dev.nexigon.system.info".to_owned(),
                            serde_json::to_value(system_info).unwrap(),
                        ))
                        .await
                        .ok();
                    tokio::time::sleep(Duration::from_secs(30 * 60)).await;
                }
            });
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
                        .context("unable to issue device token")??;
                    println!("{}", serde_json::to_string(&output).unwrap());
                }
            },
            DeviceCmd::Properties(cmd) => match cmd {
                PropertiesCmd::Set { name, value } => {
                    executor
                        .execute(SetDevicePropertyAction::new(
                            actor.device_id.clone(),
                            name.clone(),
                            serde_json::from_str(value)
                                .context("device property value must be valid JSON")?,
                        ))
                        .await
                        .context("unable to set device property")??;
                }
                PropertiesCmd::Get { name } => {
                    let output = executor
                        .execute(GetDevicePropertyAction::new(
                            actor.device_id.clone(),
                            name.clone(),
                        ))
                        .await
                        .context("unable to get device property")??;
                    println!("{}", serde_json::to_string(&output).unwrap());
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
                    .context("unable to emit event")??;
            }
        },
        Cmd::Repositories(cmd) => match cmd {
            RepositoriesCmd::IssueUrl { asset } => {
                let asset_id = resolve_asset(&mut executor, asset).await?;
                let output = executor
                    .execute(IssueAssetDownloadUrlAction::new(asset_id))
                    .await
                    .context("unable to issue asset download URL")??;
                println!("{}", serde_json::to_string(&output).unwrap());
            }
        },
    }
    Ok(())
}

/// CLI arguments.
#[derive(Debug, Parser)]
#[clap(version)]
pub struct Args {
    /// Logging arguments.
    #[clap(flatten)]
    logging: si_observability::clap4::LoggingArgs,
    /// Configuration file.
    #[clap(long)]
    config: Option<PathBuf>,
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
    /// Repositories subcommand.
    #[clap(subcommand)]
    Repositories(RepositoriesCmd),
}

/// Device subcommand.
#[derive(Debug, Parser)]
enum DeviceCmd {
    /// Output the device id on stdout.
    Id,
    /// Tokens subcommand.
    #[clap(subcommand)]
    Tokens(TokensCmd),
    /// Properties subcommand.
    #[clap(subcommand)]
    Properties(PropertiesCmd),
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

/// Repositories subcommand.
#[derive(Debug, Parser)]
enum RepositoriesCmd {
    /// Request a pre-signed URL for downloading an asset.
    IssueUrl {
        /// Asset ID or path.
        asset: String,
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

/// Properties subcommand.
#[derive(Debug, Parser)]
enum PropertiesCmd {
    /// Set a device property.
    Set {
        /// Name of the property.
        name: String,
        /// Value of the property.
        value: String,
    },
    Get {
        /// Name of the property.
        name: String,
    },
}
