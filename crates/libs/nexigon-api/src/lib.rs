use std::any::Any;

use nexigon_ids::ids::OrganizationId;
use nexigon_ids::ids::ProjectId;
use nexigon_ids::ids::RepositoryId;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::types::jwt::Jwt;
use crate::types::users::UserId;

pub mod types;

/// Represents an action that can be invoked within Nexigon Hub.
pub trait Action: Any + Serialize + DeserializeOwned + Send + std::fmt::Debug {
    /// Output type of the action.
    type Output: Any + Serialize + DeserializeOwned + Send + std::fmt::Debug;

    /// Unique name of the action.
    const NAME: &'static str;

    /// Convert the action to [`AnyAction`].
    fn into_any(self) -> AnyAction;
}

/// A resource that can be audited.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AuditEntity {
    Project(ProjectId),
    User(UserId),
    Repository(RepositoryId),
    Organization(OrganizationId),
}

impl From<ProjectId> for AuditEntity {
    fn from(value: ProjectId) -> Self {
        Self::Project(value)
    }
}

impl From<UserId> for AuditEntity {
    fn from(value: UserId) -> Self {
        Self::User(value)
    }
}

impl From<RepositoryId> for AuditEntity {
    fn from(value: RepositoryId) -> Self {
        Self::Repository(value)
    }
}

impl From<OrganizationId> for AuditEntity {
    fn from(value: OrganizationId) -> Self {
        Self::Organization(value)
    }
}

/// Represents an event that can be recorded in the audit log.
pub trait Event: Any + Serialize + DeserializeOwned + Send + std::fmt::Debug {
    /// Unique name of the event.
    const NAME: &'static str;

    /// Affected entities.
    fn audit_entities(&self) -> impl Iterator<Item = AuditEntity>;
}

