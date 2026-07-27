//! Long-running agent entry point.
//!
//! Used both by the binary's `run` subcommand and by callers that embed the
//! agent in-process (notably the test helper that hosts multiple agents
//! inside a single hub-side process).

use std::future::Future;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::bail;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use nexigon_agent_protocol::MAX_CONCURRENT_COMMANDS;
use nexigon_api::types::actor::Actor;
use nexigon_api::types::actor::GetActorAction;
use nexigon_api::types::devices::ClaimDeviceOperationWorkAction;
use nexigon_api::types::devices::DeviceCommandInvokeData;
use nexigon_api::types::devices::DeviceCommandStatus;
use nexigon_api::types::devices::DeviceOperationId;
use nexigon_api::types::devices::DeviceOperationStepReport;
use nexigon_api::types::devices::DeviceOperationStepReportStatus;
use nexigon_api::types::devices::DeviceOperationWorkKind;
use nexigon_api::types::devices::DeviceOperationWorkStep;
use nexigon_api::types::devices::ReportDeviceOperationStepAction;
use nexigon_api::types::devices::SetDevicePropertyAction;
use nexigon_client::ClientExecutor;
use nexigon_client::WebsocketConnection;
use nexigon_client::connect_executor;
use nexigon_ids::ids::DeviceId;
use nexigon_ids::ids::DeviceOperationWorkClaimId;
use nexigon_multiplex::ConnectionEvent;
use tokio::net::TcpStream;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::config::Config;
use crate::config::OperationsConfig;
use crate::handlers;
use crate::handlers::CommandRegistry;
use crate::operation_ledger::OperationLedger;
use crate::operation_ledger::PreviousExecution;
use crate::system_info::get_system_info;

const MAX_CONCURRENT_TCP_FORWARDINGS: usize = 16;
#[cfg(target_os = "linux")]
const MAX_CONCURRENT_TERMINALS: usize = 4;
#[cfg(not(target_os = "linux"))]
const MAX_CONCURRENT_TERMINALS: usize = 0;
const SUPERVISOR_QUEUE_CAPACITY: usize =
    MAX_CONCURRENT_TCP_FORWARDINGS + MAX_CONCURRENT_TERMINALS + MAX_CONCURRENT_COMMANDS;
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TASK_SHUTDOWN_GRACE: Duration = Duration::from_secs(8);

type BoxedTask = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskKind {
    Connection,
    ShutdownSignal,
    TcpConnect,
    TcpForward,
    #[cfg(target_os = "linux")]
    Terminal,
    Handler,
    SystemInfo,
    Operations,
    #[cfg(unix)]
    LocalApi,
}

impl TaskKind {
    const fn description(self) -> &'static str {
        match self {
            Self::Connection => "hub connection",
            Self::ShutdownSignal => "shutdown signal",
            Self::TcpConnect => "TCP forwarding connection",
            Self::TcpForward => "TCP forwarding relay",
            #[cfg(target_os = "linux")]
            Self::Terminal => "terminal session",
            Self::Handler => "command handler",
            Self::SystemInfo => "system-info publisher",
            Self::Operations => "operation poller",
            #[cfg(unix)]
            Self::LocalApi => "local API listener",
        }
    }
}

struct SupervisedTask {
    kind: TaskKind,
    future: BoxedTask,
}

impl SupervisedTask {
    fn new(
        kind: TaskKind,
        future: impl Future<Output = anyhow::Result<()>> + Send + 'static,
    ) -> Self {
        Self {
            kind,
            future: Box::pin(future),
        }
    }
}

struct TaskCompletion {
    kind: TaskKind,
    result: anyhow::Result<()>,
}

#[derive(Clone)]
struct EndpointLimits {
    tcp_forwardings: Arc<Semaphore>,
    #[cfg(target_os = "linux")]
    terminals: Arc<Semaphore>,
    commands: Arc<Semaphore>,
}

