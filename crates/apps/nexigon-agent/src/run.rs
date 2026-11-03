//! Long-running agent entry point.
//!
//! Used both by the binary's `run` subcommand and by callers that embed the
//! agent in-process (notably the test helper that hosts multiple agents
//! inside a single hub-side process).
//!
//! The agent talks to Nexigon Hub in sessions, one per connection. A session
//! serves the channels that Nexigon Hub opens, such as terminals, TCP
//! forwarding, and command handlers, and ends when its connection ends. The
//! reconciliation loops, the operation poller, the system-info publisher, and
//! the local API run for the lifetime of the agent. They use the connection of
//! the current session through a [`HubLink`], so that a lost connection does
//! not interrupt them.

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
use nexigon_api::types::devices::SetDevicePropertyAction;
use nexigon_api::types::properties::DeviceCommandDescriptor;
use nexigon_client::WebsocketConnection;
use nexigon_ids::ids::DeviceId;
use nexigon_multiplex::ConnectionEvent;
use nexigon_multiplex::ConnectionRef;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::config::Config;
use crate::device_api::connect_device_executor;
use crate::handlers;
use crate::handlers::CommandRegistry;
use crate::hub_link::HubLink;
use crate::hub_link::LinkController;
use crate::hub_link::hub_link;
use crate::operation_ledger::OperationLedger;
use crate::operations;
use crate::operations::OperationPoller;
use crate::reconnect::Reconnector;
use crate::reconnect::reconnect_settings;
use crate::system_info::get_system_info;
use crate::system_updates::SystemUpdates;

#[cfg(target_os = "linux")]
const MAX_CONCURRENT_TERMINALS: usize = 4;
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TASK_SHUTDOWN_GRACE: Duration = Duration::from_secs(8);
const SYSTEM_INFO_INTERVAL: Duration = Duration::from_secs(30 * 60);

type BoxedTask = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskKind {
    Connection,
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
    #[cfg(target_os = "linux")]
    terminals: Arc<Semaphore>,
    commands: Arc<Semaphore>,
    tcp_connect_timeout: Duration,
}

impl EndpointLimits {
    fn new(commands: Arc<Semaphore>) -> Self {
        Self {
            #[cfg(target_os = "linux")]
            terminals: Arc::new(Semaphore::new(MAX_CONCURRENT_TERMINALS)),
            commands,
            tcp_connect_timeout: TCP_CONNECT_TIMEOUT,
        }
    }
}

pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
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

/// Run a Nexigon agent until `shutdown` resolves.
///
/// The agent reconnects to Nexigon Hub whenever its connection is lost. When
/// `shutdown` resolves, the agent closes its connection and this function
/// returns `Ok(())`.
///
/// The caller must ensure a Rustls crypto provider has been installed
/// before invoking `run`; see [`crate::install_crypto_provider`].
pub async fn run(
    config_path: &Path,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    crate::run_agent(config_path.to_path_buf(), shutdown, None).await
}

/// Options of an agent running in-process on an established connection.
#[derive(Default)]
pub struct RunOptions {
    /// Fulfilled with the agent's [`DeviceId`] as soon as the agent has
    /// registered with the hub. In-process hosts use it to expose the device
    /// id, for example to a test harness, before the agent's long-running loop
    /// returns.
    pub ready: Option<oneshot::Sender<DeviceId>>,
    /// System updates replacing the integration in the agent configuration.
    pub system_updates: Option<SystemUpdates>,
}

/// Run the agent on an already-established connection.
///
/// The agent stops when the connection ends or `shutdown` resolves.
pub async fn run_with_connection(
    config: Arc<Config>,
    config_dir: &Path,
    connection: WebsocketConnection,
    shutdown: impl Future<Output = ()> + Send + 'static,
    options: RunOptions,
) -> anyhow::Result<()> {
    run_sessions(
        config,
        config_dir,
        Connections::Single(Some(connection)),
        shutdown,
        options,
    )
    .await
}

/// Connections on which the agent runs its sessions.
// The agent holds a single value, so its size does not matter.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Connections {
    /// One established connection, after which the agent stops.
    Single(Option<WebsocketConnection>),
    /// Connections established, and re-established after losing them, by a
    /// [`Reconnector`].
    Reconnecting(Reconnector),
}

