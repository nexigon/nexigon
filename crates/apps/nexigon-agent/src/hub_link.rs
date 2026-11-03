//! Connection to Nexigon Hub as seen by the agent's long-running components.
//!
//! The agent talks to Nexigon Hub in sessions. A session uses one connection
//! and ends when the connection is lost. Components that outlive a session,
//! such as the reconciliation loops, the operation poller, and the local API,
//! access Nexigon Hub through a [`HubLink`]. Its [`HubLink::executor`] runs
//! actions on the connection of the current session and fails while the agent
//! is disconnected. The agent publishes the state of the connection through
//! the [`LinkController`] returned by [`hub_link`].

use std::io;

use jiff::Timestamp;
use nexigon_agent_api::types::status::AgentStatus;
use nexigon_agent_api::types::status::HubConnected;
use nexigon_agent_api::types::status::HubConnectionStatus;
use nexigon_agent_api::types::status::HubDisconnected;
use nexigon_api::Action;
use nexigon_api::types::errors::ActionError;
use nexigon_client::ClientExecutor;
use nexigon_client::Execute;
use nexigon_ids::ids::DeviceId;
use nexigon_multiplex::ConnectionRef;
use nexigon_rpc::ExecuteError;
use tokio::sync::watch;

use crate::device_api::DeviceExecutor;

/// Executor for the Device API on the connection of the current session.
pub(crate) type HubExecutor = DeviceExecutor<LinkExecutor>;

/// Create a link in the disconnected state and its controller.
pub(crate) fn hub_link() -> (LinkController, HubLink) {
    let (sender, receiver) = watch::channel(LinkState {
        device_id: None,
        sessions: 0,
        connection: LinkConnection::Disconnected(Disconnected::since_now(None)),
    });
    (LinkController { sender }, HubLink { state: receiver })
}

/// Access to Nexigon Hub through the connection of the current session.
#[derive(Clone)]
pub(crate) struct HubLink {
    state: watch::Receiver<LinkState>,
}

impl HubLink {
    /// Executor for the Device API that follows the agent across sessions.
    pub(crate) fn executor(&self) -> HubExecutor {
        DeviceExecutor::new(LinkExecutor {
            link: self.clone(),
            channel: None,
        })
    }

    /// Connection of the current session, if the agent is connected.
    #[cfg_attr(not(unix), expect(dead_code, reason = "used by the Unix local API"))]
    pub(crate) fn connection(&self) -> Option<ConnectionRef> {
        self.current().map(|(_, connection)| connection)
    }

    /// Number of sessions that have started so far.
    pub(crate) fn sessions(&self) -> u64 {
        self.state.borrow().sessions
    }

    /// Wait until a session after the first `sessions` sessions has started.
    ///
    /// Never completes once the agent has stopped.
    pub(crate) async fn connected_after(&self, sessions: u64) {
        let mut state = self.state.clone();
        let connected = state
            .wait_for(|state| {
                state.sessions > sessions
                    && matches!(state.connection, LinkConnection::Connected { .. })
            })
            .await
            .is_ok();
        if !connected {
            std::future::pending::<()>().await;
        }
    }

    /// Wait until the agent is connected.
    ///
    /// Never completes once the agent has stopped.
    pub(crate) async fn connected(&self) {
        self.connected_after(0).await;
    }

    /// Status of the agent for the local API.
    #[cfg_attr(not(unix), expect(dead_code, reason = "used by the Unix local API"))]
    pub(crate) fn status(&self) -> AgentStatus {
        let state = self.state.borrow();
        let connection = match &state.connection {
            LinkConnection::Connected { since, .. } => {
                HubConnectionStatus::Connected(HubConnected::new((*since).into()))
            }
            LinkConnection::Disconnected(disconnected) => HubConnectionStatus::Disconnected(
                HubDisconnected::new(disconnected.since.into(), disconnected.failed_attempts)
                    .with_next_attempt_at(disconnected.next_attempt_at.map(Into::into))
                    .with_last_error(disconnected.last_error.clone().map(Into::into)),
            ),
        };
        AgentStatus::new(connection).with_device_id(state.device_id.clone())
    }

