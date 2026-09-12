use crate::transport_v2::envelope::{Credential, CredentialKind};
use crate::User;
use crate::{
    db::DBError,
    email::{send_hello_email, send_verification_email},
    jwt::{validate_token, AuthContext, NewToken, TokenType},
    models::email_verification::NewEmailVerification,
};
use crate::{jwt::USER_REFRESH, web::encryption_middleware::TransportSession};
use crate::{
    web::encryption_middleware::{
        decrypt_request, encrypt_response, require_transport_v2, require_v2_transport_session,
        Decrypted,
    },
    Error,
};
use crate::{ApiError, AppState};
use axum::{
    body::{Body, HttpBody},
    extract::{Path, State},
    http::Request,
    middleware::{from_fn, from_fn_with_state, Next},
    response::Response,
    routing::{get, post},
    Extension, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::spawn;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Deserialize, Clone)]
pub struct PasswordResetRequestPayload {
    email: String,
    hashed_secret: String,
    client_id: Uuid,
}

#[derive(Deserialize, Clone)]
pub struct PasswordResetConfirmPayload {
    email: String,
    alphanumeric_code: String,
    plaintext_secret: String,
    new_password: String,
    client_id: Uuid,
}

/// The existing email reset proof for v2 reset routes: the emailed
/// alphanumeric code, the client reset secret established at reset-request
/// time, and the coordinates (email, client_id) that scope the account.
/// Reused unchanged by the read-only options route and the completion route.
#[derive(Clone, Deserialize)]
pub struct PasswordResetV2Proof {
    pub email: String,
    pub alphanumeric_code: String,
    pub plaintext_secret: String,
    pub client_id: Uuid,
}

#[derive(Deserialize, Clone)]
pub struct PasswordResetV2OptionsRequest {
    pub proof: PasswordResetV2Proof,
}

#[derive(Serialize)]
pub struct PasswordResetV2OptionsResponse {
    pub recovery_enrolled: bool,
    pub destructive_reset_available: bool,
}

/// The informed choice between the two V2 reset completions. Preserve keeps
/// the enrolled seed by presenting the recovery code; Destructive discards
/// every encrypted credential and must explicitly acknowledge the data loss.
#[derive(Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum CompletePasswordResetMode {
    Preserve { recovery_code: String },
    Destructive { acknowledge_data_loss: bool },
}

#[derive(Deserialize, Clone)]
pub struct CompletePasswordResetV2Request {
    pub proof: PasswordResetV2Proof,
    pub new_password: String,
    pub mode: CompletePasswordResetMode,
}

#[derive(Serialize)]
pub struct CompletePasswordResetV2Response {
    pub message: String,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Deserialize, Clone)]
pub struct Credentials {
    pub email: Option<String>,
    pub id: Option<Uuid>,
    pub password: String,
    pub client_id: Uuid,
}

#[derive(Deserialize, Clone)]
pub struct RegisterCredentials {
    pub name: Option<String>,
    pub email: Option<String>,
    pub password: String,
    pub client_id: Uuid,
}

pub fn router(app_state: Arc<AppState>) -> Router<()> {
    Router::new()
        .route(
            "/login",
            post(login).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<Credentials>,
            )),
        )
        .route(
            "/register",
            post(register).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<RegisterCredentials>,
            )),
        )
        .route(
            "/logout",
            post(logout).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<LogoutRequest>,
            )),
        )
        .route(
            "/refresh",
            post(refresh_token).layer(from_fn_with_state(
                app_state.clone(),
                prepare_refresh_request,
            )),
        )
        .route(
            "/verify-email/:code",
            get(verify_email).layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/password-reset/request",
            post(password_reset_request).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<PasswordResetRequestPayload>,
            )),
        )
        .route(
            "/password-reset/confirm",
            post(password_reset_confirm).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<PasswordResetConfirmPayload>,
            )),
        )
        // The v2 options sub-router keeps its own v2-transport gate, merged in
        // after the legacy defenses so only v2 reset routes require it.
        .merge(password_reset_v2_router(app_state.clone()))
        .with_state(app_state)
}