impl Connections {
    /// Record that a session has started on the last connection.
    fn session_started(&mut self) {
        if let Self::Reconnecting(reconnector) = self {
            reconnector.session_started();
        }
    }

    /// Next connection, or `None` if the agent stops.
    async fn next(
        &mut self,
        link: &LinkController,
        shutdown: &CancellationToken,
    ) -> anyhow::Result<Option<WebsocketConnection>> {
        match self {
            Self::Single(connection) => Ok(connection.take()),
            Self::Reconnecting(reconnector) => reconnector.connect(link, shutdown).await,
        }
    }
}

/// Run the agent with one session per connection until the connections are
/// exhausted or `shutdown` resolves.
///
/// With a single connection, the result is the result of its session.
pub(crate) async fn run_sessions(
    config: Arc<Config>,
    config_dir: &Path,
    mut connections: Connections,
    shutdown: impl Future<Output = ()> + Send + 'static,
    options: RunOptions,
) -> anyhow::Result<()> {
    let RunOptions {
        mut ready,
        system_updates,
    } = options;

    // Without valid settings, the agent runs without system updates.
    let mut system_updates = system_updates.or_else(|| {
        crate::system_updates::settings(&config).unwrap_or_else(|error| {
            error!(
                error = format!("{error:#}"),
                "invalid system update integration configuration, system updates are disabled"
            );
            None
        })
    });
    let mut app_updates = crate::rugix_apps::settings(&config);
    let check_requested = Arc::new(Notify::new());
    let app_check_requested = Arc::new(Notify::new());
    let command_registry = command_registry(
        &config,
        system_updates.is_some().then(|| check_requested.clone()),
        app_updates.is_some().then(|| app_check_requested.clone()),
    );
    let command_slots = command_slots();
    let endpoints = SessionEndpoints {
        config: config.clone(),
        command_registry: command_registry.clone(),
        command_slots: command_slots.clone(),
        supervisor_queue_capacity: crate::config::multiplex_settings(&config)
            .context("invalid multiplex configuration")?
            .supervisor_queue_capacity(),
        connect_timeout: reconnect_settings(&config)
            .context("invalid connection configuration")?
            .connect_timeout(),
    };
    let mut operation_ledger = if operations::polling_enabled(config.operations.as_ref()) {
        Some(
            OperationLedger::load(&crate::data_path(&config, config_dir))
                .await
                .context("cannot open operation execution ledger")?,
        )
    } else {
        None
    };

    // `shutdown_requested` signals a requested shutdown, while `stopping`
    // also signals that the connections are exhausted. The system update loop
    // tells them apart when it finishes an installation.
    let shutdown_requested = CancellationToken::new();
    let shutdown_watcher = tokio::spawn({
        let shutdown_requested = shutdown_requested.clone();
        async move {
            shutdown.await;
            shutdown_requested.cancel();
        }
    });
    let stopping = shutdown_requested.child_token();
    let (link_controller, link) = hub_link();
    let mut agent_tasks = JoinSet::new();
    #[cfg(unix)]
    if let Some(local_api) = local_api_task(&config, link.clone(), stopping.clone()) {
        spawn_supervised(&mut agent_tasks, local_api);
    }

    // The system update and app loops finish a change in progress when the
    // agent stops, so they run outside the task group and its shutdown deadline.
    let mut integrations: Vec<(&str, JoinHandle<anyhow::Result<()>>)> = Vec::new();
    let device_lock = Arc::new(Mutex::new(()));
    let mut device_id: Option<DeviceId> = None;
    let mut manifest_published = false;
    let mut session_result = Ok(());
    let result = loop {
        // Connecting and identifying the device are boxed because their futures
        // are large, which would overflow the stack of unoptimized builds.
        let connection =
            match Box::pin(connections.next(&link_controller, &shutdown_requested)).await {
                Ok(Some(connection)) => connection,
                Ok(None) if matches!(connections, Connections::Single(_)) => break session_result,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            };
        let mut session =
            match Box::pin(Session::start(connection, &endpoints, &shutdown_requested)).await {
                Ok(Some(session)) => session,
                Ok(None) => continue,
                Err(error) => {
                    warn!(
                        error = format!("{error:#}"),
                        "failed to start a session with Nexigon Hub"
                    );
                    link_controller.attempt_failed(format!("{error:#}"));
                    session_result = Err(error);
                    continue;
                }
            };
        connections.session_started();
        if let Some(known) = &device_id
            && *known != session.device_id
        {
            let error = anyhow::anyhow!(
                "the device ID changed from {known} to {}; restart the agent",
                session.device_id
            );
            session.close().await;
            break Err(error);
        }
        link_controller.connected(session.device_id.clone(), session.connection_ref.clone());
        if device_id.is_none() {
            let session_device_id = session.device_id.clone();
            device_id = Some(session_device_id.clone());
            if let Some(ready) = ready.take() {
                let _ = ready.send(session_device_id.clone());
            }
            let system_info_enabled = config
                .telemetry
                .as_ref()
                .and_then(|t| t.system_info)
                .unwrap_or(true);
            if system_info_enabled {
                spawn_supervised(
                    &mut agent_tasks,
                    SupervisedTask::new(
                        TaskKind::SystemInfo,
                        log_failure(
                            TaskKind::SystemInfo,
                            publish_system_info(
                                config.clone(),
                                link.clone(),
                                session_device_id.clone(),
                                stopping.clone(),
                            ),
                        ),
                    ),
                );
            }
            if let Some(ledger) = operation_ledger.take() {
                let poller = OperationPoller {
                    link: link.clone(),
                    device_id: session_device_id.clone(),
                    command_registry: command_registry.clone(),
                    command_slots: command_slots.clone(),
                    ledger,
                    poll_interval: operations::poll_interval(&config),
                };
                spawn_supervised(
                    &mut agent_tasks,
                    SupervisedTask::new(
                        TaskKind::Operations,
                        log_failure(
                            TaskKind::Operations,
                            operations::run(poller, stopping.clone()),
                        ),
                    ),
                );
            }
            if let Some(settings) = system_updates.take() {
                integrations.push((
                    "system update integration",
                    tokio::spawn(crate::system_updates::run(
                        settings,
                        link.clone(),
                        session_device_id.clone(),
                        device_lock.clone(),
                        check_requested.clone(),
                        stopping.clone(),
                        shutdown_requested.clone(),
                    )),
                ));
            }
            if let Some(settings) = app_updates.take() {
                integrations.push((
                    "app integration",
                    tokio::spawn(crate::rugix_apps::run(
                        settings,
                        link.clone(),
                        session_device_id,
                        device_lock.clone(),
                        app_check_requested.clone(),
                        stopping.clone(),
                    )),
                ));
            }
        }
        if let Some(registry) = &command_registry
            && !manifest_published
        {
            let publish = publish_command_manifest(&link, &session.device_id, registry);
            manifest_published = run_until_cancelled(&shutdown_requested, publish)
                .await
                .unwrap_or(false);
        }
        session_result = session.run().await;
        match &session_result {
            Ok(()) if shutdown_requested.is_cancelled() => {}
            Ok(()) => info!("connection to Nexigon Hub ended"),
            Err(error) => warn!(
                error = format!("{error:#}"),
                "connection to Nexigon Hub failed"
            ),
        }
        link_controller.disconnected(session_result.as_ref().err().map(|e| format!("{e:#}")));
        session.close().await;
    };

    stopping.cancel();
    shutdown_tasks(&mut agent_tasks).await;
    for (name, task) in integrations {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => error!(?error, "{name} failed"),
            Err(error) => error!(?error, "{name} panicked"),
        }
    }
    shutdown_watcher.abort();
    result
}