impl EndpointLimits {
    fn new(commands: Arc<Semaphore>) -> Self {
        Self {
            tcp_forwardings: Arc::new(Semaphore::new(MAX_CONCURRENT_TCP_FORWARDINGS)),
            #[cfg(target_os = "linux")]
            terminals: Arc::new(Semaphore::new(MAX_CONCURRENT_TERMINALS)),
            commands,
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

fn spawn_supervised(tasks: &mut JoinSet<TaskCompletion>, task: SupervisedTask) {
    tasks.spawn(async move {
        let kind = task.kind;
        let result = AssertUnwindSafe(task.future)
            .catch_unwind()
            .await
            .map_err(|payload| {
                anyhow::anyhow!(
                    "{} task panicked: {}",
                    kind.description(),
                    panic_message(payload)
                )
            })
            .and_then(|result| result);
        TaskCompletion { kind, result }
    });
}

async fn run_until_cancelled<T>(
    cancellation: &CancellationToken,
    future: impl Future<Output = T>,
) -> Option<T> {
    tokio::pin!(future);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => None,
        result = &mut future => Some(result),
    }
}

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
    crate::run_agent(config_path.to_path_buf(), shutdown, None).await
}

/// Run the agent loop on an already-established connection.
///
/// `ready`, if provided, is fulfilled with the agent's [`DeviceId`] as soon
/// as the agent has registered with the hub — useful for in-process hosts
/// that need to expose the device id to a test harness before the agent's
/// long-running loop returns.
pub async fn run_with_connection(
    config: Arc<Config>,
    config_dir: &Path,
    connection: WebsocketConnection,
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
    let command_slots = command_slots();
    let endpoint_limits = EndpointLimits::new(command_slots.clone());
    let cancellation = CancellationToken::new();
    let (task_tx, mut task_rx) = mpsc::channel(SUPERVISOR_QUEUE_CAPACITY);
    let mut tasks = JoinSet::new();

    let shutdown_cancellation = cancellation.clone();
    spawn_supervised(
        &mut tasks,
        SupervisedTask::new(TaskKind::ShutdownSignal, async move {
            tokio::select! {
                () = shutdown => {
                    debug!("shutdown signaled, stopping agent task group");
                    shutdown_cancellation.cancel();
                }
                () = shutdown_cancellation.cancelled() => {}
            }
            Ok(())
        }),
    );

    let event_loop_config = config.clone();
    let event_loop_registry = command_registry.clone();
    let event_loop_cancellation = cancellation.clone();
    let event_loop_task_tx = task_tx.clone();
    spawn_supervised(
        &mut tasks,
        SupervisedTask::new(TaskKind::Connection, async move {
            run_connection_event_loop(
                connection,
                event_loop_config,
                event_loop_registry,
                endpoint_limits,
                event_loop_task_tx,
                event_loop_cancellation,
            )
            .await
        }),
    );

    let agent_run = async {
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
            let sysinfo_cancellation = cancellation.clone();
            spawn_supervised(
                &mut tasks,
                SupervisedTask::new(TaskKind::SystemInfo, async move {
                    loop {
                        let system_info = get_system_info(&sysinfo_config);
                        let value = serde_json::to_value(system_info)
                            .context("cannot serialize system information")?;
                        let update = sysinfo_executor.execute(SetDevicePropertyAction::new(
                            sysinfo_device_id.clone(),
                            "dev.nexigon.system.info".to_owned(),
                            value,
                        ));
                        let update = tokio::select! {
                            () = sysinfo_cancellation.cancelled() => return Ok(()),
                            update = update => update,
                        };
                        match update {
                            Ok(Ok(_)) => {}
                            Ok(Err(error)) => {
                                warn!(message = %error.message, "system-info update rejected");
                            }
                            Err(error) => {
                                warn!(?error, "failed to publish system information");
                            }
                        }
                        tokio::select! {
                            () = sysinfo_cancellation.cancelled() => return Ok(()),
                            () = tokio::time::sleep(Duration::from_secs(30 * 60)) => {}
                        }
                    }
                }),
            );
        }

        if let Some(registry) = &command_registry {
            let manifest = registry.manifest();
            let manifest =
                serde_json::to_value(manifest).context("cannot serialize command manifest")?;
            if let Err(error) = executor
                .execute(SetDevicePropertyAction::new(
                    device_id.clone(),
                    "dev.nexigon.commands".to_owned(),
                    manifest,
                ))
                .await
            {
                warn!(?error, "failed to publish command manifest");
            }
        }

        let operations_enabled = operation_polling_enabled(config.operations.as_ref());
        if operations_enabled {
            let mut operation_ledger =
                OperationLedger::load(&crate::data_path(&config, config_dir))
                    .await
                    .context("cannot open operation execution ledger")?;
            let poll_interval = Duration::from_secs(
                config
                    .operations
                    .as_ref()
                    .and_then(|o| o.poll_interval_secs)
                    .unwrap_or(60),
            );
            let operations_command_registry = command_registry.clone();
            let operations_command_slots = command_slots;
            let operations_device_id = device_id.clone();
            let mut operations_executor = connect_executor(&mut connection_ref)
                .await
                .context("cannot open operations executor channel")?;
            let operation_cancellation = cancellation.clone();
            let operation_loop = async move {
                loop {
                    if operation_cancellation.is_cancelled() {
                        return Ok(());
                    }
                    // Reserve execution capacity before claiming work so a lease never
                    // waits behind already-running interactive commands.
                    let command_permit = if operations_command_registry.is_some() {
                        let permit = tokio::select! {
                            biased;
                            () = operation_cancellation.cancelled() => return Ok(()),
                            permit = operations_command_slots.clone().acquire_owned() => permit,
                        };
                        match permit {
                            Ok(permit) => Some(permit),
                            Err(_) => {
                                anyhow::bail!("command concurrency limiter was closed");
                            }
                        }
                    } else {
                        None
                    };
                    // Whether this poll found anything. The hub releases a device operation's
                    // next step as soon as the previous one is reported, so a poll that found
                    // work is very likely to find more; sleeping out the whole idle interval
                    // between the steps of one operation would waste that. Only an empty poll
                    // means there is genuinely nothing to do.
                    let mut found_work = false;
                    let claim = operations_executor.execute(
                        ClaimDeviceOperationWorkAction::new(operations_device_id.clone())
                            // Commands are executed serially. Lease only the command we
                            // are about to execute so later work cannot expire while an
                            // earlier handler is still running.
                            .with_limit(Some(1))
                            .with_kinds(Some(vec![DeviceOperationWorkKind::DeviceCommand])),
                    );
                    let result = tokio::select! {
                        biased;
                        () = operation_cancellation.cancelled() => return Ok(()),
                        result = claim => result,
                    };
                    match result {
                        Ok(Ok(output)) => {
                            for item in output.work {
                                let device_operation_id = item.device_operation_id;
                                let step_index = item.step_index;
                                let claim_id = item.claim_id;
                                let report = match operation_ledger
                                    .previous(&device_operation_id, step_index)
                                {
                                    PreviousExecution::Completed(report) => {
                                        // A completed result may be returning under a fresh
                                        // lease after an earlier reporting outage. Commit the
                                        // current claim before trying it again so a crash
                                        // cannot leave the outbox tied to the expired claim.
                                        if let Err(error) = operation_ledger
                                            .mark_completed(
                                                &device_operation_id,
                                                step_index,
                                                &claim_id,
                                                report.clone(),
                                            )
                                            .await
                                        {
                                            warn!(
                                                ?error,
                                                "failed to persist renewed operation claim"
                                            );
                                            continue;
                                        }
                                        report
                                    }
                                    PreviousExecution::InProgress => {
                                        let report = DeviceOperationStepReport {
                                            status: DeviceOperationStepReportStatus::Failed,
                                            output: None,
                                            checkpoint: None,
                                            error: Some(
                                                "previous command execution was interrupted after dispatch; refusing unsafe automatic replay"
                                                    .to_owned(),
                                            ),
                                        };
                                        if let Err(error) = operation_ledger
                                            .mark_completed(
                                                &device_operation_id,
                                                step_index,
                                                &claim_id,
                                                report.clone(),
                                            )
                                            .await
                                        {
                                            warn!(
                                                ?error,
                                                "failed to persist interrupted operation result"
                                            );
                                            continue;
                                        }
                                        report
                                    }
                                    PreviousExecution::None => match item.step {
                                        DeviceOperationWorkStep::DeviceCommand(step) => {
                                            if let Err(error) = operation_ledger
                                                .mark_in_progress(
                                                    &device_operation_id,
                                                    step_index,
                                                    &claim_id,
                                                )
                                                .await
                                            {
                                                warn!(
                                                    ?error,
                                                    "failed to persist operation dispatch; command not executed"
                                                );
                                                continue;
                                            }
                                            let done = if let Some(registry) =
                                                operations_command_registry.as_ref()
                                            {
                                                let request = DeviceCommandInvokeData::new(
                                                    step.command,
                                                    step.input,
                                                )
                                                .with_stream_log(Some(false))
                                                .with_timeout_secs(Some(
                                                    step.timeout_secs.unwrap_or(3600),
                                                ));
                                                handlers::invoke_registered_command_with_cancellation(
                                                    registry,
                                                    request,
                                                    &operation_cancellation,
                                                )
                                                .await
                                            } else {
                                                nexigon_api::types::devices::DeviceCommandDoneData {
                                                    status: DeviceCommandStatus::Error,
                                                    output: None,
                                                    error: Some("commands not enabled".to_owned()),
                                                    log_tail: Vec::new(),
                                                    duration_ms: 0,
                                                }
                                            };
                                            if operation_cancellation.is_cancelled() {
                                                return Ok(());
                                            }
                                            let status = match done.status {
                                                DeviceCommandStatus::Ok => {
                                                    DeviceOperationStepReportStatus::Succeeded
                                                }
                                                DeviceCommandStatus::Error => {
                                                    DeviceOperationStepReportStatus::Failed
                                                }
                                            };
                                            let report = DeviceOperationStepReport {
                                                status,
                                                output: done.output,
                                                checkpoint: None,
                                                error: done.error,
                                            };
                                            if let Err(error) = operation_ledger
                                                .mark_completed(
                                                    &device_operation_id,
                                                    step_index,
                                                    &claim_id,
                                                    report.clone(),
                                                )
                                                .await
                                            {
                                                warn!(
                                                    ?error,
                                                    "failed to persist operation result; result not reported"
                                                );
                                                continue;
                                            }
                                            report
                                        }
                                        DeviceOperationWorkStep::DeviceTask(_) => {
                                            debug!(
                                                device_operation_id = %device_operation_id,
                                                step_index,
                                                "device task work is not handled by the agent"
                                            );
                                            continue;
                                        }
                                    },
                                };
                                found_work = true;
                                // Retry transport failures promptly while this claim's lease
                                // is certainly still current. The shortest valid command
                                // lease is 61 seconds; these five attempts span 15 seconds.
                                // A persistent outage falls back to normal lease expiry and
                                // reacquisition, at which point the saved result is submitted
                                // under the fresh claim without executing again.
                                let acknowledged = report_operation_step_with_retry(
                                    &mut operations_executor,
                                    &operations_device_id,
                                    &device_operation_id,
                                    step_index,
                                    &claim_id,
                                    &report,
                                    Some(&operation_cancellation),
                                )
                                .await;
                                if acknowledged
                                    && let Err(error) = operation_ledger
                                        .remove(&device_operation_id, step_index)
                                        .await
                                {
                                    warn!(?error, "failed to prune reported operation result");
                                }
                            }
                        }
                        Ok(Err(error)) => {
                            warn!(message = %error.message, "operation work claim rejected");
                        }
                        Err(error) => {
                            warn!(?error, "failed to claim operation work");
                        }
                    }
                    drop(command_permit);
                    if !found_work {
                        tokio::select! {
                            biased;
                            () = operation_cancellation.cancelled() => return Ok(()),
                            () = tokio::time::sleep(poll_interval) => {}
                        }
                    }
                }
            };
            spawn_supervised(
                &mut tasks,
                SupervisedTask::new(TaskKind::Operations, operation_loop),
            );
        }

        #[cfg(unix)]
        if let Some(local_api) = local_api_task(&config, &connection_ref, cancellation.clone()) {
            spawn_supervised(&mut tasks, local_api);
        }

        drop(executor);
        supervise_tasks(&mut tasks, &mut task_rx, &cancellation).await
    };
    let result = run_until_cancelled(&cancellation, agent_run)
        .await
        .unwrap_or(Ok(()));