/// The transport-v2 password-reset options sub-router.
///
/// Layer ordering within this sub-router places `require_transport_v2`
/// outermost, so legacy v1 transport sessions are rejected before any request
/// body is decrypted or reset proof is verified. The state is intentionally
/// kept so `router()` erases it once with `.with_state`.
fn password_reset_v2_router(app_state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/password-reset/v2/options",
            post(password_reset_v2_options).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<PasswordResetV2OptionsRequest>,
            )),
        )
        .route(
            "/password-reset/v2/complete",
            post(password_reset_v2_complete).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<CompletePasswordResetV2Request>,
            )),
        )
        .route_layer(from_fn(require_transport_v2))
}

#[derive(Serialize)]
pub struct AuthResponse {
    pub id: Uuid,
    pub email: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RefreshRequest {
    refresh_token: String,
}

#[derive(Serialize)]
pub struct RefreshResponse {
    access_token: String,
    refresh_token: String,
}

async fn prepare_refresh_request(
    State(state): State<Arc<AppState>>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let is_transport_v2 = request
        .extensions()
        .get::<TransportSession>()
        .is_some_and(TransportSession::is_v2);
    if is_transport_v2 {
        if request.body().size_hint().exact() != Some(0) {
            return Err(ApiError::BadRequest);
        }
        let refresh_token = request
            .extensions()
            .get::<Credential>()
            .filter(|credential| credential.kind() == CredentialKind::Resumption)
            .map(|credential| credential.value().to_string())
            .ok_or(ApiError::InvalidJwt)?;
        request
            .extensions_mut()
            .insert(RefreshRequest { refresh_token });
        return Ok(next.run(request).await);
    }

    let headers = request.headers().clone();
    decrypt_request::<RefreshRequest>(State(state), headers, request, next).await
}

#[derive(Deserialize, Clone)]
pub struct LogoutRequest {
    refresh_token: String,
}

pub async fn login(
    State(data): State<Arc<AppState>>,
    Decrypted(creds): Decrypted<Credentials>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    tracing::trace!("call login");

    let auth_response = login_internal(data.clone(), creds, session_id.is_v2()).await?;
    let result = encrypt_response(&data, &session_id, &auth_response).await;
    result
}

async fn login_internal(
    data: Arc<AppState>,
    creds: Credentials,
    is_transport_v2: bool,
) -> Result<AuthResponse, ApiError> {
    // First get the project by client_id and verify it's active
    let project = data
        .db
        .get_org_project_by_client_id(creds.client_id)
        .map_err(|_| ApiError::BadRequest)?;

    // Get user based on provided credentials, scoped to project
    let user = match (&creds.email, &creds.id) {
        (Some(email), _) => {
            // Try email first if provided
            match data.db.get_user_by_email(email.clone(), project.id) {
                Ok(user) => user,
                Err(DBError::UserNotFound) => {
                    error!("User not found by provided login identifier");
                    return Err(ApiError::InvalidUsernameOrPassword);
                }
                Err(e) => {
                    error!("Error fetching user by email: {:?}", e);
                    return Err(ApiError::InternalServerError);
                }
            }
        }
        (None, Some(id)) => {
            // Only allow ID-based login for guest users
            match data.db.get_user_by_uuid(*id) {
                Ok(user) => {
                    if !user.is_guest() {
                        error!("ID-based login not allowed for users with email addresses");
                        return Err(ApiError::InvalidUsernameOrPassword);
                    }
                    // Verify user belongs to the specified project
                    if user.project_id != project.id {
                        error!("User does not belong to specified project");
                        return Err(ApiError::InvalidUsernameOrPassword);
                    }
                    user
                }
                Err(DBError::UserNotFound) => {
                    error!("User not found by ID: {id}");
                    return Err(ApiError::InvalidUsernameOrPassword);
                }
                Err(e) => {
                    error!("Error fetching user by ID: {:?}", e);
                    return Err(ApiError::InternalServerError);
                }
            }
        }
        (None, None) => {
            error!("Neither email nor ID provided for login");
            return Err(ApiError::InvalidUsernameOrPassword);
        }
    };

    // Check if the user is an OAuth-only user
    if user.password_enc.is_none() {
        error!("Attempted password login for OAuth-only user");
        return Err(ApiError::InvalidUsernameOrPassword);
    }

    // Proceed with password authentication
    match data
        .authenticate_user(creds.email, creds.id, creds.password, project.id)
        .await
    {
        Ok(Some(authenticated_user)) => {
            let access_token = NewToken::new_with_auth_context(
                &authenticated_user.user,
                TokenType::access_for_transport(is_transport_v2),
                &data,
                &authenticated_user.auth_context,
            )?;
            let refresh_token = NewToken::new_with_auth_context(
                &authenticated_user.user,
                TokenType::refresh_for_transport(is_transport_v2),
                &data,
                &authenticated_user.auth_context,
            )?;
            let auth_response = AuthResponse {
                id: authenticated_user.user.get_id(),
                email: authenticated_user.user.get_email().map(|s| s.to_string()),
                access_token: access_token.token,
                refresh_token: refresh_token.token,
            };
            Ok(auth_response)
        }
        Ok(None) => {
            error!("Invalid password attempt");
            Err(ApiError::InvalidUsernameOrPassword)
        }
        Err(e) => {
            error!("Error authenticating user: {:?}", e);
            Err(ApiError::InternalServerError)
        }
    }
}

pub async fn logout(
    State(data): State<Arc<AppState>>,
    Decrypted(logout_request): Decrypted<LogoutRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    info!("Logout request received");
    // TODO actually delete the refresh token
    drop(logout_request.refresh_token);
    let response = json!({ "message": "Logged out successfully" });
    let result = encrypt_response(&data, &session_id, &response).await;
    result
}

pub async fn register(
    State(data): State<Arc<AppState>>,
    Decrypted(creds): Decrypted<RegisterCredentials>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    tracing::trace!("call register");

    let user = match data.register_user(creds.clone()).await {
        Ok(user) => user,
        Err(Error::UserAlreadyExists) => {
            tracing::warn!("Cannot register user that already exists");
            return Err(ApiError::EmailAlreadyExists);
        }
        Err(e) => {
            tracing::error!("Error registering user: {:?}", e);
            return Err(ApiError::InternalServerError);
        }
    };

    // Handle new user registration
    handle_new_user_registration(&data, &user, true).await?;

    // After registration, proceed with login
    let login_result = login_internal(
        data.clone(),
        Credentials {
            email: creds.email,
            id: Some(user.uuid),
            password: creds.password,
            client_id: creds.client_id,
        },
        session_id.is_v2(),
    )
    .await?;

    let result = encrypt_response(&data, &session_id, &login_result).await;
    result
}

pub async fn handle_new_user_registration(
    data: &AppState,
    user: &User,
    requires_email_verification: bool,
) -> Result<(), ApiError> {
    // Only handle email verification if user has an email
    if requires_email_verification && !user.is_guest() {
        // Create email verification entry
        let new_verification = NewEmailVerification::new(user.uuid, 24, false);
        let verification = match data.db.create_email_verification(new_verification) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Error creating email verification: {:?}", e);
                return Err(ApiError::InternalServerError);
            }
        };

