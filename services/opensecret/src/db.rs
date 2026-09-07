use crate::models::account_deletion::{
    AccountDeletionError, AccountDeletionRequest, NewAccountDeletionRequest,
};
use crate::models::app_data_migrations::{
    AppDataMigration, AppDataMigrationError, NewAppDataMigration,
};
use crate::models::enclave_secrets::{EnclaveSecret, EnclaveSecretError, NewEnclaveSecret};
use crate::models::invite_codes::{InviteCode, InviteCodeError, NewInviteCode};
use crate::models::oauth::{
    NewOAuthProvider, NewUserOAuthConnection, OAuthError, OAuthProvider, UserOAuthConnection,
};
use crate::models::org_memberships::NewOrgMembership;
use crate::models::org_memberships::{OrgMembership, OrgMembershipError, OrgMembershipWithUser};
use crate::models::org_project_secrets::{
    NewOrgProjectSecret, OrgProjectSecret, OrgProjectSecretError,
};
use crate::models::org_projects::{NewOrgProject, OrgProject, OrgProjectError};
use crate::models::orgs::{NewOrg, Org, OrgError};
use crate::models::password_reset::{
    NewPasswordResetRequest, PasswordResetError, PasswordResetRequest,
};
use crate::models::platform_email_verification::{
    NewPlatformEmailVerification, PlatformEmailVerification, PlatformEmailVerificationError,
};
use crate::models::platform_invite_codes::{PlatformInviteCode, PlatformInviteCodeError};
use crate::models::platform_password_reset::{
    NewPlatformPasswordResetRequest, PlatformPasswordResetError, PlatformPasswordResetRequest,
};
use crate::models::platform_users::{NewPlatformUser, PlatformUser, PlatformUserError};
use crate::models::project_settings::OAuthSettings;
use crate::models::project_settings::{
    EmailSettings, NewProjectSetting, ProjectSetting, ProjectSettingError, SettingCategory,
};
use crate::models::responses::{
    validate_conversation_project_limit, AssistantMessage, Conversation, ConversationProject,
    ConversationProjectFilter, NewAssistantMessage, NewConversation, NewConversationProject,
    NewReasoningItem, NewResponse, NewToolCall, NewToolOutput, NewUserInstruction, NewUserMessage,
    PersistedResponseWithToolItems, ProjectInstructionUpdate, RawThreadMessage,
    RawThreadMessageMetadata, ReasoningItem, Response, ResponseStatus, ResponsesError, ToolCall,
    ToolOutput, UserInstruction, UserMessage,
};
use crate::models::schema::users;
use crate::models::token_usage::{NewTokenUsage, TokenUsage, TokenUsageError};
use crate::models::user_api_keys::{NewUserApiKey, UserApiKey, UserApiKeyError};
use crate::models::user_seed_wrappings::{
    NewUserSeedWrapping, UserSeedWrapping, UserSeedWrappingError,
};
use crate::models::users::{NewUser, User, UserError};
use crate::models::{
    email_verification::{EmailVerification, EmailVerificationError, NewEmailVerification},
    org_memberships::OrgRole,
};
use crate::seed_wrapping::CredentialKind;
use chrono::{DateTime, Utc};
use diesel::{
    pg::PgConnection,
    r2d2::{ConnectionManager, Pool},
};
use diesel::{Connection, ExpressionMethods, QueryDsl, RunQueryDsl};
use std::sync::Arc;
use tracing::{debug, error, info};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum DBError {
    #[error("Database connection error")]
    ConnectionError,
    #[error("Database query error: {0}")]
    QueryError(#[from] diesel::result::Error),
    #[error("User error: {0}")]
    UserError(#[from] UserError),
    #[error("User not found")]
    UserNotFound,
    #[error("Enclave secret error: {0}")]
    EnclaveSecretError(#[from] EnclaveSecretError),
    #[error("Email verification error: {0}")]
    EmailVerificationError(#[from] EmailVerificationError),
    #[error("Email verification not found")]
    EmailVerificationNotFound,
    #[error("Password reset error: {0}")]
    PasswordResetError(#[from] PasswordResetError),
    #[error("Password reset request not found")]
    PasswordResetRequestNotFound,
    #[error("Account deletion error: {0}")]
    AccountDeletionError(#[from] AccountDeletionError),
    #[error("Account deletion request not found")]
    AccountDeletionRequestNotFound,
    #[error("Encryption error: {0}")]
    EncryptionError(#[from] crate::encrypt::EncryptError),
    #[error("OAuth error: {0}")]
    OAuthError(#[from] OAuthError),
    #[error("Token usage error: {0}")]
    TokenUsageError(#[from] TokenUsageError),
    #[error("User API key error: {0}")]
    UserApiKeyError(#[from] UserApiKeyError),
    #[error("Org error: {0}")]
    OrgError(#[from] OrgError),
    #[error("Org not found")]
    OrgNotFound,
    #[error("Org project error: {0}")]
    OrgProjectError(#[from] OrgProjectError),
    #[error("Org project not found")]
    OrgProjectNotFound,
    #[error("Org project secret error: {0}")]
    OrgProjectSecretError(#[from] OrgProjectSecretError),
    #[error("Org project secret not found")]
    OrgProjectSecretNotFound,
    #[error("Invite code error: {0}")]
    InviteCodeError(#[from] InviteCodeError),
    #[error("Invite code not found")]
    InviteCodeNotFound,
    #[error("Platform invite code error: {0}")]
    PlatformInviteCodeError(#[from] PlatformInviteCodeError),
    #[error("Platform invite code not found")]
    PlatformInviteCodeNotFound,
    #[error("Invalid invite code")]
    InvalidInviteCode,
    #[error("Platform user error: {0}")]
    PlatformUserError(#[from] PlatformUserError),
    #[error("Platform user not found")]
    PlatformUserNotFound,
    #[error("Platform email verification error: {0}")]
    PlatformEmailVerificationError(#[from] PlatformEmailVerificationError),
    #[error("Platform email verification not found")]
    PlatformEmailVerificationNotFound,
    #[error("Platform password reset error: {0}")]
    PlatformPasswordResetError(#[from] PlatformPasswordResetError),
    #[error("Platform password reset request not found")]
    PlatformPasswordResetRequestNotFound,
    #[error("Org membership error: {0}")]
    OrgMembershipError(#[from] OrgMembershipError),
    #[error("Org membership not found")]
    OrgMembershipNotFound,
    #[error("Project setting error: {0}")]
    ProjectSettingError(#[from] ProjectSettingError),
    #[error("Project setting not found")]
    ProjectSettingNotFound,
    #[error("Responses API error: {0}")]
    ResponsesError(#[from] crate::models::responses::ResponsesError),
    #[error("User seed wrapping error: {0}")]
    UserSeedWrappingError(#[from] UserSeedWrappingError),
    #[error("App data migration error: {0}")]
    AppDataMigrationError(#[from] AppDataMigrationError),
    #[error("Credential state changed during update")]
    StaleCredentialState,
}

#[allow(dead_code)]
pub trait DBConnection {
    fn create_user(&self, new_user: NewUser) -> Result<User, DBError>;
    fn get_user_by_uuid(&self, uuid: Uuid) -> Result<User, DBError>;
    fn get_user_by_email(&self, email: String, project_id: i32) -> Result<User, DBError>;
    fn create_user_seed_wrapping(
        &self,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<UserSeedWrapping, DBError>;
    fn upsert_user_seed_wrapping(
        &self,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<UserSeedWrapping, DBError>;
    fn get_user_seed_wrappings_for_user_and_kind(
        &self,
        user_id: Uuid,
        credential_kind: &str,
    ) -> Result<Vec<UserSeedWrapping>, DBError>;
    fn get_user_seed_wrapping_by_credential(
        &self,
        user_id: Uuid,
        credential_kind: &str,
        credential_lookup_hash: &[u8],
        wrapping_version: i16,
    ) -> Result<Option<UserSeedWrapping>, DBError>;
    fn delete_user_seed_wrappings_for_user(&self, user_id: Uuid) -> Result<usize, DBError>;
    fn delete_user_seed_wrappings_for_user_and_kind(
        &self,
        user_id: Uuid,
        credential_kind: &str,
    ) -> Result<usize, DBError>;
    fn get_app_data_migration(&self, name: &str) -> Result<Option<AppDataMigration>, DBError>;
    fn app_data_migration_exists(&self, name: &str) -> Result<bool, DBError>;
    fn create_app_data_migration(
        &self,
        new_migration: NewAppDataMigration,
    ) -> Result<AppDataMigration, DBError>;
    fn get_pool(&self) -> &diesel::r2d2::Pool<diesel::r2d2::ConnectionManager<PgConnection>>;
    fn create_enclave_secret(&self, new_secret: NewEnclaveSecret)
        -> Result<EnclaveSecret, DBError>;
    fn get_enclave_secret_by_id(&self, id: i32) -> Result<Option<EnclaveSecret>, DBError>;
    fn get_enclave_secret_by_key(&self, key: &str) -> Result<Option<EnclaveSecret>, DBError>;
    fn get_all_enclave_secrets(&self) -> Result<Vec<EnclaveSecret>, DBError>;
    fn update_enclave_secret(&self, secret: &EnclaveSecret) -> Result<(), DBError>;
    fn delete_enclave_secret(&self, secret: &EnclaveSecret) -> Result<(), DBError>;
    fn create_email_verification(
        &self,
        new_verification: NewEmailVerification,
    ) -> Result<EmailVerification, DBError>;
    fn get_email_verification_by_id(&self, id: i32) -> Result<EmailVerification, DBError>;
    fn get_email_verification_by_user_id(
        &self,
        user_id: Uuid,
    ) -> Result<EmailVerification, DBError>;
    fn get_email_verification_by_code(&self, code: Uuid) -> Result<EmailVerification, DBError>;
    fn update_email_verification(&self, verification: &EmailVerification) -> Result<(), DBError>;
    fn delete_email_verification(&self, verification: &EmailVerification) -> Result<(), DBError>;
    fn verify_email(&self, verification: &mut EmailVerification) -> Result<(), DBError>;
    fn create_password_reset_request(
        &self,
        new_request: NewPasswordResetRequest,
    ) -> Result<PasswordResetRequest, DBError>;
    fn get_password_reset_request_by_user_id_and_code(
        &self,
        user_id: Uuid,
        encrypted_code: Vec<u8>,
    ) -> Result<Option<PasswordResetRequest>, DBError>;
    fn update_user_password_and_seed_wrap(
        &self,
        user: &User,
        expected_password_enc: &[u8],
        new_password_enc: Vec<u8>,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<(), DBError>;
    fn mark_password_reset_as_complete(
        &self,
        request: &PasswordResetRequest,
    ) -> Result<(), DBError>;
    fn complete_destructive_password_reset(
        &self,
        user: &User,
        reset_request: &PasswordResetRequest,
        new_password_enc: Vec<u8>,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<(), DBError>;

    // Account Deletion methods
    fn create_account_deletion_request(
        &self,
        new_request: NewAccountDeletionRequest,
    ) -> Result<AccountDeletionRequest, DBError>;
    fn get_account_deletion_request_by_user_id_and_code(
        &self,
        user_id: Uuid,
        encrypted_code: Vec<u8>,
    ) -> Result<Option<AccountDeletionRequest>, DBError>;
    fn mark_account_deletion_as_complete(
        &self,
        request: &AccountDeletionRequest,
    ) -> Result<(), DBError>;
    fn delete_user(&self, user: &User) -> Result<(), DBError>;
    fn mark_and_delete_user(
        &self,
        user: &User,
        deletion_request: &AccountDeletionRequest,
    ) -> Result<(), DBError>;

    // OAuth Provider methods
    fn create_oauth_provider(
        &self,
        new_provider: NewOAuthProvider,
    ) -> Result<OAuthProvider, DBError>;
    fn get_oauth_provider_by_id(&self, id: i32) -> Result<Option<OAuthProvider>, DBError>;
    fn get_oauth_provider_by_name(&self, name: &str) -> Result<Option<OAuthProvider>, DBError>;
    fn get_all_oauth_providers(&self) -> Result<Vec<OAuthProvider>, DBError>;
    fn update_oauth_provider(&self, provider: &OAuthProvider) -> Result<(), DBError>;
    fn delete_oauth_provider(&self, provider: &OAuthProvider) -> Result<(), DBError>;

    // User OAuth Connection methods
    fn create_user_oauth_connection(
        &self,
        new_connection: NewUserOAuthConnection,
    ) -> Result<UserOAuthConnection, DBError>;
    fn get_user_oauth_connection_by_id(
        &self,
        id: i32,
    ) -> Result<Option<UserOAuthConnection>, DBError>;
    fn get_user_oauth_connection_by_user_and_provider(
        &self,
        user_id: Uuid,
        provider_id: i32,
    ) -> Result<Option<UserOAuthConnection>, DBError>;
    fn get_project_user_oauth_connection_by_provider_subject(
        &self,
        provider_id: i32,
        provider_user_id: &str,
        project_id: i32,
    ) -> Result<Option<UserOAuthConnection>, DBError>;
    fn get_all_user_oauth_connections_for_user(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<UserOAuthConnection>, DBError>;
    fn update_user_oauth_connection(&self, connection: &UserOAuthConnection)
        -> Result<(), DBError>;
    fn delete_user_oauth_connection(&self, connection: &UserOAuthConnection)
        -> Result<(), DBError>;

    fn create_token_usage(&self, new_usage: NewTokenUsage) -> Result<TokenUsage, DBError>;

    fn update_user(&self, user: &User) -> Result<(), DBError>;
    // New org-related methods
    fn create_org(&self, new_org: NewOrg) -> Result<Org, DBError>;
    fn get_org_by_id(&self, id: i32) -> Result<Org, DBError>;
    fn get_org_by_uuid(&self, uuid: Uuid) -> Result<Org, DBError>;
    fn get_org_by_name(&self, name: &str) -> Result<Option<Org>, DBError>;
    fn get_all_orgs(&self) -> Result<Vec<Org>, DBError>;
    fn update_org(&self, org: &Org) -> Result<(), DBError>;
    fn delete_org(&self, org: &Org) -> Result<(), DBError>;

    // Org project methods
    fn create_org_project(&self, new_project: NewOrgProject) -> Result<OrgProject, DBError>;
    fn get_org_project_by_id(&self, id: i32) -> Result<OrgProject, DBError>;
    fn get_org_project_by_uuid(&self, uuid: Uuid) -> Result<OrgProject, DBError>;
    fn get_org_project_by_client_id(&self, client_id: Uuid) -> Result<OrgProject, DBError>;
    fn get_org_project_by_name_and_org(
        &self,
        name: &str,
        org_id: i32,
    ) -> Result<Option<OrgProject>, DBError>;
    fn get_all_org_projects_for_org(&self, org_id: i32) -> Result<Vec<OrgProject>, DBError>;
    fn get_active_org_projects_for_org(&self, org_id: i32) -> Result<Vec<OrgProject>, DBError>;
    fn update_org_project(&self, project: &OrgProject) -> Result<(), DBError>;
    fn delete_org_project(&self, project: &OrgProject) -> Result<(), DBError>;

    // Org project secret methods
    fn create_org_project_secret(
        &self,
        new_secret: NewOrgProjectSecret,
    ) -> Result<OrgProjectSecret, DBError>;
    fn get_org_project_secret_by_id(&self, id: i32) -> Result<OrgProjectSecret, DBError>;
    fn get_org_project_secret_by_key_name_and_project(
        &self,
        key_name: &str,
        project_id: i32,
    ) -> Result<Option<OrgProjectSecret>, DBError>;
    fn get_all_org_project_secrets_for_project(
        &self,
        project_id: i32,
    ) -> Result<Vec<OrgProjectSecret>, DBError>;
    fn update_org_project_secret(&self, secret: &OrgProjectSecret) -> Result<(), DBError>;
    fn delete_org_project_secret(&self, secret: &OrgProjectSecret) -> Result<(), DBError>;

    // Invite code methods
    fn create_invite_code(&self, new_invite: NewInviteCode) -> Result<InviteCode, DBError>;
    fn get_invite_code_by_id(&self, id: i32) -> Result<InviteCode, DBError>;
    fn get_invite_code_by_code(&self, code: Uuid) -> Result<InviteCode, DBError>;
    fn get_invite_code_by_email_and_org(
        &self,
        email: &str,
        org_id: i32,
    ) -> Result<Option<InviteCode>, DBError>;
    fn get_all_invite_codes_for_org(&self, org_id: i32) -> Result<Vec<InviteCode>, DBError>;
    fn mark_invite_code_as_used(&self, invite: &InviteCode) -> Result<(), DBError>;
    fn update_invite_code(&self, invite: &InviteCode) -> Result<(), DBError>;
    fn delete_invite_code(&self, invite: &InviteCode) -> Result<(), DBError>;

    // Platform user methods
    fn create_platform_user(&self, new_user: NewPlatformUser) -> Result<PlatformUser, DBError>;
    fn get_platform_user_by_id(&self, id: i32) -> Result<PlatformUser, DBError>;
    fn get_platform_user_by_uuid(&self, uuid: Uuid) -> Result<PlatformUser, DBError>;
    fn get_platform_user_by_email(&self, email: &str) -> Result<Option<PlatformUser>, DBError>;
    fn update_platform_user(&self, user: &PlatformUser) -> Result<(), DBError>;
    fn update_platform_user_password(
        &self,
        user: &PlatformUser,
        new_password_enc: Vec<u8>,
    ) -> Result<(), DBError>;

    // Org membership methods
    fn create_org_membership(
        &self,
        new_membership: NewOrgMembership,
    ) -> Result<OrgMembership, DBError>;
    fn get_org_membership_by_platform_user_and_org(
        &self,
        platform_user_id: Uuid,
        org_id: i32,
    ) -> Result<OrgMembership, DBError>;

    fn get_org_membership_by_platform_user_and_org_with_user(
        &self,
        platform_user_id: Uuid,
        org_id: i32,
    ) -> Result<OrgMembershipWithUser, DBError>;
    fn get_all_org_memberships_for_platform_user(
        &self,
        platform_user_id: Uuid,
    ) -> Result<Vec<OrgMembership>, DBError>;
    fn get_all_org_memberships_for_org(&self, org_id: i32) -> Result<Vec<OrgMembership>, DBError>;
    fn get_all_org_memberships_with_users_for_org(
        &self,
        org_id: i32,
    ) -> Result<Vec<OrgMembershipWithUser>, DBError>;
    fn update_org_membership(&self, membership: &OrgMembership) -> Result<(), DBError>;
    fn delete_org_membership(&self, membership: &OrgMembership) -> Result<(), DBError>;
    fn update_membership_role(
        &self,
        membership: &mut OrgMembership,
        new_role: OrgRole,
    ) -> Result<(), DBError>;
    fn delete_membership_with_owner_check(&self, membership: &OrgMembership)
        -> Result<(), DBError>;

    // project-scoped methods
    fn get_users_for_project(
        &self,
        project_id: i32,
        page: Option<i64>,
        per_page: Option<i64>,
    ) -> Result<(Vec<User>, i64), DBError>;

    fn create_org_with_owner(&self, new_org: NewOrg, owner_id: Uuid) -> Result<Org, DBError>;

    fn accept_invite_transaction(
        &self,
        invite: &InviteCode,
        new_membership: NewOrgMembership,
    ) -> Result<OrgMembership, DBError>;

    // Project settings methods
    fn get_project_settings(
        &self,
        project_id: i32,
        category: SettingCategory,
    ) -> Result<Option<ProjectSetting>, DBError>;

    fn update_project_settings(
        &self,
        project_id: i32,
        category: SettingCategory,
        settings: serde_json::Value,
    ) -> Result<ProjectSetting, DBError>;

    fn get_project_email_settings(&self, project_id: i32)
        -> Result<Option<EmailSettings>, DBError>;

    fn update_project_email_settings(
        &self,
        project_id: i32,
        settings: EmailSettings,
    ) -> Result<ProjectSetting, DBError>;

    fn get_project_oauth_settings(&self, project_id: i32)
        -> Result<Option<OAuthSettings>, DBError>;

    fn update_project_oauth_settings(
        &self,
        project_id: i32,
        settings: OAuthSettings,
    ) -> Result<ProjectSetting, DBError>;

    // Platform email verification methods
    fn create_platform_email_verification(
        &self,
        new_verification: NewPlatformEmailVerification,
    ) -> Result<PlatformEmailVerification, DBError>;

    fn get_platform_email_verification_by_id(
        &self,
        id: i32,
    ) -> Result<PlatformEmailVerification, DBError>;

    fn get_platform_email_verification_by_platform_user_id(
        &self,
        platform_user_id: Uuid,
    ) -> Result<PlatformEmailVerification, DBError>;

    fn get_platform_email_verification_by_code(
        &self,
        code: Uuid,
    ) -> Result<PlatformEmailVerification, DBError>;

    fn update_platform_email_verification(
        &self,
        verification: &PlatformEmailVerification,
    ) -> Result<(), DBError>;

    fn delete_platform_email_verification(
        &self,
        verification: &PlatformEmailVerification,
    ) -> Result<(), DBError>;

    fn verify_platform_email(
        &self,
        verification: &mut PlatformEmailVerification,
    ) -> Result<(), DBError>;

    // Platform password reset methods
    fn create_platform_password_reset_request(
        &self,
        new_request: NewPlatformPasswordResetRequest,
    ) -> Result<PlatformPasswordResetRequest, DBError>;

    fn get_platform_password_reset_request_by_user_id_and_code(
        &self,
        user_id: Uuid,
        encrypted_code: Vec<u8>,
    ) -> Result<Option<PlatformPasswordResetRequest>, DBError>;

    fn mark_platform_password_reset_as_complete(
        &self,
        request: &PlatformPasswordResetRequest,
    ) -> Result<(), DBError>;

    // User API key methods
    fn create_user_api_key(&self, new_key: NewUserApiKey) -> Result<UserApiKey, DBError>;
    fn get_user_api_key_by_id(&self, id: i32) -> Result<Option<UserApiKey>, DBError>;
    fn get_user_api_key_by_hash(&self, key_hash: &str) -> Result<Option<UserApiKey>, DBError>;
    fn get_user_by_api_key_hash(&self, key_hash: &str) -> Result<Option<User>, DBError>;
    fn get_all_user_api_keys_for_user(&self, user_id: Uuid) -> Result<Vec<UserApiKey>, DBError>;
    fn delete_user_api_key(&self, id: i32, user_id: Uuid) -> Result<(), DBError>;
    fn delete_user_api_key_by_name(&self, name: &str, user_id: Uuid) -> Result<(), DBError>;

    // Platform invite code methods
    fn validate_platform_invite_code(&self, code: Uuid) -> Result<PlatformInviteCode, DBError>;

    // ---------- Responses API helpers ----------

    // Conversations
    fn create_conversation(
        &self,
        new_conversation: NewConversation,
    ) -> Result<Conversation, DBError>;
    fn create_conversation_project(
        &self,
        new_project: NewConversationProject,
    ) -> Result<ConversationProject, DBError>;
    fn get_conversation_by_id_and_user(
        &self,
        conversation_id: i64,
        user_id: Uuid,
    ) -> Result<Conversation, DBError>;
    fn get_conversation_by_uuid_and_user(
        &self,
        conversation_uuid: Uuid,
        user_id: Uuid,
    ) -> Result<Conversation, DBError>;
    fn get_conversation_project_by_id_and_user(
        &self,
        project_id: i64,
        user_id: Uuid,
    ) -> Result<ConversationProject, DBError>;
    fn get_conversation_project_by_uuid_and_user(
        &self,
        project_uuid: Uuid,
        user_id: Uuid,
    ) -> Result<ConversationProject, DBError>;
    fn update_conversation_metadata(
        &self,
        conversation_id: i64,
        user_id: Uuid,
        metadata_enc: Vec<u8>,
    ) -> Result<Conversation, DBError>;
    fn update_conversation(
        &self,
        conversation_id: i64,
        user_id: Uuid,
        metadata_enc: Option<Vec<u8>>,
        project_id: Option<Option<i64>>,
        is_pinned: Option<bool>,
    ) -> Result<Conversation, DBError>;
    fn batch_update_conversation_project(
        &self,
        conversation_uuids: &[Uuid],
        user_id: Uuid,
        target_project_id: Option<i64>,
    ) -> Result<(), DBError>;
    #[allow(clippy::too_many_arguments)]
    fn list_conversations(
        &self,
        user_id: Uuid,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
        project_filter: ConversationProjectFilter,
        pinned: Option<bool>,
    ) -> Result<Vec<Conversation>, DBError>;
    fn list_conversation_projects(
        &self,
        user_id: Uuid,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
    ) -> Result<Vec<ConversationProject>, DBError>;
    fn update_conversation_project(
        &self,
        project_id: i64,
        user_id: Uuid,
        name_enc: Option<Vec<u8>>,
        instruction_update: ProjectInstructionUpdate,
    ) -> Result<ConversationProject, DBError>;
    fn delete_conversation(&self, conversation_id: i64, user_id: Uuid) -> Result<(), DBError>;
    fn delete_all_conversations(&self, user_id: Uuid) -> Result<(), DBError>;
    fn delete_conversation_project(&self, project_id: i64, user_id: Uuid) -> Result<(), DBError>;

    // Responses (job tracker)
    fn create_response(&self, new_response: NewResponse) -> Result<Response, DBError>;
    fn create_response_with_message_and_tool_items(
        &self,
        new_response: NewResponse,
        user_message: NewUserMessage,
        tool_items: Vec<(NewToolCall, NewToolOutput)>,
    ) -> Result<PersistedResponseWithToolItems, DBError>;
    fn get_response_by_uuid_and_user(&self, uuid: Uuid, user_id: Uuid)
        -> Result<Response, DBError>;
    fn update_response_status(
        &self,
        id: i64,
        status: ResponseStatus,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<(), DBError>;
    fn update_response_status_if_current(
        &self,
        id: i64,
        current_status: ResponseStatus,
        new_status: ResponseStatus,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<bool, DBError>;
    fn cancel_response(&self, uuid: Uuid, user_id: Uuid) -> Result<Response, DBError>;
    fn delete_response(&self, uuid: Uuid, user_id: Uuid) -> Result<(), DBError>;

    #[allow(clippy::too_many_arguments)]
    fn create_conversation_with_response_and_message(
        &self,
        conversation_uuid: Uuid,
        user_id: Uuid,
        metadata_enc: Option<Vec<u8>>,
        response: Option<NewResponse>,
        first_message_content: Vec<u8>,
        first_message_tokens: i32,
        message_uuid: Uuid,
        assistant_message_uuid: Option<Uuid>,
    ) -> Result<(Conversation, Option<Response>, UserMessage), DBError>;

    // User instructions methods
    fn get_default_user_instruction(
        &self,
        user_id: Uuid,
    ) -> Result<Option<UserInstruction>, DBError>;
    fn get_project_instruction(
        &self,
        project_id: i64,
        user_id: Uuid,
    ) -> Result<Option<UserInstruction>, DBError>;
    fn get_project_instruction_for_conversation(
        &self,
        conversation_id: i64,
        user_id: Uuid,
    ) -> Result<Option<UserInstruction>, DBError>;
    fn get_user_instruction_by_uuid_and_user(
        &self,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<UserInstruction, DBError>;
    fn create_user_instruction(
        &self,
        new_instruction: NewUserInstruction,
    ) -> Result<UserInstruction, DBError>;
    fn update_user_instruction(
        &self,
        id: i64,
        user_id: Uuid,
        name_enc: Vec<u8>,
        prompt_enc: Vec<u8>,
        prompt_tokens: i32,
        is_default: bool,
    ) -> Result<UserInstruction, DBError>;
    fn delete_user_instruction(&self, id: i64, user_id: Uuid) -> Result<(), DBError>;
    fn list_user_instructions(
        &self,
        user_id: Uuid,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
    ) -> Result<Vec<UserInstruction>, DBError>;
    fn set_default_user_instruction(
        &self,
        id: i64,
        user_id: Uuid,
    ) -> Result<UserInstruction, DBError>;

    // User messages
    fn create_user_message(&self, new_msg: NewUserMessage) -> Result<UserMessage, DBError>;
    fn update_user_message_prompt_tokens(&self, id: i64, prompt_tokens: i32)
        -> Result<(), DBError>;
    fn get_user_message(&self, id: i64, user_id: Uuid) -> Result<UserMessage, DBError>;
    fn get_user_message_by_uuid(&self, uuid: Uuid, user_id: Uuid) -> Result<UserMessage, DBError>;
    fn user_message_uuid_exists(&self, uuid: Uuid) -> Result<bool, DBError>;

    // Assistant messages
    fn create_assistant_message(
        &self,
        new_msg: NewAssistantMessage,
    ) -> Result<AssistantMessage, DBError>;
    fn get_assistant_message_by_uuid(
        &self,
        message_uuid: Uuid,
    ) -> Result<Option<AssistantMessage>, DBError>;
    fn update_assistant_message(
        &self,
        message_uuid: Uuid,
        content_enc: Option<Vec<u8>>,
        completion_tokens: i32,
        status: String,
        finish_reason: Option<String>,
    ) -> Result<AssistantMessage, DBError>;

    // Reasoning items
    fn create_reasoning_item(&self, new_item: NewReasoningItem) -> Result<ReasoningItem, DBError>;
    fn get_reasoning_item_by_uuid(&self, item_uuid: Uuid)
        -> Result<Option<ReasoningItem>, DBError>;
    fn update_reasoning_item(
        &self,
        item_uuid: Uuid,
        content_enc: Option<Vec<u8>>,
        reasoning_tokens: i32,
        status: String,
    ) -> Result<ReasoningItem, DBError>;

    // Tool calls / outputs
    fn create_tool_call(&self, new_call: NewToolCall) -> Result<ToolCall, DBError>;
    fn get_tool_call_by_uuid(&self, uuid: Uuid, user_id: Uuid) -> Result<ToolCall, DBError>;
    fn create_tool_output(&self, new_output: NewToolOutput) -> Result<ToolOutput, DBError>;
    fn get_tool_output_by_uuid(&self, uuid: Uuid, user_id: Uuid) -> Result<ToolOutput, DBError>;

    // Context reconstruction
    fn get_conversation_context_messages(
        &self,
        conversation_id: i64,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
    ) -> Result<Vec<RawThreadMessage>, DBError>;
    fn get_response_context_messages(
        &self,
        response_id: i64,
    ) -> Result<Vec<RawThreadMessage>, DBError>;

    // Optimized context reconstruction (metadata-based)
    fn get_conversation_context_metadata(
        &self,
        conversation_id: i64,
    ) -> Result<Vec<RawThreadMessageMetadata>, DBError>;
    fn get_messages_by_ids(
        &self,
        conversation_id: i64,
        message_ids: &[(String, i64)],
    ) -> Result<Vec<RawThreadMessage>, DBError>;

    // Delete operation for user messages
    fn delete_user_message(&self, id: Uuid, user_id: Uuid) -> Result<(), DBError>;

    // Recovery wrap helpers
    fn get_recovery_wrap(&self, user_id: Uuid) -> Result<Option<UserSeedWrapping>, DBError>;
    fn insert_recovery_wrap_if_absent(
        &self,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<UserSeedWrapping, DBError>;
    fn replace_recovery_wrap_if_unchanged(
        &self,
        old_wrapping: &UserSeedWrapping,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<UserSeedWrapping, DBError>;
    fn delete_recovery_wrap_for_user(&self, user_id: Uuid) -> Result<usize, DBError>;
    fn recovery_wrap_exists(&self, user_id: Uuid) -> Result<bool, DBError>;
}

pub(crate) struct PostgresConnection {
    db: Pool<ConnectionManager<PgConnection>>,
}

impl DBConnection for PostgresConnection {
    fn create_user(&self, new_user: NewUser) -> Result<User, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_user.insert(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to create user: {:?}", e);
        }
        result
    }

    fn get_user_by_uuid(&self, uuid: Uuid) -> Result<User, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = User::get_by_uuid(conn, uuid)?.ok_or(DBError::UserNotFound);
        if let Err(ref e) = result {
            error!("Failed to get user by UUID: {:?}", e);
        }
        result
    }

    fn get_user_by_email(&self, email: String, project_id: i32) -> Result<User, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = User::get_by_email(conn, email, project_id)?.ok_or(DBError::UserNotFound);
        if let Err(ref e) = result {
            error!("Failed to get user by email: {:?}", e);
        }
        result
    }

    fn create_user_seed_wrapping(
        &self,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<UserSeedWrapping, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_wrapping.insert(conn).map_err(DBError::from)
    }

    fn upsert_user_seed_wrapping(
        &self,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<UserSeedWrapping, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_wrapping
            .upsert_by_credential(conn)
            .map_err(DBError::from)
    }

    fn get_user_seed_wrappings_for_user_and_kind(
        &self,
        user_id: Uuid,
        credential_kind: &str,
    ) -> Result<Vec<UserSeedWrapping>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserSeedWrapping::get_for_user_and_kind(conn, user_id, credential_kind)
            .map_err(DBError::from)
    }

    fn get_user_seed_wrapping_by_credential(
        &self,
        user_id: Uuid,
        credential_kind: &str,
        credential_lookup_hash: &[u8],
        wrapping_version: i16,
    ) -> Result<Option<UserSeedWrapping>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserSeedWrapping::get_by_credential(
            conn,
            user_id,
            credential_kind,
            credential_lookup_hash,
            wrapping_version,
        )
        .map_err(DBError::from)
    }

    fn delete_user_seed_wrappings_for_user(&self, user_id: Uuid) -> Result<usize, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserSeedWrapping::delete_for_user(conn, user_id).map_err(DBError::from)
    }

    fn delete_user_seed_wrappings_for_user_and_kind(
        &self,
        user_id: Uuid,
        credential_kind: &str,
    ) -> Result<usize, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserSeedWrapping::delete_for_user_and_kind(conn, user_id, credential_kind)
            .map_err(DBError::from)
    }

    fn get_app_data_migration(&self, name: &str) -> Result<Option<AppDataMigration>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        AppDataMigration::get(conn, name).map_err(DBError::from)
    }

    fn app_data_migration_exists(&self, name: &str) -> Result<bool, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        AppDataMigration::exists(conn, name).map_err(DBError::from)
    }

    fn create_app_data_migration(
        &self,
        new_migration: NewAppDataMigration,
    ) -> Result<AppDataMigration, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_migration.insert(conn).map_err(DBError::from)
    }

    fn get_pool(&self) -> &diesel::r2d2::Pool<diesel::r2d2::ConnectionManager<PgConnection>> {
        &self.db
    }

    fn create_enclave_secret(
        &self,
        new_secret: NewEnclaveSecret,
    ) -> Result<EnclaveSecret, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_secret.insert(conn).map_err(DBError::from)
    }

    fn get_enclave_secret_by_id(&self, id: i32) -> Result<Option<EnclaveSecret>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        EnclaveSecret::get_by_id(conn, id).map_err(DBError::from)
    }

    fn get_enclave_secret_by_key(&self, key: &str) -> Result<Option<EnclaveSecret>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        EnclaveSecret::get_by_key(conn, key).map_err(DBError::from)
    }

    fn get_all_enclave_secrets(&self) -> Result<Vec<EnclaveSecret>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        EnclaveSecret::get_all(conn).map_err(DBError::from)
    }

    fn update_enclave_secret(&self, secret: &EnclaveSecret) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        secret.update(conn).map_err(DBError::from)
    }

    fn delete_enclave_secret(&self, secret: &EnclaveSecret) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        secret.delete(conn).map_err(DBError::from)
    }

    fn create_email_verification(
        &self,
        new_verification: NewEmailVerification,
    ) -> Result<EmailVerification, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_verification.insert(conn).map_err(DBError::from)
    }

    fn get_email_verification_by_id(&self, id: i32) -> Result<EmailVerification, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        EmailVerification::get_by_id(conn, id)?.ok_or(DBError::EmailVerificationNotFound)
    }

    fn get_email_verification_by_user_id(
        &self,
        user_id: Uuid,
    ) -> Result<EmailVerification, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        EmailVerification::get_by_user_id(conn, user_id)?.ok_or(DBError::EmailVerificationNotFound)
    }

    fn get_email_verification_by_code(&self, code: Uuid) -> Result<EmailVerification, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        EmailVerification::get_by_verification_code(conn, code)?
            .ok_or(DBError::EmailVerificationNotFound)
    }

    fn update_email_verification(&self, verification: &EmailVerification) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        verification.update(conn).map_err(DBError::from)
    }

    fn delete_email_verification(&self, verification: &EmailVerification) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        verification.delete(conn).map_err(DBError::from)
    }

    fn verify_email(&self, verification: &mut EmailVerification) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = verification.verify(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to verify email: {:?}", e);
        }
        result
    }

    fn create_password_reset_request(
        &self,
        new_request: NewPasswordResetRequest,
    ) -> Result<PasswordResetRequest, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_request.insert(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to create password reset request: {:?}", e);
        }
        result
    }

    fn get_password_reset_request_by_user_id_and_code(
        &self,
        user_id: Uuid,
        encrypted_code: Vec<u8>,
    ) -> Result<Option<PasswordResetRequest>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = PasswordResetRequest::get_by_user_id_and_code(conn, user_id, &encrypted_code)
            .map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get password reset request: {:?}", e);
        }
        result
    }

    fn update_user_password_and_seed_wrap(
        &self,
        user: &User,
        expected_password_enc: &[u8],
        new_password_enc: Vec<u8>,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        conn.transaction::<_, DBError, _>(|conn| {
            let updated_user_count = diesel::update(
                users::table
                    .filter(users::uuid.eq(user.uuid))
                    .filter(users::password_enc.eq(Some(expected_password_enc.to_vec()))),
            )
            .set((
                users::password_enc.eq(Some(new_password_enc)),
                users::updated_at.eq(diesel::dsl::now),
            ))
            .execute(conn)?;
            if updated_user_count != 1 {
                return Err(DBError::StaleCredentialState);
            }

            UserSeedWrapping::delete_for_user_and_kind(
                conn,
                user.uuid,
                CredentialKind::Password.as_str(),
            )?;
            new_wrapping.insert(conn)?;
            Ok(())
        })
    }

    fn mark_password_reset_as_complete(
        &self,
        request: &PasswordResetRequest,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = request.mark_as_reset(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to mark password reset request as complete: {:?}", e);
        }
        result
    }

    fn complete_destructive_password_reset(
        &self,
        user: &User,
        reset_request: &PasswordResetRequest,
        new_password_enc: Vec<u8>,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<(), DBError> {
        use crate::models::schema::{
            agent_schedule_runs, agent_schedules, agents, conversation_projects,
            conversation_summaries, conversations, memory_blocks, notification_events,
            password_reset_requests, push_devices, user_embeddings, user_instructions, user_kv,
            user_oauth_connections, user_preferences, user_seed_wrappings, users,
        };

        debug!("Completing destructive password reset");
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        conn.transaction::<_, DBError, _>(|conn| {
            let user_id = user.uuid;

            let _locked_user = users::table
                .filter(users::uuid.eq(user_id))
                .for_update()
                .first::<User>(conn)?;

            let consumed_reset_count = diesel::update(
                password_reset_requests::table
                    .filter(password_reset_requests::id.eq(reset_request.id))
                    .filter(password_reset_requests::user_id.eq(user_id))
                    .filter(password_reset_requests::is_reset.eq(false))
                    .filter(password_reset_requests::expiration_time.gt(diesel::dsl::now)),
            )
            .set(password_reset_requests::is_reset.eq(true))
            .execute(conn)?;
            if consumed_reset_count != 1 {
                return Err(DBError::PasswordResetRequestNotFound);
            }

            diesel::update(
                password_reset_requests::table
                    .filter(password_reset_requests::user_id.eq(user_id))
                    .filter(password_reset_requests::id.ne(reset_request.id))
                    .filter(password_reset_requests::is_reset.eq(false)),
            )
            .set(password_reset_requests::is_reset.eq(true))
            .execute(conn)?;

            diesel::delete(
                user_seed_wrappings::table.filter(user_seed_wrappings::user_id.eq(user_id)),
            )
            .execute(conn)?;
            diesel::delete(
                user_oauth_connections::table.filter(user_oauth_connections::user_id.eq(user_id)),
            )
            .execute(conn)?;
            diesel::delete(user_embeddings::table.filter(user_embeddings::user_id.eq(user_id)))
                .execute(conn)?;
            diesel::delete(
                agent_schedule_runs::table.filter(agent_schedule_runs::user_id.eq(user_id)),
            )
            .execute(conn)?;
            diesel::delete(agent_schedules::table.filter(agent_schedules::user_id.eq(user_id)))
                .execute(conn)?;
            diesel::delete(agents::table.filter(agents::user_id.eq(user_id))).execute(conn)?;
            diesel::delete(
                notification_events::table.filter(notification_events::user_id.eq(user_id)),
            )
            .execute(conn)?;
            diesel::delete(push_devices::table.filter(push_devices::user_id.eq(user_id)))
                .execute(conn)?;
            diesel::delete(memory_blocks::table.filter(memory_blocks::user_id.eq(user_id)))
                .execute(conn)?;
            diesel::delete(user_preferences::table.filter(user_preferences::user_id.eq(user_id)))
                .execute(conn)?;
            diesel::delete(user_kv::table.filter(user_kv::user_id.eq(user_id))).execute(conn)?;
            diesel::delete(user_instructions::table.filter(user_instructions::user_id.eq(user_id)))
                .execute(conn)?;
            diesel::delete(
                conversation_projects::table.filter(conversation_projects::user_id.eq(user_id)),
            )
            .execute(conn)?;
            diesel::delete(
                conversation_summaries::table.filter(conversation_summaries::user_id.eq(user_id)),
            )
            .execute(conn)?;
            diesel::delete(conversations::table.filter(conversations::user_id.eq(user_id)))
                .execute(conn)?;

            diesel::update(users::table.filter(users::uuid.eq(user_id)))
                .set((
                    users::password_enc.eq(Some(new_password_enc)),
                    users::updated_at.eq(diesel::dsl::now),
                ))
                .execute(conn)?;

            new_wrapping.upsert_by_credential(conn)?;

            Ok(())
        })
    }

    // OAuth Provider method implementations
    fn create_oauth_provider(
        &self,
        new_provider: NewOAuthProvider,
    ) -> Result<OAuthProvider, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_provider.insert(conn).map_err(DBError::from)
    }

    fn get_oauth_provider_by_id(&self, id: i32) -> Result<Option<OAuthProvider>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        OAuthProvider::get_by_id(conn, id).map_err(DBError::from)
    }

    fn get_oauth_provider_by_name(&self, name: &str) -> Result<Option<OAuthProvider>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        OAuthProvider::get_by_name(conn, name).map_err(DBError::from)
    }

    fn get_all_oauth_providers(&self) -> Result<Vec<OAuthProvider>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        OAuthProvider::get_all(conn).map_err(DBError::from)
    }

    fn update_oauth_provider(&self, provider: &OAuthProvider) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        provider.update(conn).map_err(DBError::from)
    }

    fn delete_oauth_provider(&self, provider: &OAuthProvider) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        provider.delete(conn).map_err(DBError::from)
    }

    // User OAuth Connection method implementations
    fn create_user_oauth_connection(
        &self,
        new_connection: NewUserOAuthConnection,
    ) -> Result<UserOAuthConnection, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_connection.insert(conn).map_err(DBError::from)
    }

    fn get_user_oauth_connection_by_id(
        &self,
        id: i32,
    ) -> Result<Option<UserOAuthConnection>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserOAuthConnection::get_by_id(conn, id).map_err(DBError::from)
    }

    fn get_user_oauth_connection_by_user_and_provider(
        &self,
        user_id: Uuid,
        provider_id: i32,
    ) -> Result<Option<UserOAuthConnection>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserOAuthConnection::get_by_user_and_provider(conn, user_id, provider_id)
            .map_err(DBError::from)
    }

    fn get_project_user_oauth_connection_by_provider_subject(
        &self,
        provider_id: i32,
        provider_user_id: &str,
        project_id: i32,
    ) -> Result<Option<UserOAuthConnection>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserOAuthConnection::get_by_provider_subject_and_project(
            conn,
            provider_id,
            provider_user_id,
            project_id,
        )
        .map_err(DBError::from)
    }

    fn get_all_user_oauth_connections_for_user(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<UserOAuthConnection>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserOAuthConnection::get_all_for_user(conn, user_id).map_err(DBError::from)
    }

    fn update_user_oauth_connection(
        &self,
        connection: &UserOAuthConnection,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        connection.update(conn).map_err(DBError::from)
    }

    fn delete_user_oauth_connection(
        &self,
        connection: &UserOAuthConnection,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        connection.delete(conn).map_err(DBError::from)
    }

    fn create_token_usage(&self, new_usage: NewTokenUsage) -> Result<TokenUsage, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_usage.insert(conn).map_err(DBError::from)
    }

    fn update_user(&self, user: &User) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        user.update(conn).map_err(DBError::from)
    }

    // Org implementations
    fn create_org(&self, new_org: NewOrg) -> Result<Org, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_org.insert(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to create org: {:?}", e);
        }
        result
    }

    fn get_org_by_id(&self, id: i32) -> Result<Org, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = Org::get_by_id(conn, id)?.ok_or(DBError::OrgNotFound);
        if let Err(ref e) = result {
            error!("Failed to get org by ID: {:?}", e);
        }
        result
    }

    fn get_org_by_uuid(&self, uuid: Uuid) -> Result<Org, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = Org::get_by_uuid(conn, uuid)?.ok_or(DBError::OrgNotFound);
        if let Err(ref e) = result {
            error!("Failed to get org by UUID: {:?}", e);
        }
        result
    }

    fn get_org_by_name(&self, name: &str) -> Result<Option<Org>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = Org::get_by_name(conn, name).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get org by name: {:?}", e);
        }
        result
    }

    fn get_all_orgs(&self) -> Result<Vec<Org>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = Org::get_all(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get all orgs: {:?}", e);
        }
        result
    }

    fn update_org(&self, org: &Org) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = org.update(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to update org: {:?}", e);
        }
        result
    }

    fn delete_org(&self, org: &Org) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = org.delete(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to delete org: {:?}", e);
        }
        result
    }

    // Org project implementations
    fn create_org_project(&self, new_project: NewOrgProject) -> Result<OrgProject, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_project.insert(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to create org project: {:?}", e);
        }
        result
    }

    fn get_org_project_by_id(&self, id: i32) -> Result<OrgProject, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgProject::get_by_id(conn, id)?.ok_or(DBError::OrgProjectNotFound);
        if let Err(ref e) = result {
            error!("Failed to get org project by ID: {:?}", e);
        }
        result
    }

    fn get_org_project_by_uuid(&self, uuid: Uuid) -> Result<OrgProject, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgProject::get_by_uuid(conn, uuid)?.ok_or(DBError::OrgProjectNotFound);
        if let Err(ref e) = result {
            error!("Failed to get org project by UUID: {:?}", e);
        }
        result
    }

    fn get_org_project_by_client_id(&self, client_id: Uuid) -> Result<OrgProject, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result =
            OrgProject::get_by_client_id(conn, client_id)?.ok_or(DBError::OrgProjectNotFound);
        if let Err(ref e) = result {
            error!("Failed to get org project by client ID: {:?}", e);
        }
        result
    }

    fn get_org_project_by_name_and_org(
        &self,
        name: &str,
        org_id: i32,
    ) -> Result<Option<OrgProject>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgProject::get_by_name_and_org(conn, name, org_id).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get org project by name and org: {:?}", e);
        }
        result
    }

    fn get_all_org_projects_for_org(&self, org_id: i32) -> Result<Vec<OrgProject>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgProject::get_all_for_org(conn, org_id).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get all org projects for org: {:?}", e);
        }
        result
    }

    fn get_active_org_projects_for_org(&self, org_id: i32) -> Result<Vec<OrgProject>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgProject::get_active_for_org(conn, org_id).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get active org projects for org: {:?}", e);
        }
        result
    }

    fn update_org_project(&self, project: &OrgProject) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = project.update(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to update org project: {:?}", e);
        }
        result
    }

    fn delete_org_project(&self, project: &OrgProject) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = project.delete(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to delete org project: {:?}", e);
        }
        result
    }

    // Org project secret implementations
    fn create_org_project_secret(
        &self,
        new_secret: NewOrgProjectSecret,
    ) -> Result<OrgProjectSecret, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_secret.insert(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to create org project secret: {:?}", e);
        }
        result
    }

    fn get_org_project_secret_by_id(&self, id: i32) -> Result<OrgProjectSecret, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result =
            OrgProjectSecret::get_by_id(conn, id)?.ok_or(DBError::OrgProjectSecretNotFound);
        if let Err(ref e) = result {
            error!("Failed to get org project secret by ID: {:?}", e);
        }
        result
    }

    fn get_org_project_secret_by_key_name_and_project(
        &self,
        key_name: &str,
        project_id: i32,
    ) -> Result<Option<OrgProjectSecret>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgProjectSecret::get_by_key_name_and_project(conn, key_name, project_id)
            .map_err(DBError::from);
        if let Err(ref e) = result {
            error!(
                "Failed to get org project secret by key name and project: {:?}",
                e
            );
        }
        result
    }

    fn get_all_org_project_secrets_for_project(
        &self,
        project_id: i32,
    ) -> Result<Vec<OrgProjectSecret>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgProjectSecret::get_all_for_project(conn, project_id).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get all org project secrets for project: {:?}", e);
        }
        result
    }

    fn update_org_project_secret(&self, secret: &OrgProjectSecret) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = secret.update(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to update org project secret: {:?}", e);
        }
        result
    }

    fn delete_org_project_secret(&self, secret: &OrgProjectSecret) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = secret.delete(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to delete org project secret: {:?}", e);
        }
        result
    }

    // Invite code implementations
    fn create_invite_code(&self, new_invite: NewInviteCode) -> Result<InviteCode, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_invite.insert(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to create invite code: {:?}", e);
        }
        result
    }

    fn get_invite_code_by_id(&self, id: i32) -> Result<InviteCode, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = InviteCode::get_by_id(conn, id)?.ok_or(DBError::InviteCodeNotFound);
        if let Err(ref e) = result {
            error!("Failed to get invite code by ID: {:?}", e);
        }
        result
    }

    fn get_invite_code_by_code(&self, code: Uuid) -> Result<InviteCode, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = InviteCode::get_by_code(conn, code)?.ok_or(DBError::InviteCodeNotFound);
        if let Err(ref e) = result {
            error!("Failed to get invite code by code: {:?}", e);
        }
        result
    }

    fn get_invite_code_by_email_and_org(
        &self,
        email: &str,
        org_id: i32,
    ) -> Result<Option<InviteCode>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = InviteCode::get_by_email_and_org(conn, email, org_id).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get invite code by email and org: {:?}", e);
        }
        result
    }

    fn get_all_invite_codes_for_org(&self, org_id: i32) -> Result<Vec<InviteCode>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = InviteCode::get_all_for_org(conn, org_id).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get all invite codes for org: {:?}", e);
        }
        result
    }

    fn mark_invite_code_as_used(&self, invite: &InviteCode) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = invite.mark_as_used(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to mark invite code as used: {:?}", e);
        }
        result
    }

    fn update_invite_code(&self, invite: &InviteCode) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = invite.update(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to update invite code: {:?}", e);
        }
        result
    }

    fn delete_invite_code(&self, invite: &InviteCode) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = invite.delete(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to delete invite code: {:?}", e);
        }
        result
    }

    // Platform user methods
    fn create_platform_user(&self, new_user: NewPlatformUser) -> Result<PlatformUser, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_user.insert(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to create platform user: {:?}", e);
        }
        result
    }

    fn get_platform_user_by_id(&self, id: i32) -> Result<PlatformUser, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = PlatformUser::get_by_id(conn, id)?.ok_or(DBError::PlatformUserNotFound);
        if let Err(ref e) = result {
            error!("Failed to get platform user by ID: {:?}", e);
        }
        result
    }

    fn get_platform_user_by_uuid(&self, uuid: Uuid) -> Result<PlatformUser, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = PlatformUser::get_by_uuid(conn, uuid)?.ok_or(DBError::PlatformUserNotFound);
        if let Err(ref e) = result {
            error!("Failed to get platform user by UUID: {:?}", e);
        }
        result
    }

    fn get_platform_user_by_email(&self, email: &str) -> Result<Option<PlatformUser>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = PlatformUser::get_by_email(conn, email).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get platform user by email: {:?}", e);
        }
        result
    }

    fn update_platform_user(&self, user: &PlatformUser) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = user.update(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to update platform user: {:?}", e);
        }
        result
    }

    fn update_platform_user_password(
        &self,
        user: &PlatformUser,
        new_password_enc: Vec<u8>,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = user
            .update_password(conn, new_password_enc)
            .map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to update platform user password: {:?}", e);
        }
        result
    }

    // Org membership methods
    fn create_org_membership(
        &self,
        new_membership: NewOrgMembership,
    ) -> Result<OrgMembership, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_membership
            .insert(conn)
            .map_err(DBError::OrgMembershipError);
        if let Err(ref e) = result {
            error!("Failed to create org membership: {:?}", e);
        }
        result
    }

    fn get_org_membership_by_platform_user_and_org(
        &self,
        platform_user_id: Uuid,
        org_id: i32,
    ) -> Result<OrgMembership, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgMembership::get_by_platform_user_and_org(conn, platform_user_id, org_id)
            .map_err(DBError::from);
        if let Err(ref e) = result {
            error!(
                "Failed to get org membership by platform user and org: {:?}",
                e
            );
        }
        result
    }

    fn get_org_membership_by_platform_user_and_org_with_user(
        &self,
        platform_user_id: Uuid,
        org_id: i32,
    ) -> Result<OrgMembershipWithUser, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result =
            OrgMembership::get_by_platform_user_and_org_with_user(conn, platform_user_id, org_id)
                .map_err(DBError::from);
        if let Err(ref e) = result {
            error!(
                "Failed to get org membership with user info by platform user and org: {:?}",
                e
            );
        }
        result
    }

    fn get_all_org_memberships_for_platform_user(
        &self,
        platform_user_id: Uuid,
    ) -> Result<Vec<OrgMembership>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result =
            OrgMembership::get_all_for_platform_user(conn, platform_user_id).map_err(DBError::from);
        if let Err(ref e) = result {
            error!(
                "Failed to get all org memberships for platform user: {:?}",
                e
            );
        }
        result
    }

    fn get_all_org_memberships_for_org(&self, org_id: i32) -> Result<Vec<OrgMembership>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgMembership::get_all_for_org(conn, org_id).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get all org memberships for org: {:?}", e);
        }
        result
    }

    fn get_all_org_memberships_with_users_for_org(
        &self,
        org_id: i32,
    ) -> Result<Vec<OrgMembershipWithUser>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgMembership::get_all_with_users_for_org(conn, org_id).map_err(DBError::from);
        if let Err(ref e) = result {
            error!(
                "Failed to get all org memberships with users for org: {:?}",
                e
            );
        }
        result
    }

    fn update_org_membership(&self, membership: &OrgMembership) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = membership.update(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to update org membership: {:?}", e);
        }
        result
    }

    fn delete_org_membership(&self, membership: &OrgMembership) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = membership.delete(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to delete org membership: {:?}", e);
        }
        result
    }

    fn update_membership_role(
        &self,
        membership: &mut OrgMembership,
        new_role: OrgRole,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = OrgMembership::update_role_with_owner_check(conn, membership, new_role)
            .map_err(|e| match e {
                OrgMembershipError::DatabaseError(diesel::result::Error::RollbackTransaction) => {
                    DBError::OrgMembershipError(OrgMembershipError::DatabaseError(
                        diesel::result::Error::RollbackTransaction,
                    ))
                }
                _ => DBError::from(e),
            });
        if let Err(ref e) = result {
            error!("Failed to update org membership role: {:?}", e);
        }
        result
    }

    fn delete_membership_with_owner_check(
        &self,
        membership: &OrgMembership,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result =
            OrgMembership::delete_with_owner_check(conn, membership).map_err(|e| match e {
                OrgMembershipError::DatabaseError(diesel::result::Error::RollbackTransaction) => {
                    DBError::OrgMembershipError(OrgMembershipError::DatabaseError(
                        diesel::result::Error::RollbackTransaction,
                    ))
                }
                _ => DBError::from(e),
            });
        if let Err(ref e) = result {
            error!("Failed to delete org membership: {:?}", e);
        }
        result
    }

    // New project-scoped methods
    fn get_users_for_project(
        &self,
        project_id: i32,
        page: Option<i64>,
        per_page: Option<i64>,
    ) -> Result<(Vec<User>, i64), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        // Get total count first
        let total = User::get_count_for_project(conn, project_id)?;

        // Default to first page with 10 items per page
        let page = page.unwrap_or(0);
        let per_page = per_page.unwrap_or(10);

        let users = User::get_all_for_project(conn, project_id, page, per_page)?;

        Ok((users, total))
    }

    fn create_org_with_owner(&self, new_org: NewOrg, owner_id: Uuid) -> Result<Org, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        conn.transaction(|conn| {
            // Create the organization
            let org = new_org.insert(conn).map_err(DBError::from)?;

            // Create ownership membership
            let new_membership = NewOrgMembership::new(owner_id, org.id, OrgRole::Owner);
            new_membership.insert(conn)?;

            Ok(org)
        })
    }

    fn accept_invite_transaction(
        &self,
        invite: &InviteCode,
        new_membership: NewOrgMembership,
    ) -> Result<OrgMembership, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        conn.transaction(|conn| {
            // Create the membership
            let membership = new_membership.insert(conn)?;

            // Mark invite as used
            invite.mark_as_used(conn)?;

            Ok(membership)
        })
    }

    // Project settings methods
    fn get_project_settings(
        &self,
        project_id: i32,
        category: SettingCategory,
    ) -> Result<Option<ProjectSetting>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        ProjectSetting::get_by_project_and_category(conn, project_id, category)
            .map_err(DBError::from)
    }

    fn update_project_settings(
        &self,
        project_id: i32,
        category: SettingCategory,
        settings: serde_json::Value,
    ) -> Result<ProjectSetting, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        // Check if settings exist
        if let Some(mut existing) =
            ProjectSetting::get_by_project_and_category(conn, project_id, category.clone())?
        {
            existing.settings = settings;
            existing.update(conn)?;
            Ok(existing)
        } else {
            // Create new settings
            let new_settings = NewProjectSetting {
                project_id,
                category: category.as_str().to_string(),
                settings,
            };
            new_settings.insert(conn).map_err(DBError::from)
        }
    }

    fn get_project_email_settings(
        &self,
        project_id: i32,
    ) -> Result<Option<EmailSettings>, DBError> {
        let settings = self.get_project_settings(project_id, SettingCategory::Email)?;

        match settings {
            Some(s) => s.get_email_settings().map(Some).map_err(DBError::from),
            None => Ok(None),
        }
    }

    fn update_project_email_settings(
        &self,
        project_id: i32,
        settings: EmailSettings,
    ) -> Result<ProjectSetting, DBError> {
        let new_settings = NewProjectSetting::new_email_settings(project_id, settings)?;
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        // Check if settings exist
        if let Some(mut existing) =
            ProjectSetting::get_by_project_and_category(conn, project_id, SettingCategory::Email)?
        {
            existing.settings = new_settings.settings;
            existing.update(conn)?;
            Ok(existing)
        } else {
            // Create new settings
            new_settings.insert(conn).map_err(DBError::from)
        }
    }

    fn get_project_oauth_settings(
        &self,
        project_id: i32,
    ) -> Result<Option<OAuthSettings>, DBError> {
        let settings = self.get_project_settings(project_id, SettingCategory::OAuth)?;

        match settings {
            Some(s) => s.get_oauth_settings().map(Some).map_err(DBError::from),
            None => Ok(None),
        }
    }

    fn update_project_oauth_settings(
        &self,
        project_id: i32,
        settings: OAuthSettings,
    ) -> Result<ProjectSetting, DBError> {
        let new_settings = NewProjectSetting::new_oauth_settings(project_id, settings)?;
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        // Check if settings exist
        if let Some(mut existing) =
            ProjectSetting::get_by_project_and_category(conn, project_id, SettingCategory::OAuth)?
        {
            existing.settings = new_settings.settings;
            existing.update(conn)?;
            Ok(existing)
        } else {
            // Create new settings
            new_settings.insert(conn).map_err(DBError::from)
        }
    }

    // Platform email verification implementations
    fn create_platform_email_verification(
        &self,
        new_verification: NewPlatformEmailVerification,
    ) -> Result<PlatformEmailVerification, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_verification.insert(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to create platform email verification: {:?}", e);
        }
        result
    }

    fn get_platform_email_verification_by_id(
        &self,
        id: i32,
    ) -> Result<PlatformEmailVerification, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = PlatformEmailVerification::get_by_id(conn, id)?
            .ok_or(DBError::PlatformEmailVerificationNotFound);
        if let Err(ref e) = result {
            error!("Failed to get platform email verification by ID: {:?}", e);
        }
        result
    }

    fn get_platform_email_verification_by_platform_user_id(
        &self,
        platform_user_id: Uuid,
    ) -> Result<PlatformEmailVerification, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = PlatformEmailVerification::get_by_platform_user_id(conn, platform_user_id)?
            .ok_or(DBError::PlatformEmailVerificationNotFound);
        if let Err(ref e) = result {
            error!(
                "Failed to get platform email verification by platform user ID: {:?}",
                e
            );
        }
        result
    }

    fn get_platform_email_verification_by_code(
        &self,
        code: Uuid,
    ) -> Result<PlatformEmailVerification, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = PlatformEmailVerification::get_by_verification_code(conn, code)?
            .ok_or(DBError::PlatformEmailVerificationNotFound);
        if let Err(ref e) = result {
            error!("Failed to get platform email verification by code: {:?}", e);
        }
        result
    }

    fn update_platform_email_verification(
        &self,
        verification: &PlatformEmailVerification,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = verification.update(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to update platform email verification: {:?}", e);
        }
        result
    }

    fn delete_platform_email_verification(
        &self,
        verification: &PlatformEmailVerification,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = verification.delete(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to delete platform email verification: {:?}", e);
        }
        result
    }

    fn verify_platform_email(
        &self,
        verification: &mut PlatformEmailVerification,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = verification.verify(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to verify platform email: {:?}", e);
        }
        result
    }

    // Platform password reset implementations
    fn create_platform_password_reset_request(
        &self,
        new_request: NewPlatformPasswordResetRequest,
    ) -> Result<PlatformPasswordResetRequest, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_request.insert(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to create platform password reset request: {:?}", e);
        }
        result
    }

    fn get_platform_password_reset_request_by_user_id_and_code(
        &self,
        user_id: Uuid,
        encrypted_code: Vec<u8>,
    ) -> Result<Option<PlatformPasswordResetRequest>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result =
            PlatformPasswordResetRequest::get_by_user_id_and_code(conn, user_id, &encrypted_code)
                .map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get platform password reset request: {:?}", e);
        }
        result
    }

    fn mark_platform_password_reset_as_complete(
        &self,
        request: &PlatformPasswordResetRequest,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = request.mark_as_reset(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!(
                "Failed to mark platform password reset request as complete: {:?}",
                e
            );
        }
        result
    }

    // Platform invite code implementations
    fn validate_platform_invite_code(&self, code: Uuid) -> Result<PlatformInviteCode, DBError> {
        debug!("Validating platform invite code");
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = PlatformInviteCode::validate_code(conn, code).map_err(|e| match e {
            PlatformInviteCodeError::InviteCodeNotFound(_) => DBError::PlatformInviteCodeNotFound,
            _ => DBError::from(e),
        });
        if let Err(ref e) = result {
            error!("Failed to validate platform invite code: {:?}", e);
        }
        result
    }

    // User API key implementations
    fn create_user_api_key(&self, new_key: NewUserApiKey) -> Result<UserApiKey, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_key.insert(conn).map_err(DBError::from)
    }

    fn get_user_api_key_by_id(&self, id: i32) -> Result<Option<UserApiKey>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserApiKey::get_by_id(conn, id).map_err(DBError::from)
    }

    fn get_user_api_key_by_hash(&self, key_hash: &str) -> Result<Option<UserApiKey>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserApiKey::get_by_key_hash(conn, key_hash).map_err(DBError::from)
    }

    fn get_user_by_api_key_hash(&self, key_hash: &str) -> Result<Option<User>, DBError> {
        use crate::models::schema::{user_api_keys, users};
        use diesel::prelude::*;

        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        // Single JOIN query to get user directly from API key hash
        users::table
            .inner_join(user_api_keys::table.on(users::uuid.eq(user_api_keys::user_id)))
            .filter(user_api_keys::key_hash.eq(key_hash))
            .select(users::all_columns)
            .first::<User>(conn)
            .optional()
            .map_err(DBError::from)
    }

    fn get_all_user_api_keys_for_user(&self, user_id: Uuid) -> Result<Vec<UserApiKey>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserApiKey::get_all_for_user(conn, user_id).map_err(DBError::from)
    }

    fn delete_user_api_key(&self, id: i32, user_id: Uuid) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        // First verify the key belongs to the user
        // Use the same error for both "not found" and "unauthorized" to prevent information disclosure
        match UserApiKey::get_by_id(conn, id)? {
            Some(api_key) if api_key.user_id == user_id => {
                UserApiKey::delete_by_id(conn, id).map_err(DBError::from)
            }
            _ => Err(DBError::UserApiKeyError(UserApiKeyError::NotFound)),
        }
    }

    fn delete_user_api_key_by_name(&self, name: &str, user_id: Uuid) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        // First find the key by name and user_id, then delete it
        // Use the same error for both "not found" and "unauthorized" to prevent information disclosure
        UserApiKey::delete_by_name_and_user(conn, name, user_id).map_err(|e| match e {
            UserApiKeyError::NotFound => DBError::UserApiKeyError(UserApiKeyError::NotFound),
            e => DBError::from(e),
        })
    }

    // Account Deletion implementations
    fn create_account_deletion_request(
        &self,
        new_request: NewAccountDeletionRequest,
    ) -> Result<AccountDeletionRequest, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = new_request.insert(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to create account deletion request: {:?}", e);
        }
        result
    }

    fn get_account_deletion_request_by_user_id_and_code(
        &self,
        user_id: Uuid,
        encrypted_code: Vec<u8>,
    ) -> Result<Option<AccountDeletionRequest>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result =
            AccountDeletionRequest::get_by_user_id_and_code(conn, user_id, &encrypted_code)
                .map_err(DBError::from);
        if let Err(ref e) = result {
            error!("Failed to get account deletion request: {:?}", e);
        }
        result
    }

    fn mark_account_deletion_as_complete(
        &self,
        request: &AccountDeletionRequest,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let result = request.mark_as_deleted(conn).map_err(DBError::from);
        if let Err(ref e) = result {
            error!(
                "Failed to mark account deletion request as complete: {:?}",
                e
            );
        }
        result
    }

    fn delete_user(&self, user: &User) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        // Use the User model's delete method
        user.delete(conn).map_err(DBError::from)
    }

    fn mark_and_delete_user(
        &self,
        user: &User,
        deletion_request: &AccountDeletionRequest,
    ) -> Result<(), DBError> {
        debug!(
            "Deleting user and marking deletion request as complete for user: {}",
            user.uuid
        );
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        // Run both operations in a transaction to ensure atomicity
        conn.transaction(|tx| {
            // First mark the deletion request as complete
            deletion_request
                .mark_as_deleted(tx)
                .map_err(DBError::from)?;

            // Then delete the user
            user.delete(tx).map_err(DBError::from)?;

            Ok(())
        })
    }

    // ---------- Responses API implementations ----------

    // Conversations
    fn create_conversation(
        &self,
        new_conversation: NewConversation,
    ) -> Result<Conversation, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_conversation.insert(conn).map_err(DBError::from)
    }

    fn create_conversation_project(
        &self,
        new_project: NewConversationProject,
    ) -> Result<ConversationProject, DBError> {
        use crate::models::schema::users;
        use diesel::prelude::*;
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        conn.transaction::<ConversationProject, ResponsesError, _>(|tx| {
            users::table
                .filter(users::uuid.eq(new_project.user_id))
                .select(users::id)
                .for_update()
                .first::<i32>(tx)?;

            let existing_project_count =
                ConversationProject::count_for_user(tx, new_project.user_id)?;
            validate_conversation_project_limit(existing_project_count)?;

            new_project.insert(tx)
        })
        .map_err(DBError::from)
    }

    fn get_conversation_by_id_and_user(
        &self,
        conversation_id: i64,
        user_id: Uuid,
    ) -> Result<Conversation, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Conversation::get_by_id_and_user(conn, conversation_id, user_id).map_err(DBError::from)
    }

    fn get_conversation_by_uuid_and_user(
        &self,
        conversation_uuid: Uuid,
        user_id: Uuid,
    ) -> Result<Conversation, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Conversation::get_by_uuid_and_user(conn, conversation_uuid, user_id).map_err(DBError::from)
    }

    fn get_conversation_project_by_id_and_user(
        &self,
        project_id: i64,
        user_id: Uuid,
    ) -> Result<ConversationProject, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        ConversationProject::get_by_id_and_user(conn, project_id, user_id).map_err(DBError::from)
    }

    fn get_conversation_project_by_uuid_and_user(
        &self,
        project_uuid: Uuid,
        user_id: Uuid,
    ) -> Result<ConversationProject, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        ConversationProject::get_by_uuid_and_user(conn, project_uuid, user_id)
            .map_err(DBError::from)
    }

    fn update_conversation_metadata(
        &self,
        conversation_id: i64,
        user_id: Uuid,
        metadata_enc: Vec<u8>,
    ) -> Result<Conversation, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Conversation::update_metadata(conn, conversation_id, user_id, metadata_enc)
            .map_err(DBError::from)
    }

    fn update_conversation(
        &self,
        conversation_id: i64,
        user_id: Uuid,
        metadata_enc: Option<Vec<u8>>,
        project_id: Option<Option<i64>>,
        is_pinned: Option<bool>,
    ) -> Result<Conversation, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Conversation::update(
            conn,
            conversation_id,
            user_id,
            metadata_enc,
            project_id,
            is_pinned,
        )
        .map_err(DBError::from)
    }

    fn batch_update_conversation_project(
        &self,
        conversation_uuids: &[Uuid],
        user_id: Uuid,
        target_project_id: Option<i64>,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Conversation::batch_update_project(conn, conversation_uuids, user_id, target_project_id)
            .map_err(DBError::from)
    }

    fn list_conversations(
        &self,
        user_id: Uuid,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
        project_filter: ConversationProjectFilter,
        pinned: Option<bool>,
    ) -> Result<Vec<Conversation>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Conversation::list_for_user(conn, user_id, limit, after, order, project_filter, pinned)
            .map_err(DBError::from)
    }

    fn list_conversation_projects(
        &self,
        user_id: Uuid,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
    ) -> Result<Vec<ConversationProject>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        ConversationProject::list_for_user(conn, user_id, limit, after, order)
            .map_err(DBError::from)
    }

    fn update_conversation_project(
        &self,
        project_id: i64,
        user_id: Uuid,
        name_enc: Option<Vec<u8>>,
        instruction_update: ProjectInstructionUpdate,
    ) -> Result<ConversationProject, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        use crate::models::schema::{conversation_projects, user_instructions};
        use diesel::prelude::*;

        conn.transaction(|tx| {
            let target = conversation_projects::table
                .filter(conversation_projects::id.eq(project_id))
                .filter(conversation_projects::user_id.eq(user_id));

            let existing_project = target
                .first::<ConversationProject>(tx)
                .optional()?
                .ok_or(diesel::result::Error::NotFound)?;

            if let Some(name_enc) = name_enc {
                diesel::update(target)
                    .set((
                        conversation_projects::name_enc.eq(name_enc),
                        conversation_projects::updated_at.eq(diesel::dsl::now),
                    ))
                    .execute(tx)?;
            }

            match instruction_update {
                ProjectInstructionUpdate::Unchanged => {}
                ProjectInstructionUpdate::Set {
                    prompt_enc,
                    prompt_tokens,
                } => {
                    let existing_instruction = user_instructions::table
                        .filter(user_instructions::user_id.eq(user_id))
                        .filter(user_instructions::project_id.eq(Some(project_id)))
                        .first::<UserInstruction>(tx)
                        .optional()?;

                    if let Some(instruction) = existing_instruction {
                        diesel::update(
                            user_instructions::table
                                .filter(user_instructions::id.eq(instruction.id)),
                        )
                        .set((
                            user_instructions::prompt_enc.eq(prompt_enc),
                            user_instructions::prompt_tokens.eq(prompt_tokens),
                            user_instructions::is_default.eq(false),
                            user_instructions::updated_at.eq(diesel::dsl::now),
                        ))
                        .execute(tx)?;
                    } else {
                        let new_instruction = NewUserInstruction {
                            uuid: Uuid::new_v4(),
                            user_id,
                            project_id: Some(project_id),
                            name_enc: None,
                            prompt_enc,
                            prompt_tokens,
                            is_default: false,
                        };

                        diesel::insert_into(user_instructions::table)
                            .values(&new_instruction)
                            .execute(tx)?;
                    }

                    diesel::update(target)
                        .set(conversation_projects::updated_at.eq(diesel::dsl::now))
                        .execute(tx)?;
                }
                ProjectInstructionUpdate::Clear => {
                    diesel::delete(
                        user_instructions::table
                            .filter(user_instructions::user_id.eq(user_id))
                            .filter(user_instructions::project_id.eq(Some(project_id))),
                    )
                    .execute(tx)?;

                    diesel::update(target)
                        .set(conversation_projects::updated_at.eq(diesel::dsl::now))
                        .execute(tx)?;
                }
            }

            conversation_projects::table
                .filter(conversation_projects::id.eq(existing_project.id))
                .filter(conversation_projects::user_id.eq(user_id))
                .first::<ConversationProject>(tx)
        })
        .map_err(|e| match e {
            diesel::result::Error::NotFound => {
                DBError::ResponsesError(ResponsesError::ConversationProjectNotFound)
            }
            _ => DBError::ResponsesError(ResponsesError::DatabaseError(e)),
        })
    }

    fn delete_conversation(&self, conversation_id: i64, user_id: Uuid) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Conversation::delete_by_id_and_user(conn, conversation_id, user_id).map_err(DBError::from)
    }

    fn delete_all_conversations(&self, user_id: Uuid) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Conversation::delete_all_for_user(conn, user_id).map_err(DBError::from)
    }

    fn delete_conversation_project(&self, project_id: i64, user_id: Uuid) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        ConversationProject::delete_by_id_and_user(conn, project_id, user_id).map_err(DBError::from)
    }

    // Responses (job tracker) implementations
    fn create_response(&self, new_response: NewResponse) -> Result<Response, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_response.insert(conn).map_err(DBError::from)
    }

    fn create_response_with_message_and_tool_items(
        &self,
        new_response: NewResponse,
        user_message: NewUserMessage,
        tool_items: Vec<(NewToolCall, NewToolOutput)>,
    ) -> Result<PersistedResponseWithToolItems, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_response
            .create_with_message_and_tool_items(conn, user_message, tool_items)
            .map_err(DBError::from)
    }

    fn get_response_by_uuid_and_user(
        &self,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<Response, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Response::get_by_uuid_and_user(conn, uuid, user_id).map_err(DBError::from)
    }

    fn update_response_status(
        &self,
        id: i64,
        status: ResponseStatus,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Response::update_status(conn, id, status, completed_at).map_err(DBError::from)
    }

    fn update_response_status_if_current(
        &self,
        id: i64,
        current_status: ResponseStatus,
        new_status: ResponseStatus,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<bool, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Response::update_status_if_current(conn, id, current_status, new_status, completed_at)
            .map(|rows| rows > 0)
            .map_err(DBError::from)
    }

    fn cancel_response(&self, uuid: Uuid, user_id: Uuid) -> Result<Response, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Response::cancel_by_uuid_and_user(conn, uuid, user_id).map_err(DBError::from)
    }

    fn delete_response(&self, uuid: Uuid, user_id: Uuid) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        Response::delete_by_uuid_and_user(conn, uuid, user_id).map_err(DBError::from)
    }

    fn create_conversation_with_response_and_message(
        &self,
        conversation_uuid: Uuid,
        user_id: Uuid,
        metadata_enc: Option<Vec<u8>>,
        response: Option<NewResponse>,
        first_message_content: Vec<u8>,
        first_message_tokens: i32,
        message_uuid: Uuid,
        assistant_message_uuid: Option<Uuid>,
    ) -> Result<(Conversation, Option<Response>, UserMessage), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        NewConversation::create_with_response_and_message(
            conn,
            conversation_uuid,
            user_id,
            metadata_enc,
            response,
            first_message_content,
            first_message_tokens,
            message_uuid,
            assistant_message_uuid,
        )
        .map_err(DBError::from)
    }

    // User instructions implementations
    fn get_default_user_instruction(
        &self,
        user_id: Uuid,
    ) -> Result<Option<UserInstruction>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        use crate::models::schema::user_instructions;
        use diesel::prelude::*;

        user_instructions::table
            .filter(user_instructions::user_id.eq(user_id))
            .filter(user_instructions::project_id.is_null())
            .filter(user_instructions::is_default.eq(true))
            .first::<UserInstruction>(conn)
            .optional()
            .map_err(|e| DBError::ResponsesError(ResponsesError::DatabaseError(e)))
    }

    fn get_project_instruction(
        &self,
        project_id: i64,
        user_id: Uuid,
    ) -> Result<Option<UserInstruction>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        use crate::models::schema::user_instructions;
        use diesel::prelude::*;

        user_instructions::table
            .filter(user_instructions::user_id.eq(user_id))
            .filter(user_instructions::project_id.eq(Some(project_id)))
            .first::<UserInstruction>(conn)
            .optional()
            .map_err(|e| DBError::ResponsesError(ResponsesError::DatabaseError(e)))
    }

    fn get_project_instruction_for_conversation(
        &self,
        conversation_id: i64,
        user_id: Uuid,
    ) -> Result<Option<UserInstruction>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        use crate::models::schema::{conversations, user_instructions};
        use diesel::prelude::*;

        let project_id = conversations::table
            .filter(conversations::id.eq(conversation_id))
            .filter(conversations::user_id.eq(user_id))
            .select(conversations::project_id)
            .first::<Option<i64>>(conn)
            .optional()
            .map_err(|e| DBError::ResponsesError(ResponsesError::DatabaseError(e)))?
            .ok_or(DBError::ResponsesError(
                ResponsesError::ConversationNotFound,
            ))?;

        match project_id {
            Some(project_id) => user_instructions::table
                .filter(user_instructions::user_id.eq(user_id))
                .filter(user_instructions::project_id.eq(Some(project_id)))
                .first::<UserInstruction>(conn)
                .optional()
                .map_err(|e| DBError::ResponsesError(ResponsesError::DatabaseError(e))),
            None => Ok(None),
        }
    }

    fn get_user_instruction_by_uuid_and_user(
        &self,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<UserInstruction, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        use crate::models::schema::user_instructions;
        use diesel::prelude::*;

        user_instructions::table
            .filter(user_instructions::uuid.eq(uuid))
            .filter(user_instructions::user_id.eq(user_id))
            .filter(user_instructions::project_id.is_null())
            .first::<UserInstruction>(conn)
            .map_err(|e| match e {
                diesel::result::Error::NotFound => {
                    DBError::ResponsesError(ResponsesError::SystemPromptNotFound)
                }
                _ => DBError::ResponsesError(ResponsesError::DatabaseError(e)),
            })
    }

    fn create_user_instruction(
        &self,
        new_instruction: NewUserInstruction,
    ) -> Result<UserInstruction, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        use crate::models::schema::user_instructions;
        use diesel::prelude::*;

        conn.transaction(|tx| {
            // If this instruction should be default, clear other defaults first
            if new_instruction.is_default {
                diesel::update(
                    user_instructions::table
                        .filter(user_instructions::user_id.eq(new_instruction.user_id))
                        .filter(user_instructions::project_id.is_null())
                        .filter(user_instructions::is_default.eq(true)),
                )
                .set(user_instructions::is_default.eq(false))
                .execute(tx)?;
            }

            diesel::insert_into(user_instructions::table)
                .values(&new_instruction)
                .get_result(tx)
        })
        .map_err(|e| DBError::ResponsesError(ResponsesError::DatabaseError(e)))
    }

    fn update_user_instruction(
        &self,
        id: i64,
        user_id: Uuid,
        name_enc: Vec<u8>,
        prompt_enc: Vec<u8>,
        prompt_tokens: i32,
        is_default: bool,
    ) -> Result<UserInstruction, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        use crate::models::schema::user_instructions;
        use diesel::prelude::*;

        conn.transaction(|tx| {
            // If setting this as default, clear other defaults first
            if is_default {
                diesel::update(
                    user_instructions::table
                        .filter(user_instructions::user_id.eq(user_id))
                        .filter(user_instructions::project_id.is_null())
                        .filter(user_instructions::is_default.eq(true))
                        .filter(user_instructions::id.ne(id)),
                )
                .set(user_instructions::is_default.eq(false))
                .execute(tx)?;
            }

            diesel::update(
                user_instructions::table
                    .filter(user_instructions::id.eq(id))
                    .filter(user_instructions::user_id.eq(user_id))
                    .filter(user_instructions::project_id.is_null()),
            )
            .set((
                user_instructions::name_enc.eq(Some(name_enc)),
                user_instructions::prompt_enc.eq(prompt_enc),
                user_instructions::prompt_tokens.eq(prompt_tokens),
                user_instructions::is_default.eq(is_default),
                user_instructions::updated_at.eq(diesel::dsl::now),
            ))
            .get_result(tx)
        })
        .map_err(|e| DBError::ResponsesError(ResponsesError::DatabaseError(e)))
    }

    fn delete_user_instruction(&self, id: i64, user_id: Uuid) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        use crate::models::schema::user_instructions;
        use diesel::prelude::*;

        diesel::delete(
            user_instructions::table
                .filter(user_instructions::id.eq(id))
                .filter(user_instructions::user_id.eq(user_id))
                .filter(user_instructions::project_id.is_null()),
        )
        .execute(conn)
        .map(|rows| {
            if rows == 0 {
                Err(DBError::ResponsesError(
                    ResponsesError::SystemPromptNotFound,
                ))
            } else {
                Ok(())
            }
        })?
    }

    fn list_user_instructions(
        &self,
        user_id: Uuid,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
    ) -> Result<Vec<UserInstruction>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        use crate::models::schema::user_instructions;
        use diesel::prelude::*;

        let mut query = user_instructions::table
            .filter(user_instructions::user_id.eq(user_id))
            .filter(user_instructions::project_id.is_null())
            .into_boxed();

        if let Some(after_uuid) = after {
            let cursor_instruction = user_instructions::table
                .filter(user_instructions::uuid.eq(after_uuid))
                .filter(user_instructions::user_id.eq(user_id))
                .filter(user_instructions::project_id.is_null())
                .select((user_instructions::updated_at, user_instructions::id))
                .first::<(DateTime<Utc>, i64)>(conn)
                .optional()?;

            if let Some((updated_at, id)) = cursor_instruction {
                if order == "desc" {
                    query = query.filter(
                        user_instructions::updated_at.lt(updated_at).or(
                            user_instructions::updated_at
                                .eq(updated_at)
                                .and(user_instructions::id.lt(id)),
                        ),
                    );
                } else {
                    query = query.filter(
                        user_instructions::updated_at.gt(updated_at).or(
                            user_instructions::updated_at
                                .eq(updated_at)
                                .and(user_instructions::id.gt(id)),
                        ),
                    );
                }
            }
        }

        if order == "desc" {
            query = query.order((
                user_instructions::updated_at.desc(),
                user_instructions::id.desc(),
            ));
        } else {
            query = query.order((
                user_instructions::updated_at.asc(),
                user_instructions::id.asc(),
            ));
        }

        query
            .limit(limit)
            .load::<UserInstruction>(conn)
            .map_err(|e| DBError::ResponsesError(ResponsesError::DatabaseError(e)))
    }

    fn set_default_user_instruction(
        &self,
        id: i64,
        user_id: Uuid,
    ) -> Result<UserInstruction, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;

        use crate::models::schema::user_instructions;
        use diesel::prelude::*;

        conn.transaction(|tx| {
            // Clear all defaults for this user
            diesel::update(
                user_instructions::table
                    .filter(user_instructions::user_id.eq(user_id))
                    .filter(user_instructions::project_id.is_null())
                    .filter(user_instructions::is_default.eq(true)),
            )
            .set(user_instructions::is_default.eq(false))
            .execute(tx)?;

            // Set this one as default
            diesel::update(
                user_instructions::table
                    .filter(user_instructions::id.eq(id))
                    .filter(user_instructions::user_id.eq(user_id))
                    .filter(user_instructions::project_id.is_null()),
            )
            .set((
                user_instructions::is_default.eq(true),
                user_instructions::updated_at.eq(diesel::dsl::now),
            ))
            .get_result(tx)
        })
        .map_err(|e| DBError::ResponsesError(ResponsesError::DatabaseError(e)))
    }

    // User messages
    fn create_user_message(&self, new_msg: NewUserMessage) -> Result<UserMessage, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_msg.insert(conn).map_err(DBError::from)
    }

    fn update_user_message_prompt_tokens(
        &self,
        id: i64,
        prompt_tokens: i32,
    ) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        use crate::models::schema::user_messages;
        use diesel::prelude::*;

        diesel::update(user_messages::table.filter(user_messages::id.eq(id)))
            .set(user_messages::prompt_tokens.eq(prompt_tokens))
            .execute(conn)
            .map(|_| ())
            .map_err(|e| DBError::ResponsesError(ResponsesError::DatabaseError(e)))
    }

    fn get_user_message(&self, id: i64, user_id: Uuid) -> Result<UserMessage, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserMessage::get_by_id_and_user(conn, id, user_id).map_err(DBError::from)
    }

    fn get_user_message_by_uuid(&self, uuid: Uuid, user_id: Uuid) -> Result<UserMessage, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserMessage::get_by_uuid_and_user(conn, uuid, user_id).map_err(DBError::from)
    }

    fn user_message_uuid_exists(&self, uuid: Uuid) -> Result<bool, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserMessage::uuid_exists(conn, uuid).map_err(DBError::from)
    }

    // Assistant messages
    fn create_assistant_message(
        &self,
        new_msg: NewAssistantMessage,
    ) -> Result<AssistantMessage, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_msg.insert(conn).map_err(DBError::from)
    }

    fn get_assistant_message_by_uuid(
        &self,
        message_uuid: Uuid,
    ) -> Result<Option<AssistantMessage>, DBError> {
        use crate::models::schema::assistant_messages::dsl::*;
        use diesel::{ExpressionMethods, OptionalExtension, QueryDsl, RunQueryDsl};
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        assistant_messages
            .filter(uuid.eq(message_uuid))
            .first::<AssistantMessage>(conn)
            .optional()
            .map_err(DBError::from)
    }

    fn update_assistant_message(
        &self,
        message_uuid: Uuid,
        content_enc: Option<Vec<u8>>,
        completion_tokens: i32,
        status: String,
        finish_reason: Option<String>,
    ) -> Result<AssistantMessage, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        AssistantMessage::update(
            conn,
            message_uuid,
            content_enc,
            completion_tokens,
            status,
            finish_reason,
        )
        .map_err(DBError::from)
    }

    // Reasoning items
    fn create_reasoning_item(&self, new_item: NewReasoningItem) -> Result<ReasoningItem, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_item.insert(conn).map_err(DBError::from)
    }

    fn get_reasoning_item_by_uuid(
        &self,
        item_uuid: Uuid,
    ) -> Result<Option<ReasoningItem>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        ReasoningItem::get_by_uuid(conn, item_uuid).map_err(DBError::from)
    }

    fn update_reasoning_item(
        &self,
        item_uuid: Uuid,
        content_enc: Option<Vec<u8>>,
        reasoning_tokens: i32,
        status: String,
    ) -> Result<ReasoningItem, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        ReasoningItem::update(conn, item_uuid, content_enc, reasoning_tokens, status)
            .map_err(DBError::from)
    }

    // Tool calls / outputs
    fn create_tool_call(&self, new_call: NewToolCall) -> Result<ToolCall, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_call.insert(conn).map_err(DBError::from)
    }

    fn get_tool_call_by_uuid(&self, uuid: Uuid, user_id: Uuid) -> Result<ToolCall, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        ToolCall::get_by_uuid(conn, uuid, user_id).map_err(DBError::from)
    }

    fn create_tool_output(&self, new_output: NewToolOutput) -> Result<ToolOutput, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        new_output.insert(conn).map_err(DBError::from)
    }

    fn get_tool_output_by_uuid(&self, uuid: Uuid, user_id: Uuid) -> Result<ToolOutput, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        ToolOutput::get_by_uuid(conn, uuid, user_id).map_err(DBError::from)
    }

    // Context reconstruction
    fn get_conversation_context_messages(
        &self,
        conversation_id: i64,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
    ) -> Result<Vec<RawThreadMessage>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        RawThreadMessage::get_conversation_context(conn, conversation_id, limit, after, order)
            .map_err(DBError::from)
    }

    fn get_response_context_messages(
        &self,
        response_id: i64,
    ) -> Result<Vec<RawThreadMessage>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        RawThreadMessage::get_response_context(conn, response_id).map_err(DBError::from)
    }

    // Optimized context reconstruction (metadata-based)
    fn get_conversation_context_metadata(
        &self,
        conversation_id: i64,
    ) -> Result<Vec<RawThreadMessageMetadata>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        RawThreadMessageMetadata::get_conversation_context_metadata(conn, conversation_id)
            .map_err(DBError::from)
    }

    fn get_messages_by_ids(
        &self,
        conversation_id: i64,
        message_ids: &[(String, i64)],
    ) -> Result<Vec<RawThreadMessage>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        RawThreadMessage::get_messages_by_ids(conn, conversation_id, message_ids)
            .map_err(DBError::from)
    }

    fn delete_user_message(&self, id: Uuid, user_id: Uuid) -> Result<(), DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        // First get the message by UUID to find its ID
        let msg = UserMessage::get_by_uuid_and_user(conn, id, user_id)?;
        UserMessage::delete_by_id_and_user(conn, msg.id, user_id).map_err(DBError::from)
    }

    // Recovery wrap helpers
    fn get_recovery_wrap(&self, user_id: Uuid) -> Result<Option<UserSeedWrapping>, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        let wraps = UserSeedWrapping::get_for_user_and_kind(conn, user_id, CredentialKind::Recovery.as_str())
            .map_err(DBError::from)?;
        Ok(wraps.into_iter().next())
    }

    fn insert_recovery_wrap_if_absent(
        &self,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<UserSeedWrapping, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        conn.transaction::<_, DBError, _>(|conn| {
            let existing = UserSeedWrapping::get_for_user_and_kind(
                conn,
                new_wrapping.user_id,
                CredentialKind::Recovery.as_str(),
            )
            .map_err(DBError::from)?;
            if existing.into_iter().next().is_some() {
                return Err(DBError::StaleCredentialState);
            }
            new_wrapping.insert(conn).map_err(|e| match e {
                UserSeedWrappingError::DatabaseError(diesel::result::Error::DatabaseError(
                    diesel::result::DatabaseErrorKind::UniqueViolation,
                    _,
                )) => DBError::StaleCredentialState,
                other => DBError::from(other),
            })
        })
    }

    fn replace_recovery_wrap_if_unchanged(
        &self,
        old_wrapping: &UserSeedWrapping,
        new_wrapping: NewUserSeedWrapping,
    ) -> Result<UserSeedWrapping, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        conn.transaction::<_, DBError, _>(|conn| {
            let current = UserSeedWrapping::get_for_user_and_kind(
                conn,
                old_wrapping.user_id,
                CredentialKind::Recovery.as_str(),
            )
            .map_err(DBError::from)?;

            let current = current.into_iter().next();
            match current {
                Some(existing) if existing.id == old_wrapping.id => {
                    // The `id` check above is a soft guard; the conditional DELETE below is the
                    // actual CAS -- it only removes the exact row we think we’re replacing. If
                    // another transaction already replaced or deleted it, the count will be 0
                    // and we return StaleCredentialState instead of clobbering a newer wrap.
                    use crate::models::schema::user_seed_wrappings;
                    let deleted = diesel::delete(
                        user_seed_wrappings::table
                            .filter(user_seed_wrappings::user_id.eq(old_wrapping.user_id))
                            .filter(user_seed_wrappings::id.eq(old_wrapping.id))
                            .filter(
                                user_seed_wrappings::credential_kind
                                    .eq(CredentialKind::Recovery.as_str()),
                            ),
                    )
                    .execute(conn)
                    .map_err(DBError::from)?;
                    if deleted != 1 {
                        return Err(DBError::StaleCredentialState);
                    }
                    new_wrapping.insert(conn).map_err(DBError::from)
                }
                _ => Err(DBError::StaleCredentialState),
            }
        })
    }

    fn delete_recovery_wrap_for_user(&self, user_id: Uuid) -> Result<usize, DBError> {
        let conn = &mut self.db.get().map_err(|_| DBError::ConnectionError)?;
        UserSeedWrapping::delete_for_user_and_kind(conn, user_id, CredentialKind::Recovery.as_str())
            .map_err(DBError::from)
    }

    fn recovery_wrap_exists(&self, user_id: Uuid) -> Result<bool, DBError> {
        self.get_recovery_wrap(user_id).map(|wrap| wrap.is_some())
    }

    // Maintenance
}

pub(crate) fn setup_db(url: String) -> Arc<dyn DBConnection + Send + Sync> {
    info!("Connecting to database...");
    let manager = ConnectionManager::<PgConnection>::new(url);

    let pool = Pool::builder()
        .max_size(20) // Increased from 1 to support concurrent operations
        .min_idle(Some(5)) // Keep 5 connections ready for faster response
        .connection_timeout(std::time::Duration::from_secs(30))
        .idle_timeout(Some(std::time::Duration::from_secs(600))) // 10 minutes
        .max_lifetime(Some(std::time::Duration::from_secs(1800))) // 30 minutes
        .test_on_check_out(true)
        .build(manager)
        .expect("Unable to build DB connection pool");

    info!("Connected to database with pool size: 20, min idle: 5");
    Arc::new(PostgresConnection { db: pool })
}
