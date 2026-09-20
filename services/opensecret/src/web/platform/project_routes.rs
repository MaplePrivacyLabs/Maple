use crate::{
    models::{
        org_memberships::OrgRole,
        org_project_secrets::NewOrgProjectSecret,
        org_projects::NewOrgProject,
        platform_users::PlatformUser,
        project_settings::{EmailSettings, OAuthSettings},
    },
    web::encryption_middleware::{decrypt_request, encrypt_response, Decrypted, TransportSession},
    ApiError, AppState,
};
use axum::routing::put;
use axum::{
    extract::{Path, State},
    middleware::from_fn_with_state,
    response::Response,
    routing::{delete, get, patch, post},
    Extension, Router,
};
use base64::engine::general_purpose;
use base64::Engine as _;
use secp256k1::SecretKey;
use std::sync::Arc;
use tracing::{debug, error};
use uuid::Uuid;
use validator::Validate;

use super::common::{
    CreateProjectRequest, CreateSecretRequest, ProjectResponse, SecretResponse,
    UpdateEmailSettingsRequest, UpdateOAuthSettingsRequest, UpdateProjectRequest,
};

pub fn router(app_state: Arc<AppState>) -> Router {
    Router::new()
        // Project routes
        .route(
            "/platform/orgs/:org_id/projects",
            post(create_project).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<CreateProjectRequest>,
            )),
        )
        .route(
            "/platform/orgs/:org_id/projects",
            get(list_projects).layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/platform/orgs/:org_id/projects/:project_id",
            patch(update_project).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<UpdateProjectRequest>,
            )),
        )
        .route(
            "/platform/orgs/:org_id/projects/:project_id",
            get(get_project).layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/platform/orgs/:org_id/projects/:project_id",
            delete(delete_project)
                .layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/platform/orgs/:org_id/projects/:project_id/secrets",
            post(create_secret).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<CreateSecretRequest>,
            )),
        )
        .route(
            "/platform/orgs/:org_id/projects/:project_id/secrets",
            get(list_secrets).layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/platform/orgs/:org_id/projects/:project_id/secrets/:key_name",
            delete(delete_secret)
                .layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        // Project settings routes - keep only specialized endpoints
        .route(
            "/platform/orgs/:org_id/projects/:project_id/settings/email",
            get(get_email_settings)
                .layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/platform/orgs/:org_id/projects/:project_id/settings/email",
            put(update_email_settings).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<UpdateEmailSettingsRequest>,
            )),
        )
        .route(
            "/platform/orgs/:org_id/projects/:project_id/settings/oauth",
            get(get_oauth_settings)
                .layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/platform/orgs/:org_id/projects/:project_id/settings/oauth",
            put(update_oauth_settings).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<UpdateOAuthSettingsRequest>,
            )),
        )
        .with_state(app_state)
}

async fn create_project(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path(org_id): Path<Uuid>,
    Decrypted(create_request): Decrypted<CreateProjectRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Creating new project");

    // Validate request
    if create_request.validate().is_err() {
        error!("Project creation request validation failed");
        return Err(ApiError::BadRequest);
    }

    // Get org by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    // Verify user has admin or owner role
    let membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    let role: OrgRole = membership.role.into();
    if !matches!(role, OrgRole::Owner | OrgRole::Admin) {
        return Err(ApiError::Unauthorized);
    }

    // Check if a project with the same name already exists in this organization
    if data
        .db
        .get_org_project_by_name_and_org(&create_request.name, org.id)
        .map_err(|e| {
            error!("Failed to check for existing project: {:?}", e);
            ApiError::InternalServerError
        })?
        .is_some()
    {
        // Project with this name already exists
        error!(
            "Project with name '{}' already exists in this organization",
            &create_request.name
        );
        return Err(ApiError::BadRequest);
    }

    // Create the project
    let new_project = NewOrgProject::new(org.id, create_request.name).with_description(
        create_request
            .description
            .unwrap_or_else(|| String::from("")),
    );

    let project = data.db.create_org_project(new_project).map_err(|e| {
        error!("Failed to create project: {:?}", e);
        ApiError::InternalServerError
    })?;

    let response = ProjectResponse {
        id: project.uuid,
        client_id: project.client_id,
        name: project.name,
        description: project.description,
        status: project.status,
        created_at: project.created_at,
    };

    encrypt_response(&data, &session_id, &response).await
}

