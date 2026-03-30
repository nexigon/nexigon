//! Third-party integrations compiled into the agent binary.

use crate::builtins::BuiltinCommand;
use crate::config::IntegrationsConfig;

#[cfg(target_os = "linux")]
pub mod rugix_apps;

/// Collect commands from all enabled integrations.
pub fn collect_commands(config: &IntegrationsConfig) -> Vec<Box<dyn BuiltinCommand>> {
    let mut commands: Vec<Box<dyn BuiltinCommand>> = Vec::new();

    #[cfg(target_os = "linux")]
    {
        if config
            .rugix_apps
            .as_ref()
            .and_then(|c| c.enabled)
            .unwrap_or(false)
        {
            commands.extend(rugix_apps::commands());
        }
    }

    let _ = config; // suppress unused warning on non-Linux

    commands
}
