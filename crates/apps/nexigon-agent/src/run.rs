//! Long-running agent entry point.
//!
//! Used both by the binary's `run` subcommand and by callers that embed the
//! agent in-process (notably the test helper that hosts multiple agents
//! inside a single hub-side process).

use std::future::Future;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::bail;
use futures::StreamExt;
use nexigon_api::types::actor::Actor;
use nexigon_api::types::actor::GetActorAction;
use nexigon_api::types::devices::SetDevicePropertyAction;
use nexigon_client::WebsocketConnection;
use nexigon_client::connect_executor;
use nexigon_ids::ids::DeviceId;
use nexigon_multiplex::ConnectionEvent;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::config::Config;
use crate::handlers;
use crate::handlers::CommandRegistry;
use crate::system_info::get_system_info;

/// Run a Nexigon agent until `shutdown` resolves or the connection closes.
///
/// `shutdown` is awaited concurrently with the connection's event loop.
/// When it resolves, the connection is dropped (which closes any open
/// channels) and this function returns `Ok(())`.
///
/// The caller must ensure a Rustls crypto provider has been installed
/// before invoking `run`; see [`crate::install_crypto_provider`].
pub async fn run(
    config_path: &Path,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let (config, config_dir) = crate::load_config(config_path).await?;
    let connection = crate::connect(&config, &config_dir, true).await?;
    run_with_connection(config, &config_dir, connection, shutdown, None).await
}

