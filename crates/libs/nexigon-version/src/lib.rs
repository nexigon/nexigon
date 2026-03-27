//! Nexigon version information.

/// Nexigon Git version string.
pub const NEXIGON_GIT_VERSION: &str = match option_env!("NEXIGON_GIT_VERSION") {
    Some(version) => version,
    None => "unknown",
};
