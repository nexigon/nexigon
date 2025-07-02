//! API type definitions.

#[allow(warnings)]
mod generated;
pub use generated::*;

impl std::str::FromStr for devices::DeviceEventSeverity {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trace" => Ok(devices::DeviceEventSeverity::Trace),
            "debug" => Ok(devices::DeviceEventSeverity::Debug),
            "info" => Ok(devices::DeviceEventSeverity::Info),
            "warning" => Ok(devices::DeviceEventSeverity::Warning),
            "error" => Ok(devices::DeviceEventSeverity::Error),
            "critical" => Ok(devices::DeviceEventSeverity::Critical),
            _ => Err("invalid severity"),
        }
    }
}

#[expect(clippy::derivable_impls)]
impl Default for devices::DeviceEventSeverity {
    fn default() -> Self {
        devices::DeviceEventSeverity::Info
    }
}