/// Run the agent loop on an already-established connection.
///
/// `ready`, if provided, is fulfilled with the agent's [`DeviceId`] as soon
/// as the agent has registered with the hub — useful for in-process hosts
/// that need to expose the device id to a test harness before the agent's
/// long-running loop returns.
pub async fn run_with_connection(
    config: Arc<Config>,
    _config_dir: &Path,
    mut connection: WebsocketConnection,
    shutdown: impl Future<Output = ()> + Send + 'static,
    ready: Option<oneshot::Sender<DeviceId>>,
) -> anyhow::Result<()> {
    let mut connection_ref = connection.make_ref();

    let commands_enabled = config
        .commands
        .as_ref()
        .and_then(|h| h.enabled)
        .unwrap_or(false);
    let command_registry = if commands_enabled {
        let commands_dir = config
            .commands
            .as_ref()
            .and_then(|h| h.directory.as_deref())
            .unwrap_or(Path::new("/etc/nexigon/agent/commands"));
        let registry = CommandRegistry::load_external(commands_dir)
            .context("failed to load command definitions")?;
        Some(Arc::new(registry))
    } else {
        None
    };

    let event_loop_config = config.clone();
    let event_loop_registry = command_registry.clone();
    let event_loop = tokio::spawn(async move {
        let config = event_loop_config;
        let command_registry = event_loop_registry;
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown => {
                    debug!("shutdown signaled, stopping event loop");
                    return anyhow::Ok(());
                }
                event = connection.next() => {
                    let Some(event) = event else { break; };
                    match event {
                        Ok(ConnectionEvent::RequestChannel(request)) => {
                            debug!("channel request: {request:?}");
                            let endpoint = std::str::from_utf8(request.endpoint())
                                .context("invalid UTF-8 in endpoint")?;

                            if let Some(port_str) = endpoint.strip_prefix("forward/tcp/") {
                                let port: u16 = port_str.parse().context("invalid port")?;
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
                            } else if endpoint == "terminal" || endpoint.starts_with("terminal/") {
                                #[cfg(target_os = "linux")]
                                {
                                    let terminal_enabled = config
                                        .terminal
                                        .as_ref()
                                        .and_then(|t| t.enabled)
                                        .unwrap_or(false);
                                    if !terminal_enabled {
                                        request.reject(b"terminal not enabled");
                                        continue;
                                    }
                                    let requested_user =
                                        endpoint.strip_prefix("terminal/").map(|u| u.to_owned());
                                    let config = config.clone();
                                    request.accept(move |channel| {
                                        tokio::spawn(async move {
                                            if let Err(e) = crate::terminal::handle_terminal_session(
                                                channel,
                                                &config,
                                                requested_user.as_deref(),
                                            )
                                            .await
                                            {
                                                warn!("terminal session error: {e:?}");
                                            }
                                        });
                                    });
                                }
                                #[cfg(not(target_os = "linux"))]
                                {
                                    request.reject(b"terminal not supported on this platform");
                                }
                            } else if endpoint == "handler" {
                                let Some(registry) = command_registry.as_ref() else {
                                    request.reject(b"commands not enabled");
                                    continue;
                                };
                                let config = config.clone();
                                let registry = registry.clone();
                                request.accept(move |channel| {
                                    tokio::spawn(async move {
                                        if let Err(e) = handlers::handle_handler_channel(
                                            channel, &config, &registry,
                                        )
                                        .await
                                        {
                                            warn!("handler channel error: {e:?}");
                                        }
                                    });
                                });
                            } else {
                                warn!(endpoint, "unknown endpoint requested");
                                request.reject(b"unknown endpoint");
                            }
                        }
                        Ok(ConnectionEvent::Connected) => { /* ignore */ }
                        Ok(ConnectionEvent::Closed) => {
                            info!("connection closed");
                            return anyhow::Ok(());
                        }
                        Err(error) => {
                            bail!("connection error: {error}");
                        }
                    }
                }
            }
        }
        anyhow::Ok(())
    });

    let mut executor = connect_executor(&mut connection_ref)
        .await
        .context("cannot open executor channel")?;
    let device_id = match executor
        .execute(GetActorAction::new())
        .await
        .context("cannot execute GetActor")?
        .map_err(|e| anyhow::anyhow!("GetActor failed: {}", e.message))?
        .actor
    {
        Actor::Device(actor) => {
            info!(device_id = %actor.device_id);
            actor.device_id
        }
        _ => bail!("received unexpected actor type"),
    };

    if let Some(ready) = ready {
        let _ = ready.send(device_id.clone());
    }

    let system_info_enabled = config
        .telemetry
        .as_ref()
        .and_then(|t| t.system_info)
        .unwrap_or(true);
    if system_info_enabled {
        let sysinfo_config = config.clone();
        let sysinfo_device_id = device_id.clone();
        let mut sysinfo_executor = connect_executor(&mut connection_ref)
            .await
            .context("cannot open sysinfo executor channel")?;
        tokio::spawn(async move {
            loop {
                let system_info = get_system_info(&sysinfo_config);
                sysinfo_executor
                    .execute(SetDevicePropertyAction::new(
                        sysinfo_device_id.clone(),
                        "dev.nexigon.system.info".to_owned(),
                        serde_json::to_value(system_info).unwrap(),
                    ))
                    .await
                    .ok();
                tokio::time::sleep(Duration::from_secs(30 * 60)).await;
            }
        });
    }

    if let Some(registry) = &command_registry {
        let manifest = registry.manifest();
        executor
            .execute(SetDevicePropertyAction::new(
                device_id.clone(),
                "dev.nexigon.commands".to_owned(),
                serde_json::to_value(manifest).unwrap(),
            ))
            .await
            .ok();
    }

    #[cfg(unix)]
    let local_api = spawn_local_api(&config, &connection_ref);

    let result = event_loop.await?;

    #[cfg(unix)]
    if let Some((handle, shutdown_tx)) = local_api {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    }

    result?;
    Ok(())
}

#[cfg(unix)]
fn spawn_local_api(
    config: &Config,
    connection_ref: &nexigon_multiplex::ConnectionRef,
) -> Option<(tokio::task::JoinHandle<()>, oneshot::Sender<()>)> {
    use crate::config::LocalApiConfig;

    let local_api_config = config.local_api.clone();
    let enabled = local_api_config
        .as_ref()
        .and_then(|cfg| cfg.enabled)
        .unwrap_or(true);
    if !enabled {
        return None;
    }
    let local_api_config = local_api_config.unwrap_or(LocalApiConfig {
        enabled: None,
        socket_path: None,
    });
    let hub_ref = connection_ref.clone();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let shutdown = async move {
            let _ = shutdown_rx.await;
        };
        if let Err(e) = crate::local_api::serve(&local_api_config, hub_ref, shutdown).await {
            warn!("local API server error: {e:#}");
        }
    });
    Some((handle, shutdown_tx))
}
