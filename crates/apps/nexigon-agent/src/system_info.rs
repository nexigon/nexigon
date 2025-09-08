use nexigon_api::types::properties::DiskInfo;
use nexigon_api::types::properties::MemoryInfo;
use nexigon_api::types::properties::NetworkInterfaceInfo;
use nexigon_api::types::properties::SystemInfo;

pub fn get_system_info() -> SystemInfo {
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
    SystemInfo {
        name: sysinfo::System::name(),
        version: sysinfo::System::long_os_version(),
        kernel: Some(sysinfo::System::kernel_long_version()),
        hostname: sysinfo::System::host_name(),
        arch: Some(sysinfo::System::cpu_arch()),
        memory,
        networks,
        disks,
    }
}
