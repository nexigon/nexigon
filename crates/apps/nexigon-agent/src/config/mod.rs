//! Nexigon Agent configuration and local access policy.
//!
//! Exposes the Sidex-generated [`Config`] and related types, plus helpers that
//! determine which forwarding and terminal requests the configuration permits.

sidex::include_bundle!(
    #[allow(warnings)]
    nexigon_agent as generated
);
pub use generated::commands;
pub use generated::config::*;
pub use generated::operation_ledger;

/// Default maximum number of simultaneously open multiplex channels.
pub(crate) const DEFAULT_MAX_CHANNELS: u32 = 512;
/// Default channels held back from TCP forwarding.
pub(crate) const DEFAULT_RESERVED_CHANNELS: u32 = 32;
/// Default accepted channel-open rate.
pub(crate) const DEFAULT_MAX_CHANNEL_REQUESTS_PER_SECOND: u32 = 2_048;
const MIN_MAX_CHANNELS: u32 = 32;
const MAX_MAX_CHANNELS: u32 = 4_096;
const MIN_CHANNEL_REQUESTS_PER_SECOND: u32 = 32;
const MAX_CHANNEL_REQUESTS_PER_SECOND: u32 = 65_536;

/// Validated multiplex settings used by transport and task admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MultiplexSettings {
    pub(crate) max_channels: usize,
    pub(crate) forwarding_channels: usize,
    pub(crate) max_channel_requests_per_second: u32,
}

impl MultiplexSettings {
    /// Size the supervisor handoff for a full channel burst and task transitions.
    pub(crate) fn supervisor_queue_capacity(self) -> usize {
        self.max_channels * 2
    }
}

/// Resolve and validate multiplex settings whose invariants span multiple fields.
pub(crate) fn multiplex_settings(config: &Config) -> anyhow::Result<MultiplexSettings> {
    let max_channels = config
        .multiplex
        .as_ref()
        .and_then(|config| config.max_channels)
        .unwrap_or(DEFAULT_MAX_CHANNELS);
    let reserved_channels = config
        .multiplex
        .as_ref()
        .and_then(|config| config.reserved_channels)
        .unwrap_or(DEFAULT_RESERVED_CHANNELS);
    let max_channel_requests_per_second = config
        .multiplex
        .as_ref()
        .and_then(|config| config.max_channel_requests_per_second)
        .unwrap_or(DEFAULT_MAX_CHANNEL_REQUESTS_PER_SECOND);

    if !(MIN_MAX_CHANNELS..=MAX_MAX_CHANNELS).contains(&max_channels) {
        anyhow::bail!("multiplex max-channels must be between 32 and 4096");
    }
    if reserved_channels == 0 || reserved_channels >= max_channels {
        anyhow::bail!("multiplex reserved-channels must be positive and lower than max-channels");
    }
    if !(MIN_CHANNEL_REQUESTS_PER_SECOND..=MAX_CHANNEL_REQUESTS_PER_SECOND)
        .contains(&max_channel_requests_per_second)
    {
        anyhow::bail!("multiplex max-channel-requests-per-second must be between 32 and 65536");
    }
    if max_channel_requests_per_second < max_channels {
        anyhow::bail!("multiplex max-channel-requests-per-second must be at least max-channels");
    }

    Ok(MultiplexSettings {
        max_channels: max_channels as usize,
        forwarding_channels: (max_channels - reserved_channels) as usize,
        max_channel_requests_per_second,
    })
}

/// Check whether a localhost port is exported or explicitly enabled for forwarding.
#[tracing::instrument(level = "debug", skip(config), ret)]
pub fn tcp_forwarding_allowed(config: &Config, port: u16) -> bool {
    if port == 0 {
        return false;
    }
    let exported = config.exports.as_ref().is_some_and(|exports| {
        exports.iter().any(|export| match export {
            ExportConfig::Http(http) => http.port == port,
        })
    });
    exported
        || config.forwarding.as_ref().is_some_and(|forwarding| {
            forwarding.enabled == Some(true)
                && forwarding
                    .allowed_tcp_ports
                    .as_ref()
                    .is_some_and(|ports| ports.iter().any(|allowed| allowed.get() == port))
        })
}

/// Terminal service is available only when it is explicitly enabled.
pub fn terminal_enabled(config: &Config) -> bool {
    config
        .terminal
        .as_ref()
        .is_some_and(|terminal| terminal.enabled == Some(true))
        && terminal_user(config).is_some()
}

