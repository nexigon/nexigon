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
    use super::TerminalConfig;
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
}