        // Send verification email in the background
        if let Some(email) = user.get_email() {
            let email = email.to_string();
            let verification_code = verification.verification_code;
            let data = data.clone();
            let project_id = user.project_id;
            spawn(async move {
                if let Err(e) =
                    send_verification_email(&data, project_id, email, verification_code).await
                {
                    tracing::error!("Could not send verification email: {e}");
                }
            });
        }
    }

    // Only send welcome email if user has an email
    if !user.is_guest() {
        let welcome_email = user.get_email().unwrap().to_string(); // Safe to unwrap since we checked is_guest()
        let data = data.clone();
        let project_id = user.project_id;
        spawn(async move {
            if let Err(e) = send_hello_email(&data, project_id, welcome_email).await {
                tracing::error!("Could not schedule welcome email: {e}");
            }
        });
    }

    Ok(())
}

pub async fn refresh_token(
    State(data): State<Arc<AppState>>,
    Decrypted(refresh_request): Decrypted<RefreshRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    info!("Refresh token request received");

    let refresh_audience = if session_id.is_v2() {
        crate::jwt::TRANSPORT_V2_USER_REFRESH
    } else {
        USER_REFRESH
    };
    let claims = validate_token(&refresh_request.refresh_token, &data, refresh_audience)?;

    // Audience check is now handled by validate_token
    let user_id = Uuid::parse_str(&claims.sub).map_err(|_| ApiError::InvalidJwt)?;

    let user = data
        .get_user(user_id)
        .await
        .map_err(|_| ApiError::Unauthorized)?;

    let auth_context = AuthContext::from_claims(&claims)?;
    data.verify_seed_wrap_for_auth_context(&user, &auth_context)
        .map_err(|_| ApiError::InvalidJwt)?;

    let new_access_token = NewToken::new_with_auth_context(
        &user,
        TokenType::access_for_transport(session_id.is_v2()),
        &data,
        &auth_context,
    )?;
    let new_refresh_token = NewToken::new_with_auth_context(
        &user,
        TokenType::refresh_for_transport(session_id.is_v2()),
        &data,
        &auth_context,
    )?;

    let response = RefreshResponse {
        access_token: new_access_token.token,
        refresh_token: new_refresh_token.token,
    };
    let result = encrypt_response(&data, &session_id, &response).await;
    result
}

