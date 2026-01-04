//! API type definitions.

#[allow(warnings)]
mod generated;
pub use generated::*;
use nexigon_ids::ids::UserId;

use crate::types::actor::Actor;

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

impl<T> From<errors::ActionResult<T>> for Result<T, errors::ActionError> {
    fn from(value: errors::ActionResult<T>) -> Self {
        match value {
            errors::ActionResult::Ok(value) => Ok(value),
            errors::ActionResult::Error(error) => Err(error),
        }
    }
}

impl<T> From<Result<T, errors::ActionError>> for errors::ActionResult<T> {
    fn from(value: Result<T, errors::ActionError>) -> Self {
        match value {
            Ok(value) => errors::ActionResult::Ok(value),
            Err(error) => errors::ActionResult::Error(error),
        }
    }
}

impl std::fmt::Display for errors::ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for errors::ActionError {}

impl Actor {
    pub fn user_id(&self) -> Option<&UserId> {
        match self {
            Actor::User(user) => Some(&user.user_id),
            Actor::UserToken(user) => Some(&user.user_id),
            _ => None,
        }
    }
}
