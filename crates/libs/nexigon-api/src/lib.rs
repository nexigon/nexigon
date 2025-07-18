use std::any::Any;

use serde::Serialize;
use serde::de::DeserializeOwned;

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
            ("users_QueryUserTokens", QueryUserTokens, users::QueryUserTokensAction, users::QueryUserTokensOutput),
            ("users_QueryUserProjects", QueryUserProjects, users::QueryUserProjectsAction, users::QueryUserProjectsOutput),
            ("users_QueryUserRepositories", QueryUserRepositories, users::QueryUserRepositoriesAction, users::QueryUserRepositoriesOutput),
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

            // # Projects
            ("projects_QueryProjects", QueryProjects, projects::QueryProjectsAction, projects::QueryProjectsOutput),
            ("projects_GetProjectDetails", GetProjectDetails, projects::GetProjectDetailsAction, projects::GetProjectDetailsOutput),
            ("projects_CreateProject", CreateProject, projects::CreateProjectAction, projects::CreateProjectOutput),
            ("projects_DeleteProject", DeleteProject, projects::DeleteProjectAction, outputs::Empty),
            ("projects_QueryProjectDevices", QueryProjectDevices, projects::QueryProjectDevicesAction, projects::QueryProjectDevicesOutput),
            ("projects_QueryProjectMembers", QueryProjectMembers, projects::QueryProjectMembersAction, projects::QueryProjectMembersOutput),
            ("projects_QueryProjectDeploymentTokens", QueryProjectDeploymentTokens, projects::QueryProjectDeploymentTokensAction, projects::QueryProjectDeploymentTokensOutput),
            ("projects_AddProjectMember", AddProjectMember, projects::AddProjectMemberAction, outputs::Empty),
            ("projects_RemoveProjectMember", RemoveProjectMember, projects::RemoveProjectMemberAction, outputs::Empty),
            // ## Deployment Tokens
            ("projects_CreateDeploymentToken", CreateDeploymentToken, projects::CreateDeploymentTokenAction, projects::CreateDeploymentTokenOutput),
            ("projects_DeleteDeploymentToken", DeleteDeploymentToken, projects::DeleteDeploymentTokenAction, outputs::Empty),

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

            // # Repositories
            ("repositories_ResolveRepositoryName", ResolveRepositoryName, repositories::ResolveRepositoryNameAction, repositories::ResolveRepositoryNameOutput),
            ("repositories_GetRepositoryDetails", GetRepositoryDetails, repositories::GetRepositoryDetailsAction, repositories::GetRepositoryDetailsOutput),
            ("repositories_CreateRepository", CreateRepository, repositories::CreateRepositoryAction, repositories::CreateRepositoryOutput),
            ("repositories_DeleteRepository", DeleteRepository, repositories::DeleteRepositoryAction, outputs::Empty),
            ("repositories_SetRepositoryVisibility", SetRepositoryVisibility, repositories::SetRepositoryVisibilityAction, outputs::Empty),
            ("repositories_QueryRepositoryPackages", QueryRepositoryPackages, repositories::QueryRepositoryPackagesAction, repositories::QueryRepositoryPackagesOutput),
            ("repositories_QueryRepositoryAssets", QueryRepositoryAssets, repositories::QueryRepositoryAssetsAction, repositories::QueryRepositoryAssetsOutput),
            ("repositories_QueryRepositoryMembers", QueryRepositoryMembers, repositories::QueryRepositoryMembersAction, repositories::QueryRepositoryMembersOutput),
            ("repositories_AddRepositoryMember", AddRepositoryMember, repositories::AddRepositoryMemberAction, outputs::Empty),
            ("repositories_RemoveRepositoryMember", RemoveRepositoryMember, repositories::RemoveRepositoryMemberAction, outputs::Empty),
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
            // ## Assets
            ("repositories_GetAssetDetails", GetAssetDetails, repositories::GetAssetDetailsAction, repositories::GetAssetDetailsOutput),
            ("repositories_CreateAsset", CreateAsset, repositories::CreateAssetAction, repositories::CreateAssetOutput),
            ("repositories_DeleteAsset", DeleteAsset, repositories::DeleteAssetAction, outputs::Empty),
            ("repositories_IssueAssetDownloadUrl", IssueAssetDownloadUrl, repositories::IssueAssetDownloadUrlAction, repositories::IssueAssetDownloadUrlOutput),
            ("repositories_IssueAssetUploadUrl", IssueAssetUploadUrl, repositories::IssueAssetUploadUrlAction, repositories::IssueAssetUploadUrlOutput),

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
