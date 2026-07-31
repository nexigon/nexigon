//! Nexigon CLI configuration.

use std::fmt;
use std::str::FromStr;

use nexigon_client::ClientToken;
use nexigon_ids::AnyId;
use nexigon_ids::Id;
use nexigon_ids::ids::OrganizationApiToken;
use nexigon_ids::ids::UserToken;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;

/// Token accepted by the Nexigon CLI for authentication.
#[derive(Clone, PartialEq, Eq)]
pub enum AuthenticationToken {
    /// Organization-scoped API token.
    OrganizationApiToken(OrganizationApiToken),
    /// User-scoped access token.
    UserToken(UserToken),
}

impl fmt::Debug for AuthenticationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OrganizationApiToken(_) => {
                formatter.write_str("AuthenticationToken::OrganizationApiToken(<redacted>)")
            }
            Self::UserToken(_) => formatter.write_str("AuthenticationToken::UserToken(<redacted>)"),
        }
    }
}

/// Error returned when a CLI authentication token has an unsupported type.
#[derive(Debug, thiserror::Error)]
#[error("expected a Nexigon user token or organization API token")]
pub struct InvalidAuthenticationToken;

impl FromStr for AuthenticationToken {
    type Err = InvalidAuthenticationToken;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.parse::<AnyId>() {
            Ok(AnyId::OrganizationApiToken(token)) => Ok(Self::OrganizationApiToken(token)),
            Ok(AnyId::UserToken(token)) => Ok(Self::UserToken(token)),
            _ => Err(InvalidAuthenticationToken),
        }
    }
}

impl Serialize for AuthenticationToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::OrganizationApiToken(token) => serializer.serialize_str(&token.stringify()),
            Self::UserToken(token) => serializer.serialize_str(&token.stringify()),
        }
    }
}

impl<'de> Deserialize<'de> for AuthenticationToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;

        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

impl From<AuthenticationToken> for ClientToken {
    fn from(token: AuthenticationToken) -> Self {
        match token {
            AuthenticationToken::OrganizationApiToken(token) => Self::OrganizationApiToken(token),
            AuthenticationToken::UserToken(token) => Self::UserToken(token),
        }
    }
}

sidex::include_bundle!(
    #[allow(warnings)]
    nexigon_cli as generated
);
pub use generated::config::*;

#[cfg(test)]
mod tests {
    use nexigon_ids::Generate;
    use nexigon_ids::Id;
    use nexigon_ids::ids::DeploymentToken;
    use nexigon_ids::ids::OrganizationApiToken;
    use nexigon_ids::ids::UserToken;

    use super::AuthenticationToken;
    use super::Config;

    // Existing user-token configurations remain valid and serializable.
    #[test]
    fn user_token_config_round_trips() {
        let token = UserToken::generate();
        let config = Config {
            hub_url: "https://hub.example.test".to_owned(),
            token: AuthenticationToken::UserToken(token.clone()),
        };

        let encoded = toml::to_string(&config).unwrap();
        let decoded: Config = toml::from_str(&encoded).unwrap();

        assert_eq!(decoded.token, AuthenticationToken::UserToken(token));
    }

    // Organization API tokens use the same configuration field as user tokens.
    #[test]
    fn organization_api_token_config_round_trips() {
        let token = OrganizationApiToken::generate();
        let config = Config {
            hub_url: "https://hub.example.test".to_owned(),
            token: AuthenticationToken::OrganizationApiToken(token.clone()),
        };

        let encoded = toml::to_string(&config).unwrap();
        let decoded: Config = toml::from_str(&encoded).unwrap();

        assert_eq!(
            decoded.token,
            AuthenticationToken::OrganizationApiToken(token)
        );
    }

    // Deployment credentials cannot be used to authenticate an interactive CLI.
    #[test]
    fn deployment_tokens_are_rejected_without_exposing_the_secret() {
        let token = DeploymentToken::generate().stringify();

        let error = token.parse::<AuthenticationToken>().unwrap_err();

        assert!(!error.to_string().contains(&token));
    }

    // Diagnostics identify the token kind without disclosing the credential.
    #[test]
    fn debug_output_redacts_tokens() {
        let token = OrganizationApiToken::generate();
        let secret = token.stringify();

        let output = format!("{:?}", AuthenticationToken::OrganizationApiToken(token));

        assert!(!output.contains(&secret));
        assert!(output.contains("<redacted>"));
    }
}