    cancellation.cancel();
    drop(task_tx);
    task_rx.close();
    while task_rx.try_recv().is_ok() {}
    drop(task_rx);
    shutdown_tasks(&mut tasks).await;
    result
}

async fn run_connection_event_loop<S, E>(
    connection: S,
    config: Arc<Config>,
    command_registry: Option<Arc<CommandRegistry>>,
    limits: EndpointLimits,
    task_tx: mpsc::Sender<SupervisedTask>,
    cancellation: CancellationToken,
) -> anyhow::Result<()>
where
    S: Stream<Item = Result<ConnectionEvent, E>> + Send + 'static,
    E: std::fmt::Display,
{
    tokio::pin!(connection);
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(()),
            event = connection.next() => {
                let Some(event) = event else {
                    info!("connection stream ended");
                    return Ok(());
                };
                match event {
                    Ok(ConnectionEvent::RequestChannel(request)) => {
                        handle_channel_request(
                            request,
                            &config,
                            command_registry.as_ref(),
                            &limits,
                            &task_tx,
                            &cancellation,
                        );
                    }
                    Ok(ConnectionEvent::Connected) => {}
                    Ok(ConnectionEvent::Closed) => {
                        info!("connection closed");
                        return Ok(());
                    }
                    Err(error) => bail!("connection error: {error}"),
                }
            }
        }
    }
}

