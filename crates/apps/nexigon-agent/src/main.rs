//! Nexigon Agent binary.
//!
//! A thin shim around the `nexigon_agent` library: parses CLI arguments,
//! initialises process-global state (logging + crypto provider) once, and
//! dispatches to the library or to one-shot CLI handlers.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use nexigon_agent::install_crypto_provider;
use nexigon_api::types::actor::Actor;
use nexigon_api::types::actor::GetActorAction;
use nexigon_api::types::datetime::Timestamp;
use nexigon_api::types::devices::DeviceEvent;
use nexigon_api::types::devices::DeviceEventSeverity;
use nexigon_api::types::devices::GetDevicePropertyAction;
use nexigon_api::types::devices::IssueDeviceTokenAction;
use nexigon_api::types::devices::PublishDeviceEventsAction;
use nexigon_api::types::devices::SetDevicePropertyAction;
use nexigon_client::ClientExecutor;
use nexigon_client::connect_executor;
use nexigon_common::RepositoriesCmd;
use nexigon_common::execute_repositories_cmd;
use nexigon_ids::Generate;
use nexigon_ids::ids::DeviceEventId;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _logging_guard = si_observability::Initializer::new("NEXIGON")
        .apply(&args.logging)
        .init();
    info!("starting Nexigon Agent");
    install_crypto_provider();

    let config_path: PathBuf = args
        .config
        .clone()
        .unwrap_or_else(|| Path::new("/etc/nexigon/agent.toml").to_path_buf());

    match args.cmd {
        Cmd::Run => {
            nexigon_agent::run(&config_path, async {
                if let Err(e) = tokio::signal::ctrl_c().await {
                    tracing::warn!("ctrl_c handler error: {e}");
                }
            })
            .await?;
        }
        Cmd::Device(cmd) => {
            let mut executor = OneShot::open(&config_path).await?;
            run_device_cmd(&mut executor.executor, &executor.actor, cmd).await?;
        }
        Cmd::Events(cmd) => {
            let mut executor = OneShot::open(&config_path).await?;
            run_events_cmd(&mut executor.executor, &executor.actor, cmd).await?;
        }
        Cmd::Repositories(cmd) => {
            let mut executor = OneShot::open(&config_path).await?;
            execute_repositories_cmd(&cmd, &mut executor.executor).await?;
        }
    }
    Ok(())
}

/// Connection scaffolding for one-shot CLI subcommands.
///
/// Spawns a background task that drives the connection while the executor is
/// in use; the spawn is aborted when this struct is dropped.
struct OneShot {
    executor: ClientExecutor,
    actor: nexigon_api::types::actor::DeviceActor,
    _connection_task: tokio::task::JoinHandle<()>,
}

impl OneShot {
    async fn open(config_path: &Path) -> anyhow::Result<Self> {
        let (config, config_dir) = nexigon_agent::load_config(config_path).await?;
        let connection = nexigon_agent::connect(&config, &config_dir, false).await?;
        let mut connection_ref = connection.make_ref();
        let connection_task = connection.spawn();
        let mut executor = connect_executor(&mut connection_ref)
            .await
            .context("cannot open executor channel")?;
        let actor = match executor
            .execute(GetActorAction::new())
            .await
            .context("cannot execute GetActor")?
            .map_err(|e| anyhow::anyhow!("GetActor failed: {}", e.message))?
            .actor
        {
            Actor::Device(actor) => actor,
            _ => bail!("received unexpected actor type"),
        };
        Ok(Self {
            executor,
            actor,
            _connection_task: connection_task,
        })
    }
}

async fn run_device_cmd(
    executor: &mut ClientExecutor,
    actor: &nexigon_api::types::actor::DeviceActor,
    cmd: DeviceCmd,
) -> anyhow::Result<()> {
    match cmd {
        DeviceCmd::Id => {
            println!("{}", actor.device_id);
        }
        DeviceCmd::Tokens(TokensCmd::Issue { valid_for, claims }) => {
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
                        .with_valid_for_secs(valid_for),
                )
                .await
                .context("unable to issue device token")??;
            println!("{}", serde_json::to_string(&output).unwrap());
        }
        DeviceCmd::Properties(PropertiesCmd::Set { name, value }) => {
            executor
                .execute(SetDevicePropertyAction::new(
                    actor.device_id.clone(),
                    name,
                    serde_json::from_str(&value)
                        .context("device property value must be valid JSON")?,
                ))
                .await
                .context("unable to set device property")??;
        }
        DeviceCmd::Properties(PropertiesCmd::Get { name }) => {
            let output = executor
                .execute(GetDevicePropertyAction::new(actor.device_id.clone(), name))
                .await
                .context("unable to get device property")??;
            println!("{}", serde_json::to_string(&output).unwrap());
        }
    }
    Ok(())
}

async fn run_events_cmd(
    executor: &mut ClientExecutor,
    actor: &nexigon_api::types::actor::DeviceActor,
    cmd: EventsCmd,
) -> anyhow::Result<()> {
    match cmd {
        EventsCmd::Emit {
            severity,
            category,
            attributes,
            emitted_at,
            body,
        } => {
            let timestamp = match emitted_at {
                Some(raw) => raw.parse().context("unable to parse `--emitted-at`")?,
                None => Timestamp::now(),
            };
            let publish_events = PublishDeviceEventsAction::new(
                actor.device_id.clone(),
                vec![
                    DeviceEvent::new(
                        DeviceEventId::generate(),
                        severity,
                        serde_json::from_str(&body).context("unable to parse event body")?,
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
                        timestamp,
                    )
                    .with_category(category),
                ],
            );
            executor
                .execute(publish_events)
                .await
                .context("unable to emit event")??;
        }
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
        /// Event timestamp (RFC 3339; defaults to now).
        #[clap(long = "emitted-at")]
        emitted_at: Option<String>,
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
