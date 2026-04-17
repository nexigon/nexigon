use nexigon_api::types::properties::AgentCommandsConfig;
use nexigon_api::types::properties::AgentConfig;
use nexigon_api::types::properties::AgentInfo;
use nexigon_api::types::properties::AgentTerminalConfig;
use nexigon_api::types::properties::DiskInfo;
use nexigon_api::types::properties::ExportInfo;
use nexigon_api::types::properties::HttpExportInfo;
use nexigon_api::types::properties::MemoryInfo;
use nexigon_api::types::properties::NetworkInterfaceInfo;
use nexigon_api::types::properties::RugixSystemInfo;
use nexigon_api::types::properties::SystemInfo;
use nexigon_api::types::properties::YoctoSystemInfo;
use reportify::ResultExt;

use crate::config::Config;
use crate::config::ExportConfig;

/// Gather available system information for `dev.nexigon.system.info` property.
pub fn get_system_info(config: &Config) -> SystemInfo {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let memory = MemoryInfo {
        total: system.total_memory(),
    };
    let networks = sysinfo::Networks::new_with_refreshed_list()
        .iter()
        .map(|(name, network)| NetworkInterfaceInfo {
            name: name.clone(),
            mac_address: network.mac_address().to_string(),
            ip_addresses: network
                .ip_networks()
                .iter()
                .map(|ip| format!("{}/{}", ip.addr, ip.prefix))
                .collect(),
        })
        .collect();
    let disks = sysinfo::Disks::new_with_refreshed_list()
        .iter()
        .map(|disk| DiskInfo {
            name: disk.name().to_string_lossy().into_owned(),
            filesystem: disk.file_system().to_string_lossy().into_owned(),
            mount_point: disk.mount_point().to_string_lossy().into_owned(),
            total_space: disk.total_space(),
            available_space: disk.available_space(),
        })
        .collect();
    let exports = config
        .exports
        .as_ref()
        .map(|exports| exports.iter().map(convert_export).collect::<Vec<_>>());
    SystemInfo {
        name: sysinfo::System::name(),
        version: sysinfo::System::long_os_version(),
        kernel: Some(sysinfo::System::kernel_long_version()),
        hostname: sysinfo::System::host_name(),
        arch: Some(sysinfo::System::cpu_arch()),
        memory,
        networks,
        disks,
        exports,
        rugix: get_rugix_info(),
        yocto: get_yocto_info(),
        agent: Some(AgentInfo::new(
            nexigon_version::NEXIGON_GIT_VERSION.to_owned(),
            build_agent_config(config),
        )),
    }
}

/// Convert [`ExportConfig`] to [`ExportInfo`].
fn convert_export(export: &ExportConfig) -> ExportInfo {
    match export {
        ExportConfig::Http(config) => ExportInfo::Http(HttpExportInfo {
            name: config.name.clone(),
            port: config.port,
            path: config.path.clone(),
        }),
    }
}

/// Get Rugix-specific system information (if available).
fn get_rugix_info() -> Option<RugixSystemInfo> {
    std::process::Command::new("rugix-ctrl")
        .args(["system", "info", "--json"])
        .output()
        .ok()
        .and_then(|output| serde_json::from_slice(&output.stdout).log_ok())
        .map(|mut info: RugixSystemInfo| {
            if info.build.is_none() {
                info.build = std::fs::read_to_string("/etc/rugix/system-build-info.json")
                    .ok()
                    .and_then(|build_info| serde_json::from_str(&build_info).log_ok());
            }
            info
        })
}

/// Build agent config from the device configuration.
fn build_agent_config(config: &Config) -> AgentConfig {
    let terminal_enabled = config
        .terminal
        .as_ref()
        .and_then(|t| t.enabled)
        .unwrap_or(false);
    let commands_enabled = config
        .commands
        .as_ref()
        .and_then(|c| c.enabled)
        .unwrap_or(false);
    AgentConfig::new()
        .with_terminal(Some(
            AgentTerminalConfig::new()
                .with_enabled(Some(terminal_enabled))
                .with_users(terminal_users(config)),
        ))
        .with_commands(Some(
            AgentCommandsConfig::new().with_enabled(Some(commands_enabled)),
        ))
}

/// Collect available terminal users from the config.
fn terminal_users(config: &Config) -> Option<Vec<String>> {
    let terminal = config.terminal.as_ref()?;
    if !terminal.enabled.unwrap_or(false) {
        return None;
    }
    let default_user = terminal.user.clone().unwrap_or_else(|| "root".to_owned());
    let mut users = vec![default_user];
    if let Some(allowed) = &terminal.allowed_users {
        for user in allowed {
            if !users.contains(user) {
                users.push(user.clone());
            }
        }
    }
    Some(users)
}

/// Read Yocto build information from `/etc/buildinfo` (if available).
fn get_yocto_info() -> Option<YoctoSystemInfo> {
    std::fs::read_to_string("/etc/buildinfo")
        .ok()
        .map(|build_info| YoctoSystemInfo {
            build_info: build_info
                .lines()
                .filter_map(|line| {
                    line.split_once('=')
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                })
                .collect(),
        })
}