/// Return the syntactically usable terminal user, preserving the legacy `root` default.
pub fn terminal_user(config: &Config) -> Option<&str> {
    match config.terminal.as_ref()?.user.as_deref() {
        None => Some("root"),
        Some(user) if !user.is_empty() && user == user.trim() => Some(user),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Config;
    use super::DEFAULT_MAX_CHANNEL_REQUESTS_PER_SECOND;
    use super::DEFAULT_MAX_CHANNELS;
    use super::DEFAULT_RESERVED_CHANNELS;
    use super::TerminalConfig;
    use super::multiplex_settings;
    use super::terminal_enabled;
    use super::terminal_user;

    /// The configuration parser rejects invalid ports and preserves the full valid range.
    #[test]
    fn forwarding_ports_are_validated_during_deserialization() {
        for ports in ["[0]", "[-1]", "[65536]", "['*']", "['22-80']", "[80.5]"] {
            let input = format!(
                "fingerprint-script = 'unused'\n[forwarding]\nenabled = true\nallowed-tcp-ports = {ports}"
            );
            assert!(
                toml::from_str::<Config>(&input).is_err(),
                "accepted {ports}"
            );
        }
        let config: Config = toml::from_str("fingerprint-script = 'unused'\n[forwarding]\nenabled = true\nallowed-tcp-ports = [1, 65535]").unwrap();
        let ports = config.forwarding.unwrap().allowed_tcp_ports.unwrap();
        assert_eq!(
            ports.iter().map(|port| port.get()).collect::<Vec<_>>(),
            [1, 65535]
        );
    }

    fn config(terminal: Option<TerminalConfig>) -> Config {
        Config::new(PathBuf::from("fingerprint")).with_terminal(terminal)
    }

    #[test]
    fn terminal_requires_explicit_enablement_and_defaults_to_root() {
        assert!(!terminal_enabled(&config(None)));
        assert!(!terminal_enabled(&config(Some(TerminalConfig::new()))));
        let legacy = config(Some(TerminalConfig::new().with_enabled(Some(true))));
        assert!(terminal_enabled(&legacy));
        assert_eq!(terminal_user(&legacy), Some("root"));
        assert!(!terminal_enabled(&config(Some(
            TerminalConfig::new()
                .with_enabled(Some(true))
                .with_user(Some("  ".to_owned())),
        ))));
        assert!(!terminal_enabled(&config(Some(
            TerminalConfig::new()
                .with_enabled(Some(true))
                .with_user(Some(" nexigon ".to_owned())),
        ))));
        assert!(!terminal_enabled(&config(Some(
            TerminalConfig::new()
                .with_enabled(Some(false))
                .with_user(Some("nexigon".to_owned())),
        ))));
        assert!(terminal_enabled(&config(Some(
            TerminalConfig::new()
                .with_enabled(Some(true))
                .with_user(Some("nexigon".to_owned())),
        ))));
        assert!(terminal_enabled(&config(Some(
            TerminalConfig::new()
                .with_enabled(Some(true))
                .with_user(Some("root".to_owned())),
        ))));
    }

    /// Multiplex defaults expose 480 forwarding channels and reject incoherent overrides.
    #[test]
    fn multiplex_capacity_is_derived_and_cross_field_validated() {
        let defaults = multiplex_settings(&config(None)).unwrap();
        assert_eq!(defaults.max_channels, DEFAULT_MAX_CHANNELS as usize);
        assert_eq!(
            defaults.forwarding_channels,
            (DEFAULT_MAX_CHANNELS - DEFAULT_RESERVED_CHANNELS) as usize
        );
        assert_eq!(
            defaults.max_channel_requests_per_second,
            DEFAULT_MAX_CHANNEL_REQUESTS_PER_SECOND
        );

        let reserved_all: Config = toml::from_str(
            "fingerprint-script = 'unused'\n[multiplex]\nmax-channels = 64\nreserved-channels = 64",
        )
        .unwrap();
        assert!(multiplex_settings(&reserved_all).is_err());

        let slow_opens: Config = toml::from_str(
            "fingerprint-script = 'unused'\n[multiplex]\nmax-channels = 128\nmax-channel-requests-per-second = 64",
        )
        .unwrap();
        assert!(multiplex_settings(&slow_opens).is_err());
    }
}
