//! Nexigon Agent configuration.

sidex::include_bundle!(
    #[allow(warnings)]
    nexigon_agent as generated
);
pub use generated::commands;
pub use generated::config::*;
pub use generated::operation_ledger;

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