fn handle_channel_request(
    request: nexigon_multiplex::ChannelRequest,
    config: &Arc<Config>,
    command_registry: Option<&Arc<CommandRegistry>>,
    limits: &EndpointLimits,
    task_tx: &mpsc::Sender<SupervisedTask>,
    cancellation: &CancellationToken,
) {
    let endpoint = match std::str::from_utf8(request.endpoint()) {
        Ok(endpoint) => endpoint,
        Err(_) => {
            request.reject(b"invalid endpoint");
            return;
        }
    };
    debug!(endpoint, "channel request");

    if let Some(port_str) = endpoint.strip_prefix("forward/tcp/") {
        let Ok(port) = port_str.parse::<u16>() else {
            request.reject(b"invalid TCP forwarding endpoint");
            return;
        };
        let Ok(forwarding_permit) = limits.tcp_forwardings.clone().try_acquire_owned() else {
            request.reject(b"too many concurrent TCP forwardings");
            return;
        };
        let Ok(task_slot) = task_tx.clone().try_reserve_owned() else {
            request.reject(b"agent task queue is full");
            return;
        };
        task_slot.send(SupervisedTask::new(
            TaskKind::TcpConnect,
            connect_tcp_forwarding(
                request,
                port,
                forwarding_permit,
                task_tx.clone(),
                cancellation.clone(),
            ),
        ));
        return;
    }

    if endpoint == "terminal" || endpoint.starts_with("terminal/") {
        #[cfg(target_os = "linux")]
        {
            if !crate::config::terminal_enabled(config) {
                request.reject(b"terminal not enabled or terminal user invalid");
                return;
            }
            let Ok(terminal_permit) = limits.terminals.clone().try_acquire_owned() else {
                request.reject(b"too many concurrent terminal sessions");
                return;
            };
            let Ok(task_slot) = task_tx.clone().try_reserve_owned() else {
                request.reject(b"agent task queue is full");
                return;
            };
            let requested_user = endpoint.strip_prefix("terminal/").map(str::to_owned);
            let config = config.clone();
            let cancellation = cancellation.clone();
            request.accept(move |channel| {
                task_slot.send(SupervisedTask::new(TaskKind::Terminal, async move {
                    let _terminal_permit = terminal_permit;
                    crate::terminal::handle_terminal_session_with_cancellation(
                        channel,
                        &config,
                        requested_user.as_deref(),
                        cancellation,
                    )
                    .await
                }));
            });
            return;
        }
        #[cfg(not(target_os = "linux"))]
        {
            request.reject(b"terminal not supported on this platform");
            return;
        }
    }

    if endpoint == "handler" {
        let Some(registry) = command_registry else {
            request.reject(b"commands not enabled");
            return;
        };
        let Ok(command_permit) = limits.commands.clone().try_acquire_owned() else {
            request.reject(b"too many concurrent commands");
            return;
        };
        let Ok(task_slot) = task_tx.clone().try_reserve_owned() else {
            request.reject(b"agent task queue is full");
            return;
        };
        let config = config.clone();
        let registry = registry.clone();
        let cancellation = cancellation.clone();
        request.accept(move |channel| {
            task_slot.send(SupervisedTask::new(TaskKind::Handler, async move {
                let _command_permit = command_permit;
                handlers::handle_handler_channel_with_cancellation(
                    channel,
                    &config,
                    &registry,
                    cancellation,
                )
                .await
            }));
        });
        return;
    }

    warn!(endpoint, "unknown endpoint requested");
    request.reject(b"unknown endpoint");
}

