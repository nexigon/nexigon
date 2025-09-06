use std::any::Any;

use nexigon_ids::ids::ProjectId;
use nexigon_ids::ids::RepositoryId;
use serde::Serialize;
use serde::de::DeserializeOwned;

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
            ("users_QueryUsers", QueryUsers, users::QueryUsersAction, users::QueryUsersOutput),
            ("users_GetUserDetails", GetUserDetails, users::GetUserDetailsAction, users::GetUserDetailsOutput),
            ("users_CreateUser", CreateUser, users::CreateUserAction, users::CreateUserOutput),
            ("users_DeleteUser", DeleteUser, users::DeleteUserAction, outputs::Empty),
            ("users_SetUserDisplayName", SetUserDisplayName, users::SetUserDisplayNameAction, outputs::Empty),
            ("users_SetUserPassword", SetUserPassword, users::SetUserPasswordAction, outputs::Empty),
            ("users_SetUserIsAdmin", SetUserIsAdmin, users::SetUserIsAdminAction, outputs::Empty),
            ("users_QueryUserOrganizations", QueryUserOrganizations, users::QueryUserOrganizationsAction, users::QueryUserOrganizationsOutput),
            ("users_QueryUserTokens", QueryUserTokens, users::QueryUserTokensAction, users::QueryUserTokensOutput),
            ("users_QueryUserProjectInvitations", QueryUserProjectInvitations, users::QueryUserProjectInvitationsAction, users::QueryUserProjectInvitationsOutput),
            ("users_QueryUserRepositoryInvitations", QueryUserRepositoryInvitations, users::QueryUserRepositoryInvitationsAction, users::QueryUserRepositoryInvitationsOutput),
            ("users_QueryUserSessions", QueryUserSessions, users::QueryUserSessionsAction, users::QueryUserSessionsOutput),
            ("users_AuthenticateWithUserToken", AuthenticateWithUserToken, users::AuthenticateWithUserTokenAction, users::AuthenticateWithUserTokenOutput),
            ("users_AuthenticateWithSessionToken", AuthenticateWithSessionToken, users::AuthenticateWithSessionTokenAction, users::AuthenticateWithSessionTokenOutput),
            // # User Permissions
            ("users_GetDevicePermissions", GetDevicePermissions, users::GetDevicePermissionsAction, users::GetDevicePermissionsOutput),
            // ## User Tokens
            ("users_CreateUserToken", CreateUserToken, users::CreateUserTokenAction, users::CreateUserTokenOutput),
            ("users_DeleteUserToken", DeleteUserToken, users::DeleteUserTokenAction, outputs::Empty),
            // ## User Sessions
            ("users_InitiateUserSession", InitiateUserSession, users::InitiateUserSessionAction, users::InitiateUserSessionOutput),
            ("users_TerminateUserSession", TerminateUserSession, users::TerminateUserSessionAction, outputs::Empty),
            ("users_CleanupExpiredUserSessions", CleanupExpiredUserSessions, users::CleanupExpiredUserSessionsAction, outputs::Empty),
            // ## User Registrations
            ("users_RegisterUser", RegisterUser, users::RegisterUserAction, users::RegisterUserOutput),
            ("users_ResendRegistrationEmail", ResendRegistrationEmail, users::ResendRegistrationEmailAction, outputs::Empty),
            ("users_CompleteRegistration", CompleteRegistration, users::CompleteRegistrationAction, users::CompleteRegistrationOutput),
            // ## User Invitations
            ("users_AcceptProjectInvitation", AcceptProjectInvitation, users::AcceptProjectInvitationAction, outputs::Empty),
            ("users_AcceptRepositoryInvitation", AcceptRepositoryInvitation, users::AcceptRepositoryInvitationAction, outputs::Empty),

            // # Projects
            ("projects_QueryProjects", QueryProjects, projects::QueryProjectsAction, projects::QueryProjectsOutput),
            ("projects_GetProjectDetails", GetProjectDetails, projects::GetProjectDetailsAction, projects::GetProjectDetailsOutput),
            ("projects_CreateProject", CreateProject, projects::CreateProjectAction, projects::CreateProjectOutput),
            ("projects_DeleteProject", DeleteProject, projects::DeleteProjectAction, outputs::Empty),
            ("projects_QueryProjectDevices", QueryProjectDevices, projects::QueryProjectDevicesAction, projects::QueryProjectDevicesOutput),
            ("projects_QueryProjectMembers", QueryProjectMembers, projects::QueryProjectMembersAction, projects::QueryProjectMembersOutput),
            ("projects_QueryProjectInvitations", QueryProjectInvitations, projects::QueryProjectInvitationsAction, projects::QueryProjectInvitationsOutput),
            ("projects_QueryProjectDeploymentTokens", QueryProjectDeploymentTokens, projects::QueryProjectDeploymentTokensAction, projects::QueryProjectDeploymentTokensOutput),
            ("projects_QueryProjectRepositories", QueryProjectRepositories, projects::QueryProjectRepositoriesAction, projects::QueryProjectRepositoriesOutput),
            ("projects_AddProjectMember", AddProjectMember, projects::AddProjectMemberAction, outputs::Empty),
            ("projects_RemoveProjectMember", RemoveProjectMember, projects::RemoveProjectMemberAction, outputs::Empty),
            ("projects_InviteProjectMember", InviteProjectMember, projects::InviteProjectMemberAction, projects::InviteProjectMemberOutput),
            ("projects_DeleteProjectInvitation", DeleteProjectInvitation, projects::DeleteProjectInvitationAction, outputs::Empty),
            // ## Deployment Tokens
            ("projects_CreateDeploymentToken", CreateDeploymentToken, projects::CreateDeploymentTokenAction, projects::CreateDeploymentTokenOutput),
            ("projects_DeleteDeploymentToken", DeleteDeploymentToken, projects::DeleteDeploymentTokenAction, outputs::Empty),
            ("projects_SetDeploymentTokenFlags", SetDeploymentTokenFlags, projects::SetDeploymentTokenFlagsAction, outputs::Empty),
            // ## Audit Log
            ("projects_QueryAuditLog", QueryProjectAuditLog, projects::QueryAuditLogEventsAction, projects::QueryAuditLogEventsOutput),
            // ## Repositories
            ("projects_AddProjectRepository", AddProjectRepository, projects::AddProjectRepositoryAction, outputs::Empty),
            ("projects_RemoveProjectRepository", RemoveProjectRepository, projects::RemoveProjectRepositoryAction, outputs::Empty),

            // # Devices
            ("devices_QueryDevices", QueryDevices, devices::QueryDevicesAction, devices::QueryDevicesOutput),
            ("devices_GetDeviceDetails", GetDeviceDetails, devices::GetDeviceDetailsAction, devices::GetDeviceDetailsOutput),
            ("devices_CreateDevice", CreateDevice, devices::CreateDeviceAction, devices::CreateDeviceOutput),
            ("devices_DeleteDevice", DeleteDevice, devices::DeleteDeviceAction, outputs::Empty),
            ("devices_SetDeviceName", SetDeviceName, devices::SetDeviceNameAction, outputs::Empty),
            ("devices_SetDeviceMetadata", SetDeviceMetadata, devices::SetDeviceMetadataAction, outputs::Empty),
            ("devices_IssueDeviceToken", IssueDeviceToken, devices::IssueDeviceTokenAction, devices::IssueDeviceTokenOutput),
            ("devices_ValidateDeviceToken", ValidateDeviceToken, devices::ValidateDeviceTokenAction, devices::ValidateDeviceTokenOutput),
            ("devices_AuthenticateDevice", AuthenticateDevice, devices::AuthenticateDeviceAction, devices::AuthenticateDeviceOutput),
            // ## Device Certificates
            ("devices_AddDeviceCertificate", AddDeviceCertificate, devices::AddDeviceCertificateAction, devices::AddDeviceCertificateOutput),
            ("devices_DeleteDeviceCertificate", DeleteDeviceCertificate, devices::DeleteDeviceCertificateAction, outputs::Empty),
            ("devices_SetDeviceCertificateStatus", SetDeviceCertificateStatus, devices::SetDeviceCertificateStatusAction, outputs::Empty),
            // ## Device Connections
            ("devices_RegisterDeviceConnection", RegisterDeviceConnection, devices::RegisterDeviceConnectionAction, devices::RegisterDeviceConnectionOutput),
            ("devices_UnregisterDeviceConnection", UnregisterDeviceConnection, devices::UnregisterDeviceConnectionAction, outputs::Empty),
            // # HTTP Proxy
            ("devices_IssueDeviceHttpProxyToken", IssueDeviceHttpProxyToken, devices::IssueDeviceHttpProxyTokenAction, devices::IssueDeviceHttpProxyTokenOutput),
            ("devices_ValidateDeviceHttpProxyToken", ValidateDeviceHttpProxyToken, devices::ValidateDeviceHttpProxyTokenAction, devices::ValidateDeviceHttpProxyTokenOutput),
            // # Device Events
            ("devices_PublishDeviceEvents", PublishDeviceEvents, devices::PublishDeviceEventsAction, outputs::Empty),
            ("devices_QueryDeviceEvents", QueryDeviceEvents, devices::QueryDeviceEventsAction, devices::QueryDeviceEventsOutput),

            // # Organizations
            ("organizations_QueryOrganizations", QueryOrganizations, organizations::QueryOrganizationsAction, organizations::QueryOrganizationsOutput),
            ("organizations_GetOrganizationDetails", GetOrganizationDetails, organizations::GetOrganizationDetailsAction, organizations::GetOrganizationDetailsOutput),
            ("organizations_CreateOrganization", CreateOrganization, organizations::CreateOrganizationAction, organizations::CreateOrganizationOutput),
            ("organizations_DeleteOrganization", DeleteOrganization, organizations::DeleteOrganizationAction, outputs::Empty),
            ("organizations_QueryOrganizationProjects", QueryOrganizationProjects, organizations::QueryOrganizationProjectsAction, organizations::QueryOrganizationProjectsOutput),
            ("organizations_QueryOrganizationRepositories", QueryOrganizationRepositories, organizations::QueryOrganizationRepositoriesAction, organizations::QueryOrganizationRepositoriesOutput),
            ("organizations_QueryOrganizationMembers", QueryOrganizationMembers, organizations::QueryOrganizationMembersAction, organizations::QueryOrganizationMembersOutput),
            ("organizations_QueryOrganizationInvitations", QueryOrganizationInvitations, organizations::QueryOrganizationInvitationsAction, organizations::QueryOrganizationInvitationsOutput),
            ("organizations_AddOrganizationMember", AddOrganizationMember, organizations::AddOrganizationMemberAction, outputs::Empty),
            ("organizations_RemoveOrganizationMember", RemoveOrganizationMember, organizations::RemoveOrganizationMemberAction, outputs::Empty),
            ("organizations_InviteOrganizationMember", InviteOrganizationMember, organizations::InviteOrganizationMemberAction, organizations::InviteOrganizationMemberOutput),
            ("organizations_DeleteOrganizationInvitation", DeleteOrganizationInvitation, organizations::DeleteOrganizationInvitationAction, outputs::Empty),

            // # Repositories
            ("repositories_ResolveRepositoryName", ResolveRepositoryName, repositories::ResolveRepositoryNameAction, repositories::ResolveRepositoryNameOutput),
            ("repositories_GetRepositoryDetails", GetRepositoryDetails, repositories::GetRepositoryDetailsAction, repositories::GetRepositoryDetailsOutput),
            ("repositories_CreateRepository", CreateRepository, repositories::CreateRepositoryAction, repositories::CreateRepositoryOutput),
            ("repositories_DeleteRepository", DeleteRepository, repositories::DeleteRepositoryAction, outputs::Empty),
            ("repositories_SetRepositoryVisibility", SetRepositoryVisibility, repositories::SetRepositoryVisibilityAction, outputs::Empty),
            ("repositories_QueryRepositoryPackages", QueryRepositoryPackages, repositories::QueryRepositoryPackagesAction, repositories::QueryRepositoryPackagesOutput),
            ("repositories_QueryRepositoryAssets", QueryRepositoryAssets, repositories::QueryRepositoryAssetsAction, repositories::QueryRepositoryAssetsOutput),
            ("repositories_QueryRepositoryMembers", QueryRepositoryMembers, repositories::QueryRepositoryMembersAction, repositories::QueryRepositoryMembersOutput),
            ("repositories_QueryRepositoryInvitations", QueryRepositoryInvitations, repositories::QueryRepositoryInvitationsAction, repositories::QueryRepositoryInvitationsOutput),
            ("repositories_QueryRepositoryProjects", QueryRepositoryProjects, repositories::QueryRepositoryProjectsAction, repositories::QueryRepositoryProjectsOutput),
            ("repositories_AddRepositoryMember", AddRepositoryMember, repositories::AddRepositoryMemberAction, outputs::Empty),
            ("repositories_RemoveRepositoryMember", RemoveRepositoryMember, repositories::RemoveRepositoryMemberAction, outputs::Empty),
            ("repositories_InviteRepositoryMember", InviteRepositoryMember, repositories::InviteRepositoryMemberAction, repositories::InviteRepositoryMemberOutput),
            ("repositories_DeleteRepositoryInvitation", DeleteRepositoryInvitation, repositories::DeleteRepositoryInvitationAction, outputs::Empty),
            // ## Packages
            ("repositories_ResolvePackageByPath", ResolvePackageByPath, repositories::ResolvePackageByPathAction, repositories::ResolvePackageByPathOutput),
            ("repositories_GetPackageDetails", GetPackageDetails, repositories::GetPackageDetailsAction, repositories::GetPackageDetailsOutput),
            ("repositories_CreatePackage", CreatePackage, repositories::CreatePackageAction, repositories::CreatePackageOutput),
            ("repositories_DeletePackage", DeletePackage, repositories::DeletePackageAction, outputs::Empty),
            ("repositories_QueryPackageVersions", QueryPackageVersions, repositories::QueryPackageVersionsAction, repositories::QueryPackageVersionsOutput),
            // ## Package Versions
            ("repositories_ResolvePackageVersionByPath", ResolvePackageVersionByPath, repositories::ResolvePackageVersionByPathAction, repositories::ResolvePackageVersionByPathOutput),
            ("repositories_GetPackageVersionDetails", GetPackageVersionDetails, repositories::GetPackageVersionDetailsAction, repositories::GetPackageVersionDetailsOutput),
            ("repositories_CreatePackageVersion", CreatePackageVersion, repositories::CreatePackageVersionAction, repositories::CreatePackageVersionOutput),
            ("repositories_DeletePackageVersion", DeletePackageVersion, repositories::DeletePackageVersionAction, outputs::Empty),
            ("repositories_AddPackageVersionAsset", AddPackageVersionAsset, repositories::AddPackageVersionAssetAction, repositories::AddPackageVersionAssetOutput),
            ("repositories_RemovePackageVersionAsset", RemovePackageVersionAsset, repositories::RemovePackageVersionAssetAction, outputs::Empty),
            ("repositories_TagPackageVersion", TagPackageVersion, repositories::TagPackageVersionAction, outputs::Empty),
            ("repositories_UntagPackageVersion", UntagPackageVersion, repositories::UntagPackageVersionAction, outputs::Empty),
            ("repositories_ResolvePackageVersionAssetByPath", ResolvePackageVersionAssetByPath, repositories::ResolvePackageVersionAssetByPathAction, repositories::ResolvePackageVersionAssetByPathOutput),
            // ## S3 Config
            ("repositories_SetRepositoryS3Config", SetRepositoryS3Credentials, repositories::SetRepositoryS3ConfigAction, outputs::Empty),
            ("repositories_GetRepositoryS3Config", GetRepositoryS3Credentials, repositories::GetRepositoryS3ConfigAction, repositories::GetRepositoryS3ConfigOutput),
            // ## Assets
            ("repositories_GetAssetDetails", GetAssetDetails, repositories::GetAssetDetailsAction, repositories::GetAssetDetailsOutput),
            ("repositories_CreateAsset", CreateAsset, repositories::CreateAssetAction, repositories::CreateAssetOutput),
            ("repositories_DeleteAsset", DeleteAsset, repositories::DeleteAssetAction, outputs::Empty),
            ("repositories_IssueAssetDownloadUrl", IssueAssetDownloadUrl, repositories::IssueAssetDownloadUrlAction, repositories::IssueAssetDownloadUrlOutput),
            ("repositories_IssueAssetUploadUrl", IssueAssetUploadUrl, repositories::IssueAssetUploadUrlAction, repositories::IssueAssetUploadUrlOutput),
            // # Audit Log
            ("repositories_QueryAuditLogEvents", QueryRepositoryAuditLogEvents, repositories::QueryAuditLogEventsAction, repositories::QueryAuditLogEventsOutput),

            // # Audit Log
            ("audit_QueryAuditLogEvents", QueryAuditLogEvents, audit::QueryAuditLogEventsAction, audit::QueryAuditLogEventsOutput),
            ("audit_QueryAuditLogActions", QueryAuditLogActions, audit::QueryAuditLogActionsAction, audit::QueryAuditLogActionsOutput),

            // # Jobs
            ("jobs_QueryJobs", QueryJobs, jobs::QueryJobsAction, jobs::QueryJobsOutput),

            // # Instance
            ("instance_GetInstanceStatistics", GetInstanceStatistics, instance::GetInstanceStatisticsAction, instance::GetInstanceStatisticsOutput),
            ("instance_GetInstanceSettingsRaw", GetInstanceSettingsRaw, instance::GetInstanceSettingsRawAction, instance::GetInstanceSettingsRawOutput),
            ("instance_SetInstanceSettingRaw", SetInstanceSettingRaw, instance::SetInstanceSettingRawAction, outputs::Empty),

            // # Cluster
            ("cluster_GetClusterDetails", GetClusterDetails, cluster::GetClusterDetailsAction, cluster::GetClusterDetailsOutput),
            // ## Cluster Nodes
            ("cluster_RegisterClusterNode", RegisterClusterNode, cluster::RegisterClusterNodeAction, cluster::RegisterClusterNodeOutput),
            ("cluster_ReportClusterNodeHeartbeat", ReportClusterNodeHeartbeat, cluster::ReportClusterNodeHeartbeatAction, outputs::Empty),
            ("cluster_CleanupInactiveClusterNodes", CleanupInactiveClusterNodes, cluster::CleanupInactiveClusterNodesAction, outputs::Empty),

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
            ("projects_MemberAdded", projects::ProjectMemberAddedEvent, { user_id, project_id }),
            ("projects_MemberRemoved", projects::ProjectMemberRemovedEvent, { user_id, project_id }),
            ("projects_InvitationCreated", projects::ProjectInvitationCreatedEvent, { project_id }),
            ("projects_DeploymentTokenCreated", projects::DeploymentTokenCreatedEvent, { project_id }),
            ("projects_DeploymentTokenDeleted", projects::DeploymentTokenDeletedEvent, { project_id }),
            ("projects_DeploymentTokenFlagsChanged", projects::DeploymentTokenFlagsChangedEvent, { project_id }),
            ("projects_RepositoryAdded", projects::ProjectRepositoryAddedEvent, { project_id, repository_id }),
            ("projects_RepositoryRemoved", projects::ProjectRepositoryRemovedEvent, { project_id, repository_id }),

            // # Devices
            ("devices_Created", devices::DeviceCreatedEvent, { project_id }),
            ("devices_Deleted", devices::DeviceDeletedEvent, { project_id }),
            ("devices_CertificateAdded", devices::DeviceCertificateAddedEvent, { project_id }),
            ("devices_CertificateDeleted", devices::DeviceCertificateDeletedEvent, { project_id }),
            ("devices_CertificateStatusChanged", devices::DeviceCertificateStatusChangedEvent, { project_id }),

            // # Repositories
            ("repositories_InvitationCreated", repositories::RepositoryInvitationCreatedEvent, { repository_id }),
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