/// Configuration and limits shared by all sessions.
struct SessionEndpoints {
    config: Arc<Config>,
    command_registry: Option<Arc<CommandRegistry>>,
    command_slots: Arc<Semaphore>,
    supervisor_queue_capacity: usize,
    /// Time allowed for identifying the device after connecting.
    connect_timeout: Duration,
}

/// Tasks serving the channels of one connection.
struct Session {
    device_id: DeviceId,
    connection_ref: ConnectionRef,
    tasks: SessionTasks,
}

impl Session {
    /// Start serving `connection` and identify the device.
    ///
    /// Returns `None` if `shutdown` is cancelled before the device is
    /// identified.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn start(
        connection: WebsocketConnection,
        endpoints: &SessionEndpoints,
        shutdown: &CancellationToken,
    ) -> anyhow::Result<Option<Self>> {
        let mut connection_ref = connection.make_ref();
        let tasks = SessionTasks::spawn(connection, endpoints, shutdown.child_token());
        let identify = async {
            let mut executor = connect_device_executor(&mut connection_ref)
                .await
                .context("cannot open executor channel")?;
            let actor = executor
                .execute(GetActorAction::new())
                .await
                .context("cannot execute GetActor")?
                .map_err(|e| anyhow::anyhow!("GetActor failed: {}", e.message))?
                .actor;
            let Actor::Device(actor) = actor else {
                bail!("received unexpected actor type");
            };
            info!(device_id = %actor.device_id);
            Ok(actor.device_id)
        };
        let identify = async {
            tokio::time::timeout(endpoints.connect_timeout, identify)
                .await
                .context("identifying the device timed out")?
        };
        let identified = run_until_cancelled(&tasks.cancellation, identify).await;
        let device_id = match identified {
            Some(Ok(identified)) => identified,
            Some(Err(error)) => {
                tasks.close().await;
                return Err(error);
            }
            None => {
                tasks.close().await;
                return Ok(None);
            }
        };
        Ok(Some(Self {
            device_id,
            connection_ref,
            tasks,
        }))
    }

    /// Serve the connection until it ends or the agent shuts down.
    async fn run(&mut self) -> anyhow::Result<()> {
        self.tasks.supervise().await
    }

    async fn close(self) {
        self.tasks.close().await;
    }
}

