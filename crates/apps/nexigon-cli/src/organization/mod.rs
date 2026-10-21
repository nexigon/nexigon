//! Organization selection from the authenticated actor and CLI options.

use anyhow::bail;
use nexigon_api::types::actor::Actor;
use nexigon_ids::ids::OrganizationId;

/// Reject actor types that cannot authenticate the CLI.
pub fn validate_actor(actor: &Actor) -> anyhow::Result<()> {
    match actor {
        Actor::UserToken(_) | Actor::OrganizationApiToken(_) => Ok(()),
        _ => bail!("received unexpected actor type"),
    }
}

/// Resolve one organization without choosing an implicit default for a user.
pub fn select(actor: &Actor, requested: Option<&OrganizationId>) -> anyhow::Result<OrganizationId> {
    match actor {
        Actor::UserToken(_) => requested.cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "--organization <org-id> is required with a user token for resource commands"
            )
        }),
        Actor::OrganizationApiToken(actor) => {
            if requested.is_some_and(|id| id != &actor.organization_id) {
                bail!("--organization conflicts with the organization API token");
            }
            Ok(actor.organization_id.clone())
        }
        _ => bail!("received unexpected actor type"),
    }
}