pub async fn verify_email(
    State(data): State<Arc<AppState>>,
    Path(code): Path<Uuid>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    let verification = match data.db.get_email_verification_by_code(code) {
        Ok(v) => v,
        Err(DBError::EmailVerificationNotFound) => return Err(ApiError::BadRequest),
        Err(_) => return Err(ApiError::InternalServerError),
    };

    if verification.is_expired() {
        return Err(ApiError::BadRequest);
    }

    if verification.is_verified {
        let response = json!({
            "message": "Email already verified"
        });
        return encrypt_response(&data, &session_id, &response).await;
    }

    let mut verification = verification;
    if data.db.verify_email(&mut verification).is_err() {
        return Err(ApiError::InternalServerError);
    }

    let response = json!({
        "message": "Email verified successfully"
    });
    let result = encrypt_response(&data, &session_id, &response).await;
    result
}

pub async fn password_reset_request(
    State(data): State<Arc<AppState>>,
    Decrypted(payload): Decrypted<PasswordResetRequestPayload>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    // Get project by client_id
    let project = data
        .db
        .get_org_project_by_client_id(payload.client_id)
        .map_err(|_| ApiError::BadRequest)?;

    // Check if user exists and is not an OAuth-only user
    match data.db.get_user_by_email(payload.email.clone(), project.id) {
        Ok(user) => {
            if user.password_enc.is_none() {
                error!("OAuth-only user attempted to reset password");
                // Still return success to not leak information about the account
                let response = json!({
                    "message": "If an account with that email exists, we have sent a password reset link."
                });
                return encrypt_response(&data, &session_id, &response).await;
            }
        }
        Err(DBError::UserNotFound) => {
            // User doesn't exist, but we don't want to leak this information
            let response = json!({
                "message": "If an account with that email exists, we have sent a password reset link."
            });
            return encrypt_response(&data, &session_id, &response).await;
        }
        Err(e) => {
            error!("Error in password reset request: {:?}", e);
            return Err(ApiError::InternalServerError);
        }
    }

    // Proceed with password reset request
    let _ = data
        .create_password_reset_request(payload.email.clone(), payload.hashed_secret, project.id)
        .await
        .map_err(|e| {
            error!("Error in create_password_reset_request: {:?}", e);
            // We don't expose this error to the user
        });

    let response = json!({
        "message": "If an account with that email exists, we have sent a password reset link."
    });
    let result = encrypt_response(&data, &session_id, &response).await;
    result
}