/// Supervised tasks of a session: the connection's event loop and the tasks
/// serving its channels.
struct SessionTasks {
    tasks: JoinSet<TaskCompletion>,
    task_tx: mpsc::Sender<SupervisedTask>,
    task_rx: mpsc::Receiver<SupervisedTask>,
    cancellation: CancellationToken,
}

impl SessionTasks {
    fn spawn(
        connection: WebsocketConnection,
        endpoints: &SessionEndpoints,
        cancellation: CancellationToken,
    ) -> Self {
        let (task_tx, task_rx) = mpsc::channel(endpoints.supervisor_queue_capacity);
        let mut tasks = JoinSet::new();
        spawn_supervised(
            &mut tasks,
            SupervisedTask::new(
                TaskKind::Connection,
                run_connection_event_loop(
                    connection,
                    endpoints.config.clone(),
                    endpoints.command_registry.clone(),
                    EndpointLimits::new(endpoints.command_slots.clone()),
                    task_tx.clone(),
                    cancellation.clone(),
                ),
            ),
        );
        Self {
            tasks,
            task_tx,
            task_rx,
            cancellation,
        }
    }

    /// Supervise the tasks until the connection ends or the session is
    /// cancelled.
    async fn supervise(&mut self) -> anyhow::Result<()> {
        supervise_tasks(&mut self.tasks, &mut self.task_rx, &self.cancellation).await
    }

    /// Stop the tasks, waiting for them up to a grace period.
    async fn close(mut self) {
        self.cancellation.cancel();
        drop(self.task_tx);
        self.task_rx.close();
        while self.task_rx.try_recv().is_ok() {}
        shutdown_tasks(&mut self.tasks).await;
    }
}

/// Publish the manifest of the registered commands and return whether it has
/// been published.
async fn publish_command_manifest(
    link: &HubLink,
    device_id: &DeviceId,
    registry: &CommandRegistry,
) -> bool {
    let manifest = match serde_json::to_value(registry.manifest()) {
        Ok(manifest) => manifest,
        Err(error) => {
            warn!(?error, "cannot serialize command manifest");
            return false;
        }
    };
    let publish = SetDevicePropertyAction::new(
        device_id.clone(),
        "dev.nexigon.commands".to_owned(),
        manifest.into(),
    );
    match link.executor().execute(publish).await {
        Ok(Ok(_)) => true,
        Ok(Err(error)) => {
            warn!(message = %error.message, "command manifest rejected");
            false
        }
        Err(error) => {
            warn!(?error, "failed to publish command manifest");
            false
        }
    }
}

