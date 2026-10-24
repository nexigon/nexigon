//! Access to the Device API of Nexigon Hub.
//!
//! Devices run released agent versions for years. Nexigon Hub keeps the actions of the
//! Device API backward compatible for at least five years, while other actions may change
//! sooner. The agent therefore invokes Nexigon Hub actions through [`DeviceExecutor`],
//! which accepts nothing else. The only exception is the repository commands, which
//! the agent shares with Nexigon CLI.

use nexigon_api::Action;
use nexigon_api::types::DeviceGroupMarker;
use nexigon_api::types::errors::ActionError;
use nexigon_client::ClientError;
use nexigon_client::ClientExecutor;
use nexigon_client::Execute;
use nexigon_multiplex::ConnectionRef;
use nexigon_rpc::ExecuteError;

/// Executor for actions of the Device API.
#[derive(Debug)]
pub struct DeviceExecutor<E = ClientExecutor>(E);

impl<E: Execute> DeviceExecutor<E> {
    /// Restrict `executor` to the Device API.
    pub fn new(executor: E) -> Self {
        Self(executor)
    }

    /// Execute the given Device API [`Action`].
    pub async fn execute<A: Action + DeviceGroupMarker>(
        &mut self,
        action: A,
    ) -> Result<Result<A::Output, ActionError>, ExecuteError> {
        self.0.execute(action).await
    }

    /// Lift the restriction to the Device API.
    pub fn into_inner(self) -> E {
        self.0
    }
}

/// Connect a [`DeviceExecutor`] via the given [`ConnectionRef`].
pub async fn connect_device_executor(
    connection: &mut ConnectionRef,
) -> Result<DeviceExecutor, ClientError> {
    nexigon_client::connect_executor(connection)
        .await
        .map(DeviceExecutor::new)
}