pub async fn password_reset_confirm(
    State(data): State<Arc<AppState>>,
    Decrypted(payload): Decrypted<PasswordResetConfirmPayload>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    // Get project by client_id
    let project = data
        .db
        .get_org_project_by_client_id(payload.client_id)
        .map_err(|_| ApiError::BadRequest)?;

    // Check if user exists and is not an OAuth-only user
    match data.db.get_user_by_email(payload.email.clone(), project.id) {
        Ok(user) => {
            if user.password_enc.is_none() {
                error!("OAuth-only user attempted to reset password");
                return Err(ApiError::InvalidUsernameOrPassword);
            }
        }
        Err(DBError::UserNotFound) => {
            error!("User not found in password reset confirm");
            return Err(ApiError::InvalidUsernameOrPassword);
        }
        Err(e) => {
            error!("Error in password reset confirm: {:?}", e);
            return Err(ApiError::InternalServerError);
        }
    }

    // Proceed with password reset confirmation
    data.confirm_password_reset(
        payload.email,
        payload.alphanumeric_code,
        payload.plaintext_secret,
        payload.new_password,
        project.id,
    )
    .await
    .map_err(|e| match e {
        crate::Error::PasswordResetExpired => ApiError::BadRequest,
        crate::Error::InvalidPasswordResetSecret => ApiError::BadRequest,
        crate::Error::InvalidPasswordResetRequest => ApiError::BadRequest,
        _ => ApiError::InternalServerError,
    })?;

    let response = json!({
        "message": "Password reset successful. You can now log in with your new password."
    });
    let result = encrypt_response(&data, &session_id, &response).await;
    result
}

/// Maps reset-proof rejections to one sanitized status so the options route
/// cannot distinguish unknown accounts from wrong codes or wrong secrets.
/// Only proof rejections stay generic: infrastructure failures remain
/// internal-server errors and never leak into a successful response.
fn map_reset_proof_error(e: Error) -> ApiError {
    match e {
        Error::UserNotFound
        | Error::PasswordResetExpired
        | Error::InvalidPasswordResetSecret
        | Error::InvalidPasswordResetRequest
        | Error::DatabaseError(DBError::UserNotFound) => ApiError::BadRequest,
        _ => ApiError::InternalServerError,
    }
}

/// Transport-v2 password reset options.
///
/// Verifies the existing email reset proof — the same proof legacy
/// `/password-reset/confirm` verifies, mapped onto one sanitized error — and
/// only after it succeeds reveals whether recovery is enrolled. The check is
/// read-only: the reset request is neither consumed nor modified, so
/// repeated calls stay safe until the request expires or is consumed by a
/// completion. An unauthenticated caller learns nothing about account state.
pub async fn password_reset_v2_options(
    State(data): State<Arc<AppState>>,
    Extension(transport_session): Extension<TransportSession>,
    Decrypted(request): Decrypted<PasswordResetV2OptionsRequest>,
) -> Result<Response, ApiError> {
    // Defense-in-depth: the sub-router middleware already rejected non-v2
    // transports before decryption; this guard keeps the handler
    // self-sufficient if it is ever reachable from a mis-wired router.
    require_v2_transport_session(&transport_session)?;

    // The project client id scopes the email to exactly one account space.
    let project = data
        .db
        .get_org_project_by_client_id(request.proof.client_id)
        .map_err(|_| ApiError::BadRequest)?;

    let (user, _active_request) = data
        .verify_password_reset_proof(
            request.proof.email,
            request.proof.alphanumeric_code,
            request.proof.plaintext_secret,
            project.id,
        )
        .map_err(map_reset_proof_error)?;

    // Read-only reveal, after the proof succeeded.
    let recovery_enrolled = data
        .db
        .recovery_wrap_exists(user.uuid)
        .map_err(|_| ApiError::InternalServerError)?;

    let response = PasswordResetV2OptionsResponse {
        recovery_enrolled,
        destructive_reset_available: true,
    };
    encrypt_response(&data, &transport_session, &response).await
}

/// Maps seed-preserving completion rejections to one sanitized status:
/// malformed codes, well-formed codes that failed to open the wrap,
/// unenrolled accounts, and every lost commit-time race share the same
/// generic 400 body, so the route cannot distinguish them. Infrastructure
/// failures stay internal-server errors and never leak into a response.
fn map_preserving_completion_error(e: Error) -> ApiError {
    match e {
        Error::InvalidRecoveryCode
        | Error::AuthenticationError
        | Error::RecoveryNotEnrolled
        | Error::InvalidPasswordResetRequest
        | Error::PasswordResetExpired
        | Error::InvalidPasswordResetSecret
        | Error::DatabaseError(DBError::UserNotFound)
        | Error::DatabaseError(DBError::PasswordResetRequestNotFound)
        | Error::DatabaseError(DBError::StaleCredentialState) => ApiError::BadRequest,
        _ => ApiError::InternalServerError,
    }
}

