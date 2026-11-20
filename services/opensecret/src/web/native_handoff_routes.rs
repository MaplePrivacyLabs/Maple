use std::{str::FromStr, sync::Arc};

use axum::{
    extract::State, middleware::from_fn_with_state, response::Response, routing::post, Extension,
    Router,
};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{
    db::DBError,
    jwt::{
        issue_native_handoff_grant, validate_jwt, validate_native_handoff_grant, AuthContext,
        NewToken, TokenType,
    },
    models::users::User,
    provider_cache::CacheNamespaceRoot,
    transport_v2::{
        crypto::SessionId,
        envelope::{Credential, RequestId},
    },
    web::{
        encryption_middleware::{decrypt_request, encrypt_response, Decrypted, TransportSession},
        login_routes::AuthResponse,
    },
    ApiError, AppState,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeHandoffGrantRequest {
    native_session_id: String,
    native_request_id: RequestId,
}

#[derive(Serialize)]
struct NativeHandoffGrantResponse<'a> {
    grant: &'a str,
    expires_at: u64,
}

#[derive(Clone, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
struct NativeHandoffRedeemRequest {
    grant: String,
}

pub(crate) fn router(app_state: Arc<AppState>) -> Router<()> {
    let grant = Router::new()
        .route(
            "/auth/native-handoff/grant",
            post(create_native_handoff_grant).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<NativeHandoffGrantRequest>,
            )),
        )
        .route_layer(from_fn_with_state(app_state.clone(), validate_jwt));
    let redeem = Router::new().route(
        "/auth/native-handoff/redeem",
        post(redeem_native_handoff).layer(from_fn_with_state(
            app_state.clone(),
            decrypt_request::<NativeHandoffRedeemRequest>,
        )),
    );
    grant.merge(redeem).with_state(app_state)
}

async fn create_native_handoff_grant(
    State(app_state): State<Arc<AppState>>,
    Extension(session): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Decrypted(request): Decrypted<NativeHandoffGrantRequest>,
) -> Result<Response, ApiError> {
    let target_session_id =
        SessionId::from_str(&request.native_session_id).map_err(|_| ApiError::BadRequest)?;
    let mut issued = issue_native_handoff_grant(
        &user,
        &auth_context,
        target_session_id,
        request.native_request_id,
        &app_state,
    )?;
    let expires_at =
        u64::try_from(issued.expires_at.timestamp()).map_err(|_| ApiError::InternalServerError)?;
    let response = encrypt_response(
        &app_state,
        &session,
        &NativeHandoffGrantResponse {
            grant: &issued.grant,
            expires_at,
        },
    )
    .await;
    issued.grant.zeroize();
    response
}

async fn redeem_native_handoff(
    State(app_state): State<Arc<AppState>>,
    Extension(session): Extension<TransportSession>,
    Extension(request_id): Extension<RequestId>,
    credential: Option<Extension<Credential>>,
    cache_root: Option<Extension<CacheNamespaceRoot>>,
    Decrypted(request): Decrypted<NativeHandoffRedeemRequest>,
) -> Result<Response, ApiError> {
    let session_id = session.session_id();
    if credential.is_some() || cache_root.is_some() {
        return Err(ApiError::BadRequest);
    }

    let (user_id, auth_context) =
        validate_native_handoff_grant(&request.grant, session_id, request_id, &app_state)?;
    let user = app_state
        .db
        .get_user_by_uuid(user_id)
        .map_err(|error| match error {
            DBError::UserNotFound => ApiError::InvalidJwt,
            _ => ApiError::InternalServerError,
        })?;
    if user.project_id != auth_context.project_id
        || app_state
            .verify_seed_wrap_for_auth_context(&user, &auth_context)
            .is_err()
    {
        return Err(ApiError::InvalidJwt);
    }

    let access_token = NewToken::new_with_auth_context(
        &user,
        TokenType::TransportV2Access,
        &app_state,
        &auth_context,
    )?;
    let refresh_token = NewToken::new_with_auth_context(
        &user,
        TokenType::TransportV2Refresh,
        &app_state,
        &auth_context,
    )?;
    encrypt_response(
        &app_state,
        &session,
        &AuthResponse {
            id: user.uuid,
            email: user.email,
            access_token: access_token.token,
            refresh_token: refresh_token.token,
        },
    )
    .await
}