async fn list_projects(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path(org_id): Path<Uuid>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Listing projects");

    // Get org by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    // Verify user has any role in the org
    let _membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    // Get all projects
    let projects = data.db.get_all_org_projects_for_org(org.id).map_err(|e| {
        error!("Failed to get projects: {:?}", e);
        ApiError::InternalServerError
    })?;

    let response: Vec<ProjectResponse> = projects
        .into_iter()
        .map(|p| ProjectResponse {
            id: p.uuid,
            client_id: p.client_id,
            name: p.name,
            description: p.description,
            status: p.status,
            created_at: p.created_at,
        })
        .collect();

    encrypt_response(&data, &session_id, &response).await
}

async fn update_project(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Decrypted(update_request): Decrypted<UpdateProjectRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Updating project");

    // Get org and project by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    let project = data
        .db
        .get_org_project_by_uuid(project_id)
        .map_err(|_| ApiError::NotFound)?;

    // Ensure project belongs to org
    if project.org_id != org.id {
        return Err(ApiError::NotFound);
    }

    // Verify user has admin or owner role
    let membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    let role: OrgRole = membership.role.into();
    if !matches!(role, OrgRole::Owner | OrgRole::Admin) {
        return Err(ApiError::Unauthorized);
    }

    // Update the project
    let mut updated_project = project;
    if let Some(name) = update_request.name {
        // If name is changing, check if it conflicts with an existing project
        if name != updated_project.name
            && data
                .db
                .get_org_project_by_name_and_org(&name, org.id)
                .map_err(|e| {
                    error!("Failed to check for existing project: {:?}", e);
                    ApiError::InternalServerError
                })?
                .is_some()
        {
            // Project with this name already exists
            error!(
                "Project with name '{}' already exists in this organization",
                &name
            );
            return Err(ApiError::BadRequest);
        }
        updated_project.name = name;
    }
    if let Some(description) = update_request.description {
        updated_project.description = Some(description);
    }
    if let Some(status) = update_request.status {
        updated_project.status = status;
    }

    data.db.update_org_project(&updated_project).map_err(|e| {
        error!("Failed to update project: {:?}", e);
        ApiError::InternalServerError
    })?;

    let response = ProjectResponse {
        id: updated_project.uuid,
        client_id: updated_project.client_id,
        name: updated_project.name,
        description: updated_project.description,
        status: updated_project.status,
        created_at: updated_project.created_at,
    };

    encrypt_response(&data, &session_id, &response).await
}

async fn get_project(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Getting project");

    // Get org and project by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    // Verify user has any role in the org (read access is allowed for all roles)
    let _membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    let project = data
        .db
        .get_org_project_by_uuid(project_id)
        .map_err(|_| ApiError::NotFound)?;

    // Ensure project belongs to org
    if project.org_id != org.id {
        error!("Project does not belong to organization");
        return Err(ApiError::NotFound);
    }

    // Return the project response
    let response = ProjectResponse {
        id: project.uuid,
        client_id: project.client_id,
        name: project.name,
        description: project.description,
        status: project.status,
        created_at: project.created_at,
    };

    encrypt_response(&data, &session_id, &response).await
}

async fn delete_project(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Deleting project");

    // Get org and project by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    let project = data
        .db
        .get_org_project_by_uuid(project_id)
        .map_err(|_| ApiError::NotFound)?;

    // Ensure project belongs to org
    if project.org_id != org.id {
        return Err(ApiError::NotFound);
    }

    // Verify user has admin or owner role
    let membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    let role: OrgRole = membership.role.into();
    if !matches!(role, OrgRole::Owner | OrgRole::Admin) {
        return Err(ApiError::Unauthorized);
    }

    // Delete the project
    data.db.delete_org_project(&project).map_err(|e| {
        error!("Failed to delete project: {:?}", e);
        ApiError::InternalServerError
    })?;

    let response = serde_json::json!({
        "message": "Project deleted successfully"
    });

    encrypt_response(&data, &session_id, &response).await
}

