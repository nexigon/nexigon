//! Nexigon CLI configuration.

sidex::include_bundle!(
    #[allow(warnings)]
    nexigon_cli as generated
);
pub use generated::config::*;