/// Macro for generating code for all actions.
/// 
/// This macro takes another macro as an argument and invokes it with a list of actions.
#[macro_export]
#[rustfmt::skip]
macro_rules! with_actions {
    ($name:ident) => {
        $name![
            // # Users
            ("users_Query", QueryUsers, users::QueryUsersAction, users::QueryUsersOutput),
            ("users_GetDetails", GetUserDetails, users::GetUserDetailsAction, users::GetUserDetailsOutput),
            ("users_Create", CreateUser, users::CreateUserAction, users::CreateUserOutput),
            ("users_Delete", DeleteUser, users::DeleteUserAction, outputs::Empty),
            ("users_SetDisplayName", SetUserDisplayName, users::SetUserDisplayNameAction, outputs::Empty),
            ("users_SetPassword", SetUserPassword, users::SetUserPasswordAction, outputs::Empty),
            ("users_SetIsAdmin", SetUserIsAdmin, users::SetUserIsAdminAction, outputs::Empty),
            ("users_ResetPassword", ResetUserPassword, users::ResetUserPasswordAction, outputs::Empty),
            ("users_CompletePasswordReset", CompleteUserPasswordReset, users::CompleteUserPasswordResetAction, users::CompleteUserPasswordResetOutput),
            ("users_QueryTokens", QueryUserTokens, users::QueryUserTokensAction, users::QueryUserTokensOutput),
            ("users_QueryOrganizations", QueryUserOrganizations, users::QueryUserOrganizationsAction, users::QueryUserOrganizationsOutput),
            ("users_QueryOrganizationInvitations", QueryUserOrganizationInvitations, users::QueryUserOrganizationInvitationsAction, users::QueryUserOrganizationInvitationsOutput),
            ("users_QuerySessions", QueryUserSessions, users::QueryUserSessionsAction, users::QueryUserSessionsOutput),
            ("users_AuthenticateWithToken", AuthenticateWithUserToken, users::AuthenticateWithUserTokenAction, users::AuthenticateWithUserTokenOutput),
            ("users_AuthenticateWithSessionToken", AuthenticateWithSessionToken, users::AuthenticateWithSessionTokenAction, users::AuthenticateWithSessionTokenOutput),
            // # User Permissions
            ("users_GetDevicePermissions", GetDevicePermissions, users::GetDevicePermissionsAction, users::GetDevicePermissionsOutput),
            // ## User Tokens
            ("users_CreateToken", CreateUserToken, users::CreateUserTokenAction, users::CreateUserTokenOutput),
            ("users_DeleteToken", DeleteUserToken, users::DeleteUserTokenAction, outputs::Empty),
            // ## User Sessions
            ("users_InitiateSession", InitiateUserSession, users::InitiateUserSessionAction, users::InitiateUserSessionOutput),
            ("users_TerminateSession", TerminateUserSession, users::TerminateUserSessionAction, outputs::Empty),
            ("users_CleanupExpiredSessions", CleanupExpiredUserSessions, users::CleanupExpiredUserSessionsAction, outputs::Empty),
            // ## User Registrations
            ("users_Register", RegisterUser, users::RegisterUserAction, users::RegisterUserOutput),
            ("users_ResendRegistrationEmail", ResendRegistrationEmail, users::ResendRegistrationEmailAction, outputs::Empty),
            ("users_CompleteRegistration", CompleteRegistration, users::CompleteRegistrationAction, users::CompleteRegistrationOutput),
            // ## User Invitations
            ("users_AcceptOrganizationInvitation", AcceptOrganizationInvitation, users::AcceptOrganizationInvitationAction, outputs::Empty),

            // # Organizations
            ("organizations_Query", QueryOrganizations, organizations::QueryOrganizationsAction, organizations::QueryOrganizationsOutput),
            ("organizations_QueryMembers", QueryOrganizationMembers, organizations::QueryOrganizationMembersAction, organizations::QueryOrganizationMembersOutput),
            ("organizations_QueryProjects", QueryOrganizationProjects, organizations::QueryOrganizationProjectsAction, organizations::QueryOrganizationProjectsOutput),
            ("organizations_QueryRepositories", QueryOrganizationRepositories, organizations::QueryOrganizationRepositoriesAction, organizations::QueryOrganizationRepositoriesOutput),
            ("organizations_QueryInvitations", QueryOrganizationInvitations, organizations::QueryOrganizationInvitationsAction, organizations::QueryOrganizationInvitationsOutput),
            ("organizations_Create", CreateOrganization, organizations::CreateOrganizationAction, organizations::CreateOrganizationOutput),
            ("organizations_Delete", DeleteOrganization, organizations::DeleteOrganizationAction, outputs::Empty),
            // ## Organization Members
            ("organizations_AddMember", AddOrganizationMember, organizations::AddOrganizationMemberAction, outputs::Empty),
            ("organizations_RemoveMember", RemoveOrganizationMember, organizations::RemoveOrganizationMemberAction, outputs::Empty),
            ("organizations_InviteMember", InviteOrganizationMember, organizations::InviteOrganizationMemberAction, organizations::InviteOrganizationMemberOutput),
            ("organizations_DeleteInvitation", DeleteOrganizationInvitation, organizations::DeleteOrganizationInvitationAction, outputs::Empty),
            // ## Organization Resources
            ("organizations_GetResourceUsage", GetOrganizationResourceUsage, organizations::GetOrganizationResourceUsageAction, organizations::GetOrganizationResourceUsageOutput),

            // # Projects
            ("projects_Query", QueryProjects, projects::QueryProjectsAction, projects::QueryProjectsOutput),
            ("projects_GetDetails", GetProjectDetails, projects::GetProjectDetailsAction, projects::GetProjectDetailsOutput),
            ("projects_Create", CreateProject, projects::CreateProjectAction, projects::CreateProjectOutput),
            ("projects_Delete", DeleteProject, projects::DeleteProjectAction, outputs::Empty),
            ("projects_QueryDevices", QueryProjectDevices, projects::QueryProjectDevicesAction, projects::QueryProjectDevicesOutput),
            ("projects_QueryDeploymentTokens", QueryProjectDeploymentTokens, projects::QueryProjectDeploymentTokensAction, projects::QueryProjectDeploymentTokensOutput),
            ("projects_QueryLinkedRepositories", QueryProjectRepositories, projects::QueryProjectRepositoriesAction, projects::QueryProjectRepositoriesOutput),
            ("projects_SetOrganization", SetProjectOrganization, projects::SetProjectOrganizationAction, outputs::Empty),
            // ## Deployment Tokens
            ("projects_CreateDeploymentToken", CreateDeploymentToken, projects::CreateDeploymentTokenAction, projects::CreateDeploymentTokenOutput),
            ("projects_DeleteDeploymentToken", DeleteDeploymentToken, projects::DeleteDeploymentTokenAction, outputs::Empty),
            ("projects_SetDeploymentTokenFlags", SetDeploymentTokenFlags, projects::SetDeploymentTokenFlagsAction, outputs::Empty),
            // ## Audit Log
            ("projects_QueryAuditLog", QueryProjectAuditLog, projects::QueryAuditLogEventsAction, projects::QueryAuditLogEventsOutput),
            // ## Repositories
            ("projects_LinkRepository", AddProjectRepository, projects::AddProjectRepositoryAction, outputs::Empty),
            ("projects_UnlinkRepository", RemoveProjectRepository, projects::RemoveProjectRepositoryAction, outputs::Empty),

            // # Devices
            ("devices_Query", QueryDevices, devices::QueryDevicesAction, devices::QueryDevicesOutput),
            ("devices_GetDetails", GetDeviceDetails, devices::GetDeviceDetailsAction, devices::GetDeviceDetailsOutput),
            ("devices_Create", CreateDevice, devices::CreateDeviceAction, devices::CreateDeviceOutput),
            ("devices_Delete", DeleteDevice, devices::DeleteDeviceAction, outputs::Empty),
            ("devices_SetName", SetDeviceName, devices::SetDeviceNameAction, outputs::Empty),
            ("devices_IssueDeviceToken", IssueDeviceToken, devices::IssueDeviceTokenAction, devices::IssueDeviceTokenOutput),
            ("devices_ValidateDeviceToken", ValidateDeviceToken, devices::ValidateDeviceTokenAction, devices::ValidateDeviceTokenOutput),
            ("devices_Authenticate", AuthenticateDevice, devices::AuthenticateDeviceAction, devices::AuthenticateDeviceOutput),
            // ## Device Certificates
            ("devices_AddCertificate", AddDeviceCertificate, devices::AddDeviceCertificateAction, devices::AddDeviceCertificateOutput),
            ("devices_DeleteCertificate", DeleteDeviceCertificate, devices::DeleteDeviceCertificateAction, outputs::Empty),
            ("devices_SetCertificateStatus", SetDeviceCertificateStatus, devices::SetDeviceCertificateStatusAction, outputs::Empty),
            // ## Device Connections
            ("devices_RegisterConnection", RegisterDeviceConnection, devices::RegisterDeviceConnectionAction, devices::RegisterDeviceConnectionOutput),
            ("devices_UnregisterConnection", UnregisterDeviceConnection, devices::UnregisterDeviceConnectionAction, outputs::Empty),
            // ## HTTP Proxy
            ("devices_IssueHttpProxyToken", IssueDeviceHttpProxyToken, devices::IssueDeviceHttpProxyTokenAction, devices::IssueDeviceHttpProxyTokenOutput),
            ("devices_ValidateHttpProxyToken", ValidateDeviceHttpProxyToken, devices::ValidateDeviceHttpProxyTokenAction, devices::ValidateDeviceHttpProxyTokenOutput),
            // ## Device Events
            ("devices_PublishEvents", PublishDeviceEvents, devices::PublishDeviceEventsAction, outputs::Empty),
            ("devices_QueryEvents", QueryDeviceEvents, devices::QueryDeviceEventsAction, devices::QueryDeviceEventsOutput),
            // ## Device Properties
            ("devices_SetProperty", SetDeviceProperty, devices::SetDevicePropertyAction, outputs::Empty),
            ("devices_GetProperty", GetDeviceProperty, devices::GetDevicePropertyAction, devices::GetDevicePropertyOutput),
            ("devices_RemoveProperty", RemoveDeviceProperty, devices::RemoveDevicePropertyAction, devices::RemoveDevicePropertyOutput),
            ("devices_QueryProperties", QueryDeviceProperties, devices::QueryDevicePropertiesAction, devices::QueryDevicePropertiesOutput),
            // ## Device Resources
            ("devices_GetResourceUsage", GetDeviceResourceUsage, devices::GetDeviceResourceUsageAction, devices::GetDeviceResourceUsageOutput),
            ("devices_GetConsumption", GetDeviceConsumption, devices::GetDeviceConsumptionAction, devices::GetDeviceConsumptionOutput),

            // # Repositories
            ("repositories_ResolveName", ResolveRepositoryName, repositories::ResolveRepositoryNameAction, repositories::ResolveRepositoryNameOutput),
            ("repositories_GetDetails", GetRepositoryDetails, repositories::GetRepositoryDetailsAction, repositories::GetRepositoryDetailsOutput),
            ("repositories_Create", CreateRepository, repositories::CreateRepositoryAction, repositories::CreateRepositoryOutput),
            ("repositories_Delete", DeleteRepository, repositories::DeleteRepositoryAction, outputs::Empty),
            ("repositories_SetOrganization", SetRepositoryOrganization, repositories::SetRepositoryOrganizationAction, outputs::Empty),
            ("repositories_SetVisibility", SetRepositoryVisibility, repositories::SetRepositoryVisibilityAction, outputs::Empty),
            ("repositories_QueryPackages", QueryRepositoryPackages, repositories::QueryRepositoryPackagesAction, repositories::QueryRepositoryPackagesOutput),
            ("repositories_QueryAssets", QueryRepositoryAssets, repositories::QueryRepositoryAssetsAction, repositories::QueryRepositoryAssetsOutput),
            ("repositories_QueryLinkedProjects", QueryRepositoryProjects, repositories::QueryRepositoryProjectsAction, repositories::QueryRepositoryProjectsOutput),
            // ## Packages
            ("repositories_ResolvePackageByPath", ResolvePackageByPath, repositories::ResolvePackageByPathAction, repositories::ResolvePackageByPathOutput),
            ("repositories_GetPackageDetails", GetPackageDetails, repositories::GetPackageDetailsAction, repositories::GetPackageDetailsOutput),
            ("repositories_CreatePackage", CreatePackage, repositories::CreatePackageAction, repositories::CreatePackageOutput),
            ("repositories_DeletePackage", DeletePackage, repositories::DeletePackageAction, outputs::Empty),
            ("repositories_QueryPackageVersions", QueryPackageVersions, repositories::QueryPackageVersionsAction, repositories::QueryPackageVersionsOutput),
            // ## Package Versions
            ("repositories_ResolveVersionByPath", ResolvePackageVersionByPath, repositories::ResolvePackageVersionByPathAction, repositories::ResolvePackageVersionByPathOutput),
            ("repositories_GetVersionDetails", GetPackageVersionDetails, repositories::GetPackageVersionDetailsAction, repositories::GetPackageVersionDetailsOutput),
            ("repositories_CreateVersion", CreatePackageVersion, repositories::CreatePackageVersionAction, repositories::CreatePackageVersionOutput),
            ("repositories_DeleteVersion", DeletePackageVersion, repositories::DeletePackageVersionAction, outputs::Empty),
            ("repositories_AddVersionAsset", AddPackageVersionAsset, repositories::AddPackageVersionAssetAction, repositories::AddPackageVersionAssetOutput),
            ("repositories_RemoveVersionAsset", RemovePackageVersionAsset, repositories::RemovePackageVersionAssetAction, outputs::Empty),
            ("repositories_TagVersion", TagPackageVersion, repositories::TagPackageVersionAction, outputs::Empty),
            ("repositories_UntagVersion", UntagPackageVersion, repositories::UntagPackageVersionAction, outputs::Empty),
            ("repositories_ResolveVersionAssetByPath", ResolvePackageVersionAssetByPath, repositories::ResolvePackageVersionAssetByPathAction, repositories::ResolvePackageVersionAssetByPathOutput),
            // ## S3 Config
            ("repositories_SetS3Config", SetRepositoryS3Credentials, repositories::SetRepositoryS3ConfigAction, outputs::Empty),
            ("repositories_GetS3Config", GetRepositoryS3Credentials, repositories::GetRepositoryS3ConfigAction, repositories::GetRepositoryS3ConfigOutput),
            // ## Assets
            ("repositories_GetAssetDetails", GetAssetDetails, repositories::GetAssetDetailsAction, repositories::GetAssetDetailsOutput),
            ("repositories_CreateAsset", CreateAsset, repositories::CreateAssetAction, repositories::CreateAssetOutput),
            ("repositories_DeleteAsset", DeleteAsset, repositories::DeleteAssetAction, outputs::Empty),
            ("repositories_IssueAssetDownloadUrl", IssueAssetDownloadUrl, repositories::IssueAssetDownloadUrlAction, repositories::IssueAssetDownloadUrlOutput),
            ("repositories_IssueAssetUploadUrl", IssueAssetUploadUrl, repositories::IssueAssetUploadUrlAction, repositories::IssueAssetUploadUrlOutput),
            // # Audit Log
            ("repositories_QueryAuditLog", QueryRepositoryAuditLogEvents, repositories::QueryAuditLogEventsAction, repositories::QueryAuditLogEventsOutput),

            // # Audit Log
            ("audit_QueryAuditLogEvents", QueryAuditLogEvents, audit::QueryAuditLogEventsAction, audit::QueryAuditLogEventsOutput),
            ("audit_QueryAuditLogActions", QueryAuditLogActions, audit::QueryAuditLogActionsAction, audit::QueryAuditLogActionsOutput),

            // # Jobs
            ("jobs_Query", QueryJobs, jobs::QueryJobsAction, jobs::QueryJobsOutput),

            // # Instance
            ("instance_GetStatistics", GetInstanceStatistics, instance::GetInstanceStatisticsAction, instance::GetInstanceStatisticsOutput),
            ("instance_GetSettingsRaw", GetInstanceSettingsRaw, instance::GetInstanceSettingsRawAction, instance::GetInstanceSettingsRawOutput),
            ("instance_SetSettingRaw", SetInstanceSettingRaw, instance::SetInstanceSettingRawAction, outputs::Empty),

            // # Cluster
            ("cluster_GetDetails", GetClusterDetails, cluster::GetClusterDetailsAction, cluster::GetClusterDetailsOutput),
            // ## Cluster Nodes
            ("cluster_RegisterNode", RegisterClusterNode, cluster::RegisterClusterNodeAction, cluster::RegisterClusterNodeOutput),
            ("cluster_ReportNodeHeartbeat", ReportClusterNodeHeartbeat, cluster::ReportClusterNodeHeartbeatAction, outputs::Empty),
            ("cluster_CleanupInactiveNodes", CleanupInactiveClusterNodes, cluster::CleanupInactiveClusterNodesAction, outputs::Empty),

            // # Actors
            ("actor_GetActor", GetActor, actor::GetActorAction, actor::GetActorOutput),
        ];
    };
}