/// Publish the system information periodically until cancelled.
///
/// After a failed publication, the next one follows a reconnection.
async fn publish_system_info(
    config: Arc<Config>,
    link: HubLink,
    device_id: DeviceId,
    cancellation: CancellationToken,
) -> anyhow::Result<()> {
    let mut executor = link.executor();
    loop {
        let sessions = link.sessions();
        let system_info = get_system_info(&config);
        let value =
            serde_json::to_value(system_info).context("cannot serialize system information")?;
        let update = executor.execute(SetDevicePropertyAction::new(
            device_id.clone(),
            "dev.nexigon.system.info".to_owned(),
            value.into(),
        ));
        let update = tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            update = update => update,
        };
        let published = match update {
            Ok(Ok(_)) => true,
            Ok(Err(error)) => {
                warn!(message = %error.message, "system-info update rejected");
                false
            }
            Err(error) => {
                warn!(?error, "failed to publish system information");
                false
            }
        };
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(SYSTEM_INFO_INTERVAL) => {}
            () = link.connected_after(sessions), if !published => {}
        }
    }
}

/// Log a failure or panic of a long-running task when it happens instead of
/// when the agent stops.
async fn log_failure(
    kind: TaskKind,
    task: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    match AssertUnwindSafe(task).catch_unwind().await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => error!(
            task = kind.description(),
            ?error,
            "agent task failed and stays stopped until the agent restarts"
        ),
        Err(payload) => error!(
            task = kind.description(),
            panic = panic_message(payload),
            "agent task panicked and stays stopped until the agent restarts"
        ),
    }
    Ok(())
}