    /// Number and connection of the current session, if the agent is connected.
    fn current(&self) -> Option<(u64, ConnectionRef)> {
        let state = self.state.borrow();
        match &state.connection {
            LinkConnection::Connected { connection, .. } => {
                Some((state.sessions, connection.clone()))
            }
            LinkConnection::Disconnected(_) => None,
        }
    }
}

/// Side of the link through which the agent publishes the connection state.
pub(crate) struct LinkController {
    sender: watch::Sender<LinkState>,
}

impl LinkController {
    /// Record that a session has started on `connection`.
    pub(crate) fn connected(&self, device_id: DeviceId, connection: ConnectionRef) {
        self.sender.send_modify(|state| {
            state.device_id = Some(device_id);
            state.sessions += 1;
            state.connection = LinkConnection::Connected {
                connection,
                since: Timestamp::now(),
            };
        });
    }

    /// Record that the current session has ended, with the reason if it failed.
    pub(crate) fn disconnected(&self, error: Option<String>) {
        self.sender.send_modify(|state| {
            state.connection = LinkConnection::Disconnected(Disconnected::since_now(error));
        });
    }

    /// Record a failed connection attempt.
    pub(crate) fn attempt_failed(&self, error: String) {
        self.update_disconnected(|disconnected| {
            disconnected.failed_attempts = disconnected.failed_attempts.saturating_add(1);
            disconnected.last_error = Some(error);
        });
    }

    /// Record the time of the next connection attempt.
    pub(crate) fn next_attempt_at(&self, at: Option<Timestamp>) {
        self.update_disconnected(|disconnected| disconnected.next_attempt_at = at);
    }

    fn update_disconnected(&self, update: impl FnOnce(&mut Disconnected)) {
        self.sender
            .send_if_modified(|state| match &mut state.connection {
                LinkConnection::Disconnected(disconnected) => {
                    update(disconnected);
                    true
                }
                LinkConnection::Connected { .. } => false,
            });
    }
}

/// Executor that opens an executor channel on the connection of the current
/// session and opens a new one after the session changed or the channel failed.
pub(crate) struct LinkExecutor {
    link: HubLink,
    /// Session and channel of the last successful action.
    channel: Option<(u64, ClientExecutor)>,
}

impl Execute for LinkExecutor {
    async fn execute<A: Action>(
        &mut self,
        action: A,
    ) -> Result<Result<A::Output, ActionError>, ExecuteError> {
        let Some((session, mut connection)) = self.link.current() else {
            return Err(ExecuteError::Write(io::Error::new(
                io::ErrorKind::NotConnected,
                "the agent is not connected to Nexigon Hub",
            )));
        };
        // The futures are boxed because action futures are large, which would
        // overflow the stack of unoptimized builds.
        let mut executor = match self.channel.take() {
            Some((channel_session, executor)) if channel_session == session => executor,
            _ => Box::pin(nexigon_client::connect_executor(&mut connection))
                .await
                .map_err(|error| ExecuteError::Write(io::Error::other(error)))?,
        };
        let result = Box::pin(executor.execute(action)).await;
        // A failed exchange may leave a partial frame on the channel.
        if result.is_ok() {
            self.channel = Some((session, executor));
        }
        result
    }
}

#[derive(Clone)]
struct LinkState {
    /// ID of the device, known once the first session has started.
    device_id: Option<DeviceId>,
    /// Number of sessions that have started so far.
    sessions: u64,
    connection: LinkConnection,
}

#[derive(Clone)]
enum LinkConnection {
    Connected {
        connection: ConnectionRef,
        since: Timestamp,
    },
    Disconnected(Disconnected),
}

#[derive(Clone)]
struct Disconnected {
    since: Timestamp,
    failed_attempts: u32,
    next_attempt_at: Option<Timestamp>,
    last_error: Option<String>,
}

impl Disconnected {
    fn since_now(last_error: Option<String>) -> Self {
        Self {
            since: Timestamp::now(),
            failed_attempts: 0,
            next_attempt_at: None,
            last_error,
        }
    }
}