async fn create_secret(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Decrypted(create_request): Decrypted<CreateSecretRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Creating project secret");

    // Validate request
    if create_request.validate().is_err() {
        error!("Project secret creation request validation failed");
        return Err(ApiError::BadRequest);
    }

    // Get org and project by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    let project = data.db.get_org_project_by_uuid(project_id).map_err(|_| {
        error!("Project not found");
        ApiError::NotFound
    })?;

    // Ensure project belongs to org
    if project.org_id != org.id {
        error!("Project does not belong to organization");
        return Err(ApiError::NotFound);
    }

    // Verify user has admin or owner role
    let membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    let role: OrgRole = membership.role.into();
    if !matches!(role, OrgRole::Owner | OrgRole::Admin) {
        return Err(ApiError::Unauthorized);
    }

    // Decode base64 secret value to raw bytes
    let secret_bytes = general_purpose::STANDARD
        .decode(&create_request.secret)
        .map_err(|e| {
            error!("Invalid base64 encoding: {}", e);
            ApiError::BadRequest
        })?;

    // Encrypt the secret bytes with the enclave key
    let secret_key = SecretKey::from_slice(&data.enclave_key).map_err(|e| {
        error!("Failed to create secret key: {}", e);
        ApiError::InternalServerError
    })?;
    let encrypted_secret = crate::encrypt::encrypt_with_key(&secret_key, &secret_bytes).await;

    // Create the new secret
    let new_secret = NewOrgProjectSecret::new(
        project.id,
        create_request.key_name.clone(),
        encrypted_secret,
    );

    let secret = data.db.create_org_project_secret(new_secret).map_err(|e| {
        error!("Failed to create project secret: {:?}", e);
        ApiError::InternalServerError
    })?;

    let response = SecretResponse {
        key_name: secret.key_name,
        created_at: secret.created_at,
        updated_at: secret.updated_at,
    };

    encrypt_response(&data, &session_id, &response).await
}

async fn list_secrets(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Listing project secrets");

    // Get org and project by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    let project = data.db.get_org_project_by_uuid(project_id).map_err(|_| {
        error!("Project not found");
        ApiError::NotFound
    })?;

    // Ensure project belongs to org
    if project.org_id != org.id {
        error!("Project does not belong to organization");
        return Err(ApiError::NotFound);
    }

    // Verify user has any role in the org
    let _membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    // Get all secrets for the project
    let secrets = data
        .db
        .get_all_org_project_secrets_for_project(project.id)
        .map_err(|e| {
            error!("Failed to get project secrets: {:?}", e);
            ApiError::InternalServerError
        })?;

    let response: Vec<SecretResponse> = secrets
        .into_iter()
        .map(|s| SecretResponse {
            key_name: s.key_name,
            created_at: s.created_at,
            updated_at: s.updated_at,
        })
        .collect();

    encrypt_response(&data, &session_id, &response).await
}

async fn delete_secret(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path((org_id, project_id, key_name)): Path<(Uuid, Uuid, String)>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Deleting project secret");

    // Get org and project by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    let project = data.db.get_org_project_by_uuid(project_id).map_err(|_| {
        error!("Project not found");
        ApiError::NotFound
    })?;

    // Ensure project belongs to org
    if project.org_id != org.id {
        error!("Project does not belong to organization");
        return Err(ApiError::NotFound);
    }

    // Verify user has admin or owner role
    let membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    let role: OrgRole = membership.role.into();
    if !matches!(role, OrgRole::Owner | OrgRole::Admin) {
        return Err(ApiError::Unauthorized);
    }

    // Get the secret
    let secret = data
        .db
        .get_org_project_secret_by_key_name_and_project(&key_name, project.id)
        .map_err(|e| {
            error!("Failed to get project secret: {:?}", e);
            ApiError::InternalServerError
        })?
        .ok_or_else(|| {
            error!("Secret not found");
            ApiError::NotFound
        })?;

    // Delete the secret
    data.db.delete_org_project_secret(&secret).map_err(|e| {
        error!("Failed to delete project secret: {:?}", e);
        ApiError::InternalServerError
    })?;

    let response = serde_json::json!({
        "message": "Secret deleted successfully"
    });

    encrypt_response(&data, &session_id, &response).await
}

async fn get_email_settings(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Getting project email settings");

    // Get org and project by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    let project = data.db.get_org_project_by_uuid(project_id).map_err(|_| {
        error!("Project not found");
        ApiError::NotFound
    })?;

    // Ensure project belongs to org
    if project.org_id != org.id {
        error!("Project does not belong to organization");
        return Err(ApiError::NotFound);
    }

    // Verify user has any role in the org
    let _membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    // Get email settings
    let settings = data
        .db
        .get_project_email_settings(project.id)?
        .ok_or_else(|| {
            error!("Email settings not found");
            ApiError::NotFound
        })?;

    encrypt_response(&data, &session_id, &settings).await
}