/// Transport-v2 password reset completion.
///
/// Re-verifies the existing email reset proof — the same read-only proof the
/// options route verifies — and then performs the requested completion:
///
/// - **Preserve** opens the enrolled recovery wrap with the submitted
///   recovery code and installs a new password credential over the same
///   seed. The recovery wrap, seed-key-encrypted data, and OAuth
///   connections are left unchanged.
/// - **Destructive** requires an explicit data-loss acknowledgment and
///   reuses the legacy destructive reset path unchanged; it creates no
///   recovery wrap.
///
/// Both modes revalidate the reset request at commit time inside the
/// transaction that mutates credentials, so a concurrent completion,
/// password change, disablement, or rotation either wins the whole commit
/// or loses without consuming anything. On success the caller receives new
/// access and refresh tokens bound to the new password credential.
pub async fn password_reset_v2_complete(
    State(data): State<Arc<AppState>>,
    Extension(transport_session): Extension<TransportSession>,
    Decrypted(request): Decrypted<CompletePasswordResetV2Request>,
) -> Result<Response, ApiError> {
    // Defense-in-depth: the sub-router middleware already rejected non-v2
    // transports before decryption; this guard keeps the handler
    // self-sufficient if it is ever reachable from a mis-wired router.
    require_v2_transport_session(&transport_session)?;

    // Destructive reset must be an explicit, informed choice before any
    // proof verification or account lookup.
    if matches!(
        request.mode,
        CompletePasswordResetMode::Destructive {
            acknowledge_data_loss: false
        }
    ) {
        return Err(ApiError::BadRequest);
    }

    // The project client id scopes the email to exactly one account space.
    let project = data
        .db
        .get_org_project_by_client_id(request.proof.client_id)
        .map_err(|_| ApiError::BadRequest)?;

    let (user, selected_request) = data
        .verify_password_reset_proof(
            request.proof.email.clone(),
            request.proof.alphanumeric_code.clone(),
            request.proof.plaintext_secret.clone(),
            project.id,
        )
        .map_err(map_reset_proof_error)?;

    let new_auth_context = match request.mode {
        CompletePasswordResetMode::Preserve { recovery_code } => data
            .complete_preserving_password_reset_v2(
                &user,
                &selected_request,
                &recovery_code,
                request.new_password,
            )
            .await
            .map_err(map_preserving_completion_error)?,
        CompletePasswordResetMode::Destructive {
            acknowledge_data_loss: true,
        } => {
            // Reuse the legacy destructive reset path unchanged: it
            // reverifies the proof, reseeds, disconnects OAuth, preserves
            // API keys, and installs the new password wrap without creating
            // recovery state.
            data.confirm_password_reset(
                request.proof.email,
                request.proof.alphanumeric_code,
                request.proof.plaintext_secret,
                request.new_password,
                project.id,
            )
            .await
            .map_err(map_reset_proof_error)?
        }
        CompletePasswordResetMode::Destructive {
            acknowledge_data_loss: false,
        } => {
            debug_assert!(false, "checked before the proof verification");
            return Err(ApiError::BadRequest);
        }
    };

    // Tokens are bound to the new password credential, matching the session
    // transport the completion arrived on.
    let access_token = NewToken::new_with_auth_context(
        &user,
        TokenType::access_for_transport(transport_session.is_v2()),
        &data,
        &new_auth_context,
    )?;
    let refresh_token = NewToken::new_with_auth_context(
        &user,
        TokenType::refresh_for_transport(transport_session.is_v2()),
        &data,
        &new_auth_context,
    )?;

    let response = CompletePasswordResetV2Response {
        message: "Password reset successful.".to_string(),
        access_token: access_token.token,
        refresh_token: refresh_token.token,
    };
    encrypt_response(&data, &transport_session, &response).await
}