async fn connect_tcp_forwarding(
    request: nexigon_multiplex::ChannelRequest,
    port: u16,
    forwarding_permit: OwnedSemaphorePermit,
    task_tx: mpsc::Sender<SupervisedTask>,
    cancellation: CancellationToken,
) -> anyhow::Result<()> {
    let address = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    let connect = tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(address));
    let mut tcp = tokio::select! {
        () = cancellation.cancelled() => return Ok(()),
        result = connect => match result {
            Ok(Ok(tcp)) => tcp,
            Ok(Err(error)) => {
                debug!(?error, %address, "local TCP forwarding target is unavailable");
                request.reject(b"local TCP forwarding target is unavailable");
                return Ok(());
            }
            Err(_) => {
                debug!(%address, "local TCP forwarding connection timed out");
                request.reject(b"local TCP forwarding target timed out");
                return Ok(());
            }
        }
    };

    let Ok(task_slot) = task_tx.try_reserve_owned() else {
        request.reject(b"agent task queue is full");
        return Ok(());
    };
    request.accept(move |mut channel| {
        task_slot.send(SupervisedTask::new(TaskKind::TcpForward, async move {
            let _forwarding_permit = forwarding_permit;
            tokio::select! {
                () = cancellation.cancelled() => Ok(()),
                result = tokio::io::copy_bidirectional(&mut channel, &mut tcp) => {
                    result
                        .map(|_| ())
                        .context("TCP forwarding relay failed")
                }
            }
        }));
    });
    Ok(())
}

async fn supervise_tasks(
    tasks: &mut JoinSet<TaskCompletion>,
    task_rx: &mut mpsc::Receiver<SupervisedTask>,
    cancellation: &CancellationToken,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            biased;
            completion = tasks.join_next() => {
                let Some(completion) = completion else {
                    bail!("agent task group stopped unexpectedly");
                };
                let completion = completion.context("supervised agent task was cancelled")?;
                if completion.kind == TaskKind::Connection {
                    return completion.result;
                }
                if completion.kind == TaskKind::ShutdownSignal && cancellation.is_cancelled() {
                    return Ok(());
                }
                match completion.result {
                    Ok(()) => {
                        debug!(task = completion.kind.description(), "agent child task finished");
                    }
                    Err(error) => {
                        warn!(
                            task = completion.kind.description(),
                            ?error,
                            "agent child task failed"
                        );
                    }
                }
            }
            () = cancellation.cancelled() => return Ok(()),
            task = task_rx.recv() => {
                let Some(task) = task else {
                    bail!("agent task queue closed unexpectedly");
                };
                spawn_supervised(tasks, task);
            }
        }
    }
}

async fn shutdown_tasks(tasks: &mut JoinSet<TaskCompletion>) {
    let graceful = async {
        while let Some(completion) = tasks.join_next().await {
            match completion {
                Ok(TaskCompletion {
                    result: Err(error),
                    kind,
                }) => {
                    debug!(
                        task = kind.description(),
                        ?error,
                        "agent task failed during shutdown"
                    );
                }
                Ok(TaskCompletion { result: Ok(()), .. }) => {}
                Err(error) => warn!(?error, "agent task join failed during shutdown"),
            }
        }
    };
    if tokio::time::timeout(TASK_SHUTDOWN_GRACE, graceful)
        .await
        .is_err()
    {
        warn!("agent tasks did not stop within the shutdown grace period; aborting");
        tasks.abort_all();
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                warn!(?error, "agent task join failed after abort");
            }
        }
    }
}

fn operation_polling_enabled(config: Option<&OperationsConfig>) -> bool {
    config
        .and_then(|operations| operations.enabled)
        .unwrap_or(false)
}

fn command_slots() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_CONCURRENT_COMMANDS))
}

#[derive(Debug)]
enum ReportAttempt {
    Acknowledged,
    Rejected(String),
    TransportFailed(String),
}

trait OperationReporter {
    fn report(
        &mut self,
        device_id: DeviceId,
        operation_id: DeviceOperationId,
        step_index: u32,
        claim_id: DeviceOperationWorkClaimId,
        report: DeviceOperationStepReport,
    ) -> impl Future<Output = ReportAttempt>;
}

impl OperationReporter for ClientExecutor {
    async fn report(
        &mut self,
        device_id: DeviceId,
        operation_id: DeviceOperationId,
        step_index: u32,
        claim_id: DeviceOperationWorkClaimId,
        report: DeviceOperationStepReport,
    ) -> ReportAttempt {
        match self
            .execute(ReportDeviceOperationStepAction::new(
                device_id,
                operation_id,
                step_index,
                claim_id,
                report,
            ))
            .await
        {
            Ok(Ok(_)) => ReportAttempt::Acknowledged,
            Ok(Err(error)) => ReportAttempt::Rejected(error.message),
            Err(error) => ReportAttempt::TransportFailed(error.to_string()),
        }
    }
}