async fn update_email_settings(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Decrypted(update_request): Decrypted<UpdateEmailSettingsRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Updating project email settings");

    // Validate request
    if update_request.validate().is_err() {
        error!("Project email settings request validation failed");
        return Err(ApiError::BadRequest);
    }

    // Get org and project by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    let project = data.db.get_org_project_by_uuid(project_id).map_err(|_| {
        error!("Project not found");
        ApiError::NotFound
    })?;

    // Ensure project belongs to org
    if project.org_id != org.id {
        error!("Project does not belong to organization");
        return Err(ApiError::NotFound);
    }

    // Verify user has admin or owner role
    let membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    let role: OrgRole = membership.role.into();
    if !matches!(role, OrgRole::Owner | OrgRole::Admin) {
        return Err(ApiError::Unauthorized);
    }

    // Create email settings
    let email_settings = EmailSettings {
        provider: update_request.provider,
        send_from: update_request.send_from,
        email_verification_url: update_request.email_verification_url,
    };

    // Update settings and get the result back
    let settings = data
        .db
        .update_project_email_settings(project.id, email_settings)?;

    // Get the updated settings from the database to return
    let updated_settings = settings.get_email_settings().map_err(|e| {
        error!("Failed to parse updated email settings: {:?}", e);
        ApiError::InternalServerError
    })?;

    encrypt_response(&data, &session_id, &updated_settings).await
}

async fn get_oauth_settings(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Getting project OAuth settings");

    // Get org and project by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    let project = data.db.get_org_project_by_uuid(project_id).map_err(|_| {
        error!("Project not found");
        ApiError::NotFound
    })?;

    // Ensure project belongs to org
    if project.org_id != org.id {
        error!("Project does not belong to organization");
        return Err(ApiError::NotFound);
    }

    // Verify user has any role in the org
    let _membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    // Get OAuth settings
    let settings = data
        .db
        .get_project_oauth_settings(project.id)?
        .unwrap_or_default();

    encrypt_response(&data, &session_id, &settings).await
}

async fn update_oauth_settings(
    State(data): State<Arc<AppState>>,
    Extension(platform_user): Extension<PlatformUser>,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Decrypted(update_request): Decrypted<UpdateOAuthSettingsRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Updating project OAuth settings");

    // Validate request using validator
    if update_request.validate().is_err() {
        error!("Project OAuth settings request validation failed");
        return Err(ApiError::BadRequest);
    }

    // Additional validation for the relationship between enabled flags and settings
    if update_request.google_oauth_enabled && update_request.google_oauth_settings.is_none() {
        error!("Google OAuth settings must be provided when Google OAuth is enabled");
        return Err(ApiError::BadRequest);
    }

    if update_request.github_oauth_enabled && update_request.github_oauth_settings.is_none() {
        error!("GitHub OAuth settings must be provided when GitHub OAuth is enabled");
        return Err(ApiError::BadRequest);
    }

    if update_request.apple_oauth_enabled && update_request.apple_oauth_settings.is_none() {
        error!("Apple OAuth settings must be provided when Apple OAuth is enabled");
        return Err(ApiError::BadRequest);
    }

    // Get org and project by UUID
    let org = data
        .db
        .get_org_by_uuid(org_id)
        .map_err(|_| ApiError::NotFound)?;

    let project = data.db.get_org_project_by_uuid(project_id).map_err(|_| {
        error!("Project not found");
        ApiError::NotFound
    })?;

    // Ensure project belongs to org
    if project.org_id != org.id {
        error!("Project does not belong to organization");
        return Err(ApiError::NotFound);
    }

    // Verify user has admin or owner role
    let membership = data
        .db
        .get_org_membership_by_platform_user_and_org(platform_user.uuid, org.id)
        .map_err(|_| ApiError::Unauthorized)?;

    let role: OrgRole = membership.role.into();
    if !matches!(role, OrgRole::Owner | OrgRole::Admin) {
        return Err(ApiError::Unauthorized);
    }

    // Create OAuth settings
    let oauth_settings = OAuthSettings {
        google_oauth_enabled: update_request.google_oauth_enabled,
        github_oauth_enabled: update_request.github_oauth_enabled,
        apple_oauth_enabled: update_request.apple_oauth_enabled,
        google_oauth_settings: update_request.google_oauth_settings,
        github_oauth_settings: update_request.github_oauth_settings,
        apple_oauth_settings: update_request.apple_oauth_settings,
    };

    // The database preserves omitted additional callback lists atomically;
    // every other setting retains whole-object replacement semantics.
    let settings = data
        .db
        .update_project_oauth_settings(project.id, oauth_settings)?;

    // Get the updated settings from the database to return
    let updated_settings = settings.get_oauth_settings().map_err(|e| {
        error!("Failed to parse updated oauth settings: {:?}", e);
        ApiError::InternalServerError
    })?;

    encrypt_response(&data, &session_id, &updated_settings).await
}
