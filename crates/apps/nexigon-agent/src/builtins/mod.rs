//! Built-in commands compiled into the agent binary.

use std::future::Future;
use std::pin::Pin;

use nexigon_api::types::devices::DeviceCommandDoneData;
use nexigon_api::types::properties::DeviceCommandDescriptor;

use nexigon_multiplex::ConnectionRef;

use crate::config::BuiltinCommandsConfig;

#[cfg(target_os = "linux")]
mod system;
#[cfg(target_os = "linux")]
mod systemd;

/// Context passed to built-in command invocations.
pub struct InvocationCtx {
    pub input: serde_json::Value,
    /// Reference to the hub connection for issuing API calls.
    pub connection_ref: Option<ConnectionRef>,
}

/// A built-in command that executes natively in the agent process.
pub trait BuiltinCommand: Send + Sync {
    /// Returns the descriptor for this command (used in the manifest).
    fn descriptor(&self) -> DeviceCommandDescriptor;

    /// Execute the command with the given invocation context.
    fn execute(
        &self,
        ctx: InvocationCtx,
    ) -> Pin<Box<dyn Future<Output = DeviceCommandDoneData> + Send + '_>>;
}

/// Collect all enabled built-in commands based on configuration.
pub fn collect_builtins(config: &BuiltinCommandsConfig) -> Vec<Box<dyn BuiltinCommand>> {
    let mut builtins: Vec<Box<dyn BuiltinCommand>> = Vec::new();

    #[cfg(target_os = "linux")]
    {
        if config
            .system
            .as_ref()
            .and_then(|g| g.enabled)
            .unwrap_or(false)
        {
            builtins.extend(system::commands());
        }
        if config
            .systemd
            .as_ref()
            .and_then(|g| g.enabled)
            .unwrap_or(false)
        {
            builtins.extend(systemd::commands());
        }
    }

    let _ = config; // suppress unused warning on non-Linux

    builtins
}
