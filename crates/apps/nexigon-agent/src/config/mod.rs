//! Nexigon Agent configuration.

sidex::include_bundle!(
    #[allow(warnings)]
    nexigon_agent as generated
);
pub use generated::commands;
pub use generated::config::*;