/// Auxiliary macro for implementing [`Action`] for all actions.
macro_rules! impl_actions {
    ($(($name:literal, $variant:ident, $input:path, $output:path),)*) => {
        use types::*;

        $(
            impl Action for $input {
                type Output = $output;

                const NAME: &'static str = $name;

                fn into_any(self) -> AnyAction {
                    AnyAction::$variant(self)
                }
            }
        )*

        /// Any action.
        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
        #[serde(tag = "action", content = "parameters")]
        pub enum AnyAction {
            $(
                #[doc = concat!("Action `", $name, "`.")]
                #[serde(rename = $name)]
                $variant($input),
            )*
        }

        // Ensure that all action names are unique.
        const _: () = {
            fn _assert_unique_names(name: &str) {
                #[deny(unreachable_patterns, reason = "duplicate action names")]
                match name {
                    $(
                        $name => {},
                    )*
                    _ => {}
                }
            }
        };
    };
}

with_actions!(impl_actions);

/// Macro for generating code for all events.
/// 
/// This macro takes another macro as an argument and invokes it with a list of events.
#[rustfmt::skip]
macro_rules! with_events {
    ($name:ident) => {
        $name![
            // # Users
            ("users_Created", users::UserCreatedEvent, { user_id }),
            ("users_Deleted", users::UserDeletedEvent, {}),
            ("users_SetIsAdmin", users::UserSetIsAdminEvent, { user_id }),
            ("users_SetPassword", users::UserSetPasswordEvent, { user_id }),
            ("users_TokenCreated", users::UserTokenCreatedEvent, { user_id }),
            ("users_TokenDeleted", users::UserTokenDeletedEvent, { user_id }),
            ("users_SessionInitiated", users::UserSessionInitiatedEvent, { user_id }),
            ("users_RegistrationCreated", users::UserRegistrationCreatedEvent, { user_id }),
            ("users_RegistrationEmailSent", users::UserRegistrationEmailSentEvent, { user_id }),
            ("users_RegistrationCompleted", users::UserRegistrationCompletedEvent, { user_id }),

            // # Projects
            ("projects_Created", projects::ProjectCreatedEvent, { project_id }),
            ("projects_Deleted", projects::ProjectDeletedEvent, {}),
            ("projects_DeploymentTokenCreated", projects::DeploymentTokenCreatedEvent, { project_id }),
            ("projects_DeploymentTokenDeleted", projects::DeploymentTokenDeletedEvent, { project_id }),
            ("projects_DeploymentTokenFlagsChanged", projects::DeploymentTokenFlagsChangedEvent, { project_id }),
            ("projects_RepositoryAdded", projects::ProjectRepositoryAddedEvent, { project_id, repository_id }),
            ("projects_RepositoryRemoved", projects::ProjectRepositoryRemovedEvent, { project_id, repository_id }),

            // # Organizations
            ("organizations_Created", organizations::OrganizationCreatedEvent, { organization_id }),
            ("organizations_Deleted", organizations::OrganizationDeletedEvent, { organization_id }),
            ("organizations_MemberAdded", organizations::OrganizationMemberAddedEvent, { organization_id, user_id }),
            ("organizations_MemberRemoved", organizations::OrganizationMemberRemovedEvent, { organization_id, user_id }),
            ("organizations_InvitationCreated", organizations::OrganizationInvitationCreatedEvent, { organization_id }),

            // # Devices
            ("devices_Created", devices::DeviceCreatedEvent, { project_id }),
            ("devices_Deleted", devices::DeviceDeletedEvent, { project_id }),
            ("devices_CertificateAdded", devices::DeviceCertificateAddedEvent, { project_id }),
            ("devices_CertificateDeleted", devices::DeviceCertificateDeletedEvent, { project_id }),
            ("devices_CertificateStatusChanged", devices::DeviceCertificateStatusChangedEvent, { project_id }),
        ];
    };
}

macro_rules! impl_events {
    ($(($name:literal, $event:path, { $($entity:ident),* }),)*) => {
        $(
            impl Event for $event {
                const NAME: &'static str = $name;

                fn audit_entities(&self) -> impl Iterator<Item = AuditEntity> {
                    [
                        $(
                            (self.$entity).clone().into(),
                        )*
                    ].into_iter()
                }
            }
        )*
    };
}

with_events!(impl_events);

/// Executor for actions.
pub trait Executor {
    /// Error type.
    type Error: 'static + std::error::Error + Send + Sync;

    /// Execute an action.
    fn execute<A: Action>(
        &self,
        action: A,
    ) -> impl Future<Output = Result<A::Output, Self::Error>> + Send;
}

impl Jwt {
    pub fn from_string_unchecked(jwt: String) -> Self {
        Self(jwt)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