async fn report_operation_step_with_retry(
    reporter: &mut impl OperationReporter,
    device_id: &DeviceId,
    operation_id: &DeviceOperationId,
    step_index: u32,
    claim_id: &DeviceOperationWorkClaimId,
    report: &DeviceOperationStepReport,
    cancellation: Option<&CancellationToken>,
) -> bool {
    let mut retry_delay = Duration::from_secs(1);
    for attempt in 1..=5 {
        let report_attempt = reporter.report(
            device_id.clone(),
            operation_id.clone(),
            step_index,
            claim_id.clone(),
            report.clone(),
        );
        tokio::pin!(report_attempt);
        let report_attempt = if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return false,
                result = &mut report_attempt => result,
            }
        } else {
            report_attempt.await
        };
        match report_attempt {
            ReportAttempt::Acknowledged => return true,
            ReportAttempt::Rejected(message) => {
                warn!(%message, "operation step report rejected");
                return false;
            }
            ReportAttempt::TransportFailed(error) => {
                warn!(%error, attempt, "failed to report operation step");
                if attempt < 5 {
                    if let Some(cancellation) = cancellation {
                        tokio::select! {
                            biased;
                            () = cancellation.cancelled() => return false,
                            () = tokio::time::sleep(retry_delay) => {}
                        }
                    } else {
                        tokio::time::sleep(retry_delay).await;
                    }
                    retry_delay *= 2;
                }
            }
        }
    }
    false
}