/// Load the command registry if commands are enabled, with the built-in
/// commands that request immediate checks of the enabled integrations.
fn command_registry(
    config: &Config,
    system_update_check: Option<Arc<Notify>>,
    app_check: Option<Arc<Notify>>,
) -> Option<Arc<CommandRegistry>> {
    let commands = config.commands.as_ref();
    if !commands.and_then(|h| h.enabled).unwrap_or(false) {
        return None;
    }
    let commands_dir = commands
        .and_then(|h| h.directory.as_deref())
        .unwrap_or(Path::new("/etc/nexigon/agent/commands"));
    let mut registry = load_command_registry(commands_dir);
    if let Some(check_requested) = system_update_check {
        registry.register_builtin(
            DeviceCommandDescriptor::new(crate::system_updates::CHECK_COMMAND)
                .with_description(Some("Request an immediate system update check.".into()))
                .with_category(Some("OTA".into())),
            Arc::new(move || check_requested.notify_one()),
        );
    }
    if let Some(check_requested) = app_check {
        registry.register_builtin(
            DeviceCommandDescriptor::new(crate::rugix_apps::CHECK_COMMAND)
                .with_description(Some("Request an immediate check of Rugix Apps.".into()))
                .with_category(Some("Rugix Apps".into())),
            Arc::new(move || check_requested.notify_one()),
        );
    }
    Some(Arc::new(registry))
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
        if port == 0 {
            request.reject(b"invalid TCP forwarding endpoint");
            return;
        }
        if !crate::config::tcp_forwarding_allowed(config, port) {
            request.reject(b"TCP forwarding port is not allowed");
            return;
        }
        let Ok(task_slot) = task_tx.clone().try_reserve_owned() else {
            request.reject(b"agent task queue is full");
            return;
        };
        task_slot.send(SupervisedTask::new(
            TaskKind::TcpConnect,
            connect_tcp_forwarding(
                request,
                port,
                limits.tcp_connect_timeout,
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
    connect_timeout: Duration,
    task_tx: mpsc::Sender<SupervisedTask>,
    cancellation: CancellationToken,
) -> anyhow::Result<()> {
    let address = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    let connect = tokio::time::timeout(connect_timeout, TcpStream::connect(address));
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

/// Load optional command support without making it an agent startup dependency.
fn load_command_registry(commands_dir: &Path) -> CommandRegistry {
    match CommandRegistry::load_external(commands_dir) {
        Ok(registry) => registry,
        Err(error) => {
            warn!(
                ?commands_dir,
                error = ?error,
                "failed to load command definitions, continuing without external commands"
            );
            CommandRegistry::default()
        }
    }
}

fn command_slots() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_CONCURRENT_COMMANDS))
}

#[cfg(unix)]
fn local_api_task(
    config: &Config,
    link: HubLink,
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
    Some(SupervisedTask::new(
        TaskKind::LocalApi,
        log_failure(TaskKind::LocalApi, async move {
            crate::local_api::serve(&local_api_config, link, cancellation.cancelled_owned()).await
        }),
    ))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use anyhow::Context;
    use bytes::Bytes;
    use futures::StreamExt;
    use futures::future::join_all;
    use nexigon_multiplex::Connection;
    use nexigon_multiplex::ConnectionEvent;
    use nexigon_multiplex::ConnectionRef;
    use nexigon_multiplex::OpenError;
    use nexigon_multiplex::transport::InMemory;
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;
    use tokio::task::JoinSet;
    use tokio_util::sync::CancellationToken;

    use super::EndpointLimits;
    #[cfg(target_os = "linux")]
    use super::MAX_CONCURRENT_TERMINALS;
    use super::SupervisedTask;
    use super::TCP_CONNECT_TIMEOUT;
    use super::TaskKind;
    use super::command_slots;
    use super::load_command_registry;
    use super::run_connection_event_loop;
    use super::run_until_cancelled;
    use super::shutdown_tasks;
    use super::spawn_supervised;
    use super::supervise_tasks;
    use crate::config::Config;

    /// A commands-directory failure leaves an empty registry for agent startup.
    #[test]
    fn command_registry_failures_do_not_escape_startup() {
        let directory = TempDir::new().unwrap();
        let not_a_directory = directory.path().join("commands");
        std::fs::write(&not_a_directory, b"not a directory").unwrap();

        let registry = load_command_registry(&not_a_directory);

        assert!(registry.manifest().commands.is_empty());
    }

    struct EndpointTestAgent {
        hub_ref: ConnectionRef,
        cancellation: CancellationToken,
        agent: JoinHandle<anyhow::Result<()>>,
        hub: JoinHandle<()>,
    }

    impl EndpointTestAgent {
        async fn start() -> Self {
            Self::with_config(Config::new(PathBuf::from("unused-fingerprint"))).await
        }

        /// Start an agent with one explicitly allowed TCP destination.
        async fn forwarding(port: u16) -> Self {
            Self::forwarding_with_connect_timeout(port, TCP_CONNECT_TIMEOUT).await
        }

        /// Start an agent with one TCP destination and a test-controlled connect timeout.
        async fn forwarding_with_connect_timeout(port: u16, connect_timeout: Duration) -> Self {
            let config = toml::from_str(&format!(
                "fingerprint-script = 'unused'\n[forwarding]\nenabled = true\nallowed-tcp-ports = [{port}]"
            ))
            .unwrap();
            Self::with_config_and_connect_timeout(config, connect_timeout).await
        }

        /// Run real channel handling and task supervision with the supplied policy.
        async fn with_config(config: Config) -> Self {
            Self::with_config_and_connect_timeout(config, TCP_CONNECT_TIMEOUT).await
        }

        async fn with_config_and_connect_timeout(
            config: Config,
            connect_timeout: Duration,
        ) -> Self {
            let (hub_transport, agent_transport) = InMemory::<Bytes, Bytes>::new_buffered(64);
            let multiplex_settings = crate::config::multiplex_settings(&config).unwrap();
            let hub_limits = crate::hub_connection_limits(multiplex_settings);
            let mut hub_connection = Connection::with_limits(hub_transport, hub_limits);
            let agent_connection = Connection::with_limits(
                agent_transport,
                crate::hub_connection_limits(multiplex_settings),
            );
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
                let config = Arc::new(config);
                let mut limits = EndpointLimits::new(command_slots());
                limits.tcp_connect_timeout = connect_timeout;
                let supervisor_queue_capacity = crate::config::multiplex_settings(&config)
                    .unwrap()
                    .supervisor_queue_capacity();
                let (task_tx, mut task_rx) = mpsc::channel(supervisor_queue_capacity);
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn endpoint_feature_limits_are_strictly_bounded_and_recover() {
        let limits = EndpointLimits::new(command_slots());

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

    /// A browser-sized burst can establish far more than the legacy 24 connections.
    #[tokio::test]
    async fn live_forwarding_accepts_large_browser_burst() {
        const CONCURRENT_FORWARDINGS: usize = 128;

        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind forwarding target");
        let port = listener.local_addr().unwrap().port();
        let (accepted_tx, mut accepted_rx) = mpsc::channel(CONCURRENT_FORWARDINGS);
        let accept_driver = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if accepted_tx.send(stream).await.is_err() {
                    break;
                }
            }
        });
        let agent = EndpointTestAgent::forwarding(port).await;
        let endpoint = format!("forward/tcp/{port}");
        let mut channels = Vec::new();
        let mut local_streams = Vec::new();

        let channel_requests = (0..CONCURRENT_FORWARDINGS).map(|_| {
            let mut hub_ref = agent.hub_ref.clone();
            let endpoint = endpoint.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(2), hub_ref.open(endpoint.as_bytes()))
                    .await
                    .expect("forwarding request timed out")
                    .expect("forwarding request was rejected")
            }
        });
        channels.extend(join_all(channel_requests).await);
        for _ in 0..CONCURRENT_FORWARDINGS {
            local_streams.push(
                tokio::time::timeout(Duration::from_secs(2), accepted_rx.recv())
                    .await
                    .expect("forwarding target was not connected")
                    .expect("forwarding accept driver stopped"),
            );
        }

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

    /// Missing, disabled, or non-matching forwarding policies never reach the target.
    #[tokio::test]
    async fn forwarding_policy_rejects_before_connecting() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let policies = [
            String::new(),
            "[forwarding]".to_owned(),
            format!("[forwarding]\nallowed-tcp-ports = [{port}]"),
            "[forwarding]\nallow-all-ports = true".to_owned(),
            format!("[forwarding]\nenabled = false\nallowed-tcp-ports = [{port}]"),
            "[forwarding]\nenabled = false\nallow-all-ports = true".to_owned(),
            "[forwarding]\nenabled = true".to_owned(),
            "[forwarding]\nenabled = true\nallowed-tcp-ports = []".to_owned(),
            format!(
                "[forwarding]\nenabled = true\nallowed-tcp-ports = [{}]",
                if port == 1 { 2 } else { 1 }
            ),
        ];
        for policy in policies {
            let config =
                toml::from_str(&format!("fingerprint-script = 'unused'\n{policy}")).unwrap();
            let mut agent = EndpointTestAgent::with_config(config).await;
            let endpoint = format!("forward/tcp/{port}");
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                agent.hub_ref.open(endpoint.as_bytes()),
            )
            .await
            .expect("denied forwarding request timed out");
            assert!(
                matches!(result, Err(OpenError::Rejected(ref rejection)) if rejection.reason() == b"TCP forwarding port is not allowed"),
                "policy: {policy}"
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err(),
                "denied request reached the target: {policy}"
            );
            assert!(!agent.agent.is_finished());
            agent.stop().await;
        }
    }

    /// Exports authorize raw TCP independently; enabled policies can grant listed or all
    /// ports.
    #[tokio::test]
    async fn http_exports_allow_forwarding_independently_of_additional_ports() {
        let exported = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let exported_port = exported.local_addr().unwrap().port();
        let additional = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let additional_port = additional.local_addr().unwrap().port();
        let unlisted = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let unlisted_port = unlisted.local_addr().unwrap().port();
        let policies = [
            (String::new(), false, false),
            (
                format!("[forwarding]\nallowed-tcp-ports = [{additional_port}]"),
                false,
                false,
            ),
            (
                format!("[forwarding]\nenabled = false\nallowed-tcp-ports = [{additional_port}]"),
                false,
                false,
            ),
            (
                "[forwarding]\nenabled = true\nallowed-tcp-ports = []".to_owned(),
                false,
                false,
            ),
            (
                format!("[forwarding]\nenabled = true\nallowed-tcp-ports = [{additional_port}]"),
                true,
                false,
            ),
            (
                "[forwarding]\nenabled = true\nallow-all-ports = true".to_owned(),
                true,
                true,
            ),
        ];
        for (policy, additional_allowed, unlisted_allowed) in policies {
            let config = toml::from_str(&format!(
                "fingerprint-script = 'unused'\n{policy}\n[[exports]]\nprotocol = 'http'\nname = 'Web UI'\nport = {exported_port}\npath = '/ui'"
            )).unwrap();
            let mut agent = EndpointTestAgent::with_config(config).await;
            let endpoint = format!("forward/tcp/{exported_port}");
            let mut channel = tokio::time::timeout(
                Duration::from_secs(2),
                agent.hub_ref.open(endpoint.as_bytes()),
            )
            .await
            .expect("export request timed out")
            .expect("export was rejected");
            let (mut target, _) = tokio::time::timeout(Duration::from_secs(2), exported.accept())
                .await
                .expect("export target connection timed out")
                .unwrap();
            channel.write_all(b"raw TCP").await.unwrap();
            channel.flush().await.unwrap();
            let mut data = [0; 7];
            tokio::time::timeout(Duration::from_secs(2), target.read_exact(&mut data))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&data, b"raw TCP");

            for (port, listener, allowed) in [
                (additional_port, &additional, additional_allowed),
                (unlisted_port, &unlisted, unlisted_allowed),
            ] {
                let endpoint = format!("forward/tcp/{port}");
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    agent.hub_ref.open(endpoint.as_bytes()),
                )
                .await
                .expect("forwarding request timed out");
                if allowed {
                    let _channel = result.expect("additional allowed port was rejected");
                    let _target = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                        .await
                        .expect("additional target connection timed out")
                        .unwrap();
                } else {
                    assert!(
                        matches!(result, Err(OpenError::Rejected(ref rejection)) if rejection.reason() == b"TCP forwarding port is not allowed")
                    );
                    assert!(
                        tokio::time::timeout(Duration::from_millis(50), listener.accept())
                            .await
                            .is_err()
                    );
                }
            }
            agent.stop().await;
        }
    }

    /// A bound but non-listening socket is refused immediately on some operating systems
    /// and remains pending on others. Both outcomes must reject without a port race.
    #[tokio::test]
    async fn unavailable_forwarding_port_is_rejected_without_panicking() {
        let target = tokio::net::TcpSocket::new_v4().unwrap();
        target.bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        let port = target.local_addr().unwrap().port();
        let mut agent =
            EndpointTestAgent::forwarding_with_connect_timeout(port, Duration::from_millis(100))
                .await;
        let endpoint = format!("forward/tcp/{port}");
        let unavailable = tokio::time::timeout(
            Duration::from_secs(2),
            agent.hub_ref.open(endpoint.as_bytes()),
        )
        .await
        .expect("unavailable forwarding request timed out");
        assert!(matches!(
            unavailable,
            Err(OpenError::Rejected(ref rejection))
                if matches!(
                    rejection.reason(),
                    b"local TCP forwarding target is unavailable"
                        | b"local TCP forwarding target timed out"
                )
        ));
        assert!(!agent.agent.is_finished());
        agent.stop().await;
    }

    /// Port zero and malformed or out-of-range endpoints cannot bypass an enabled policy.
    #[tokio::test]
    async fn invalid_forwarding_ports_are_rejected() {
        let mut agent = EndpointTestAgent::forwarding(80).await;
        for port in ["0", "65536", "-1", "*", "80/other", "127.0.0.1:80"] {
            let endpoint = format!("forward/tcp/{port}");
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                agent.hub_ref.open(endpoint.as_bytes()),
            )
            .await
            .expect("invalid request timed out");
            assert!(
                matches!(result, Err(OpenError::Rejected(ref rejection)) if rejection.reason() == b"invalid TCP forwarding endpoint")
            );
        }
        agent.stop().await;
    }

    /// Stopping the agent closes an allowed relay after it has transferred data.
    #[tokio::test]
    async fn shutdown_closes_an_active_forwarding_relay() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local forwarding target");
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let mut agent = EndpointTestAgent::forwarding(port).await;
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
}