#[cfg(unix)]
fn local_api_task(
    config: &Config,
    connection_ref: &nexigon_multiplex::ConnectionRef,
    cancellation: CancellationToken,
) -> Option<SupervisedTask> {
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
    Some(SupervisedTask::new(TaskKind::LocalApi, async move {
        crate::local_api::serve(&local_api_config, hub_ref, cancellation.cancelled_owned()).await
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::Ipv4Addr;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use anyhow::Context;
    use bytes::Bytes;
    use futures::StreamExt;
    use nexigon_api::types::devices::DeviceOperationStepReportStatus;
    use nexigon_ids::Generate;
    use nexigon_multiplex::Connection;
    use nexigon_multiplex::ConnectionEvent;
    use nexigon_multiplex::ConnectionRef;
    use nexigon_multiplex::transport::InMemory;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;
    use tokio::task::JoinSet;
    use tokio_util::sync::CancellationToken;

    use super::DeviceId;
    use super::DeviceOperationId;
    use super::DeviceOperationStepReport;
    use super::DeviceOperationWorkClaimId;
    use super::EndpointLimits;
    use super::MAX_CONCURRENT_TCP_FORWARDINGS;
    #[cfg(target_os = "linux")]
    use super::MAX_CONCURRENT_TERMINALS;
    use super::OperationReporter;
    use super::ReportAttempt;
    use super::SUPERVISOR_QUEUE_CAPACITY;
    use super::SupervisedTask;
    use super::TaskKind;
    use super::command_slots;
    use super::operation_polling_enabled;
    use super::report_operation_step_with_retry;
    use super::run_connection_event_loop;
    use super::run_until_cancelled;
    use super::shutdown_tasks;
    use super::spawn_supervised;
    use super::supervise_tasks;
    use crate::config::Config;
    use crate::config::OperationsConfig;

    struct EndpointTestAgent {
        hub_ref: ConnectionRef,
        cancellation: CancellationToken,
        agent: JoinHandle<anyhow::Result<()>>,
        hub: JoinHandle<()>,
    }

    impl EndpointTestAgent {
        async fn start() -> Self {
            let (hub_transport, agent_transport) = InMemory::<Bytes, Bytes>::new_buffered(64);
            let mut hub_connection = Connection::new(hub_transport);
            let agent_connection = Connection::new(agent_transport);
            let hub_ref = hub_connection.make_ref();
            let hub = tokio::spawn(async move {
                while let Some(event) = hub_connection.next().await {
                    match event {
                        Ok(ConnectionEvent::Connected) => {}
                        Ok(ConnectionEvent::RequestChannel(request)) => {
                            request.reject(b"not supported by test hub");
                        }
                        Ok(ConnectionEvent::Closed) | Err(_) => break,
                    }
                }
            });

            let cancellation = CancellationToken::new();
            let agent_cancellation = cancellation.clone();
            let agent = tokio::spawn(async move {
                let config = Arc::new(Config::new(PathBuf::from("unused-fingerprint")));
                let limits = EndpointLimits::new(command_slots());
                let (task_tx, mut task_rx) = mpsc::channel(SUPERVISOR_QUEUE_CAPACITY);
                let mut tasks = JoinSet::new();
                spawn_supervised(
                    &mut tasks,
                    SupervisedTask::new(TaskKind::Connection, {
                        let cancellation = agent_cancellation.clone();
                        let task_tx = task_tx.clone();
                        async move {
                            run_connection_event_loop(
                                agent_connection,
                                config,
                                None,
                                limits,
                                task_tx,
                                cancellation,
                            )
                            .await
                        }
                    }),
                );
                let result = supervise_tasks(&mut tasks, &mut task_rx, &agent_cancellation).await;
                agent_cancellation.cancel();
                drop(task_tx);
                task_rx.close();
                while task_rx.try_recv().is_ok() {}
                shutdown_tasks(&mut tasks).await;
                result
            });

            Self {
                hub_ref,
                cancellation,
                agent,
                hub,
            }
        }

        async fn stop(self) {
            self.cancellation.cancel();
            tokio::time::timeout(Duration::from_secs(2), self.agent)
                .await
                .expect("agent supervisor did not stop")
                .expect("agent supervisor panicked")
                .expect("agent supervisor failed");
            self.hub.abort();
            let _ = self.hub.await;
        }
    }

    #[test]
    fn operation_polling_requires_explicit_opt_in() {
        assert!(!operation_polling_enabled(None));
        assert!(!operation_polling_enabled(Some(&OperationsConfig::new())));
        assert!(!operation_polling_enabled(Some(
            &OperationsConfig::new().with_enabled(Some(false)),
        )));
        assert!(operation_polling_enabled(Some(
            &OperationsConfig::new().with_enabled(Some(true)),
        )));
    }

    #[tokio::test]
    async fn command_execution_slots_are_strictly_bounded() {
        let slots = command_slots();
        let mut permits = Vec::new();
        for _ in 0..nexigon_agent_protocol::MAX_CONCURRENT_COMMANDS {
            permits.push(slots.clone().try_acquire_owned().unwrap());
        }
        assert!(slots.clone().try_acquire_owned().is_err());
        permits.pop();
        assert!(slots.try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn endpoint_feature_limits_are_strictly_bounded_and_recover() {
        let limits = EndpointLimits::new(command_slots());

        let mut forwarding = Vec::new();
        for _ in 0..MAX_CONCURRENT_TCP_FORWARDINGS {
            forwarding.push(
                limits
                    .tcp_forwardings
                    .clone()
                    .try_acquire_owned()
                    .expect("forwarding slot within limit"),
            );
        }
        assert!(limits.tcp_forwardings.clone().try_acquire_owned().is_err());
        forwarding.pop();
        assert!(limits.tcp_forwardings.clone().try_acquire_owned().is_ok());

        #[cfg(target_os = "linux")]
        {
            let mut terminals = Vec::new();
            for _ in 0..MAX_CONCURRENT_TERMINALS {
                terminals.push(
                    limits
                        .terminals
                        .clone()
                        .try_acquire_owned()
                        .expect("terminal slot within limit"),
                );
            }
            assert!(limits.terminals.clone().try_acquire_owned().is_err());
            terminals.pop();
            assert!(limits.terminals.try_acquire_owned().is_ok());
        }
    }

    #[tokio::test]
    async fn live_forwarding_limit_is_enforced_and_recovers() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind forwarding target");
        let port = listener.local_addr().unwrap().port();
        let (accepted_tx, mut accepted_rx) = mpsc::channel(MAX_CONCURRENT_TCP_FORWARDINGS + 1);
        let accept_driver = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if accepted_tx.send(stream).await.is_err() {
                    break;
                }
            }
        });
        let mut agent = EndpointTestAgent::start().await;
        let endpoint = format!("forward/tcp/{port}");
        let mut channels = Vec::new();
        let mut local_streams = Vec::new();

        for _ in 0..MAX_CONCURRENT_TCP_FORWARDINGS {
            channels.push(
                tokio::time::timeout(
                    Duration::from_secs(2),
                    agent.hub_ref.open(endpoint.as_bytes()),
                )
                .await
                .expect("forwarding request timed out")
                .expect("forwarding request below limit was rejected"),
            );
            local_streams.push(
                tokio::time::timeout(Duration::from_secs(2), accepted_rx.recv())
                    .await
                    .expect("forwarding target was not connected")
                    .expect("forwarding accept driver stopped"),
            );
        }

        let over_limit = tokio::time::timeout(
            Duration::from_secs(2),
            agent.hub_ref.open(endpoint.as_bytes()),
        )
        .await
        .expect("over-limit request timed out");
        assert!(over_limit.is_err(), "forwarding limit was not enforced");

        drop(channels.pop());
        drop(local_streams.pop());
        let replacement = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match agent.hub_ref.open(endpoint.as_bytes()).await {
                    Ok(channel) => break channel,
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("forwarding permit was not recovered");
        channels.push(replacement);
        local_streams.push(
            accepted_rx
                .recv()
                .await
                .expect("replacement forwarding was not connected"),
        );

        drop(channels);
        drop(local_streams);
        agent.stop().await;
        accept_driver.abort();
        let _ = accept_driver.await;
    }

    #[tokio::test]
    async fn malformed_endpoints_are_rejected_without_stopping_the_connection() {
        let mut agent = EndpointTestAgent::start().await;

        let invalid_utf8 =
            tokio::time::timeout(Duration::from_secs(2), agent.hub_ref.open(&[0xff, 0xfe]))
                .await
                .expect("invalid UTF-8 request timed out");
        assert!(invalid_utf8.is_err());

        let invalid_port = tokio::time::timeout(
            Duration::from_secs(2),
            agent.hub_ref.open(b"forward/tcp/not-a-port"),
        )
        .await
        .expect("invalid port request timed out");
        assert!(invalid_port.is_err());

        let follow_up = tokio::time::timeout(
            Duration::from_secs(2),
            agent.hub_ref.open(b"still-connected"),
        )
        .await
        .expect("follow-up request timed out");
        assert!(follow_up.is_err());
        assert!(!agent.agent.is_finished());

        agent.stop().await;
    }

    #[tokio::test]
    async fn unavailable_forwarding_port_is_rejected_without_panicking() {
        // Keep the port bound without listening so connection attempts are rejected and no
        // parallel test can claim the fixture between port selection and the assertion.
        let unavailable_socket = tokio::net::TcpSocket::new_v4().expect("create TCP socket");
        unavailable_socket
            .bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .expect("reserve unavailable local TCP port");
        let port = unavailable_socket.local_addr().unwrap().port();

        let mut agent = EndpointTestAgent::start().await;
        let endpoint = format!("forward/tcp/{port}");
        let unavailable = tokio::time::timeout(
            Duration::from_secs(2),
            agent.hub_ref.open(endpoint.as_bytes()),
        )
        .await
        .expect("unavailable forwarding request timed out");
        assert!(unavailable.is_err());
        assert!(!agent.agent.is_finished());

        let follow_up = tokio::time::timeout(
            Duration::from_secs(2),
            agent.hub_ref.open(b"still-connected"),
        )
        .await
        .expect("follow-up request timed out");
        assert!(follow_up.is_err());
        agent.stop().await;
    }

    #[tokio::test]
    async fn shutdown_closes_an_active_forwarding_relay() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local forwarding target");
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let mut agent = EndpointTestAgent::start().await;
        let endpoint = format!("forward/tcp/{port}");
        let mut channel = tokio::time::timeout(
            Duration::from_secs(2),
            agent.hub_ref.open(endpoint.as_bytes()),
        )
        .await
        .expect("forwarding request timed out")
        .expect("forwarding request rejected");
        let mut local = tokio::time::timeout(Duration::from_secs(2), accept)
            .await
            .expect("forwarding target was not connected")
            .expect("forwarding target accept task panicked");

        channel.write_all(b"probe").await.unwrap();
        channel.flush().await.unwrap();
        let mut probe = [0u8; 5];
        tokio::time::timeout(Duration::from_secs(2), local.read_exact(&mut probe))
            .await
            .expect("forwarded data was not delivered")
            .expect("reading forwarded data failed");
        assert_eq!(&probe, b"probe");

        agent.stop().await;
        let mut byte = [0u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(1), local.read(&mut byte))
            .await
            .expect("forwarding socket was left open");
        assert!(
            matches!(closed, Ok(0))
                || matches!(closed, Err(ref error) if error.kind() == std::io::ErrorKind::ConnectionReset),
            "forwarding socket remained usable: {closed:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_awaits_every_supervised_child() {
        struct ActiveGuard(Arc<AtomicUsize>);

        impl Drop for ActiveGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let active = Arc::new(AtomicUsize::new(0));
        let cancellation = CancellationToken::new();
        let (ready_tx, mut ready_rx) = mpsc::channel(2);
        let mut tasks = JoinSet::new();
        for kind in [TaskKind::SystemInfo, TaskKind::TcpForward] {
            let active = active.clone();
            let cancellation = cancellation.clone();
            let ready_tx = ready_tx.clone();
            spawn_supervised(
                &mut tasks,
                SupervisedTask::new(kind, async move {
                    active.fetch_add(1, Ordering::SeqCst);
                    let _guard = ActiveGuard(active);
                    ready_tx
                        .send(())
                        .await
                        .context("reporting task readiness")?;
                    cancellation.cancelled().await;
                    Ok(())
                }),
            );
        }
        drop(ready_tx);
        ready_rx.recv().await.expect("first task started");
        ready_rx.recv().await.expect("second task started");
        assert_eq!(active.load(Ordering::SeqCst), 2);

        cancellation.cancel();
        shutdown_tasks(&mut tasks).await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn connection_task_errors_are_not_hidden_by_cancellation() {
        let cancellation = CancellationToken::new();
        let (_task_tx, mut task_rx) = mpsc::channel(1);
        let mut tasks = JoinSet::new();
        spawn_supervised(
            &mut tasks,
            SupervisedTask::new(TaskKind::Connection, async {
                anyhow::bail!("sentinel connection failure")
            }),
        );

        let error = supervise_tasks(&mut tasks, &mut task_rx, &cancellation)
            .await
            .expect_err("connection error was swallowed");
        assert!(error.to_string().contains("sentinel connection failure"));
        assert!(!cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn shutdown_cancels_a_stalled_startup_future() {
        struct StartupGuard(Arc<AtomicBool>);

        impl Drop for StartupGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task_dropped = dropped.clone();
        let task = tokio::spawn(async move {
            run_until_cancelled(&task_cancellation, async move {
                let _guard = StartupGuard(task_dropped);
                let _ = started_tx.send(());
                std::future::pending::<()>().await;
            })
            .await
        });
        started_rx.await.expect("startup future did not begin");

        cancellation.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("stalled startup ignored cancellation")
                .expect("startup task panicked"),
            None
        );
        assert!(dropped.load(Ordering::SeqCst));
    }

    struct MockReporter {
        attempts: VecDeque<ReportAttempt>,
        calls: usize,
    }

    impl OperationReporter for MockReporter {
        async fn report(
            &mut self,
            _: DeviceId,
            _: DeviceOperationId,
            _: u32,
            _: DeviceOperationWorkClaimId,
            _: DeviceOperationStepReport,
        ) -> ReportAttempt {
            self.calls += 1;
            self.attempts.pop_front().expect("one result per call")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn completed_operation_reports_retry_transport_failures() {
        let mut reporter = MockReporter {
            attempts: VecDeque::from([
                ReportAttempt::TransportFailed("offline".to_owned()),
                ReportAttempt::TransportFailed("still offline".to_owned()),
                ReportAttempt::Acknowledged,
            ]),
            calls: 0,
        };
        let acknowledged = report_operation_step_with_retry(
            &mut reporter,
            &DeviceId::generate(),
            &DeviceOperationId::generate(),
            0,
            &DeviceOperationWorkClaimId::generate(),
            &DeviceOperationStepReport::new(DeviceOperationStepReportStatus::Succeeded),
            None,
        )
        .await;

        assert!(acknowledged);
        assert_eq!(reporter.calls, 3);
    }
}
