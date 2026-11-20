use crate::jwt::{validate_access_token_for_auth, AuthContext, TRANSPORT_V2_USER_ACCESS};
use crate::transport_v2::envelope::{Credential, CredentialKind};
use crate::web::encryption_middleware::TransportSession;
use crate::ApiError;
use axum::{
    body::Body,
    extract::State,
    http::Request,
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AuthMethod {
    Jwt,
    ApiKey,
}

#[derive(Clone, Copy)]
enum PresentedCredential<'a> {
    ApiKey(&'a str),
    Bearer(&'a str),
}

pub async fn validate_openai_auth(
    State(data): State<Arc<crate::AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    // Credentials arrive only inside the Transport V2 envelope. A request
    // that did not come through the gateway has no transport session, and
    // outer HTTP headers are never read.
    if req.extensions().get::<TransportSession>().is_none() {
        return ApiError::InvalidJwt.into_response();
    }
    let presented = match req.extensions().get::<Credential>() {
        Some(credential) if credential.kind() == CredentialKind::ApiKey => {
            PresentedCredential::ApiKey(credential.value())
        }
        Some(credential) if credential.kind() == CredentialKind::Bearer => {
            PresentedCredential::Bearer(credential.value())
        }
        _ => return ApiError::InvalidJwt.into_response(),
    };

    let token = match presented {
        PresentedCredential::ApiKey(api_key) => match Uuid::parse_str(api_key) {
            Ok(api_key_uuid) => {
                let mut hasher = Sha256::new();
                hasher.update(api_key_uuid.to_string().as_bytes());
                let key_hash = format!("{:x}", hasher.finalize());
                match data.db.get_user_by_api_key_hash(&key_hash) {
                    Ok(Some(user)) => {
                        req.extensions_mut().insert(user);
                        req.extensions_mut().insert(AuthMethod::ApiKey);
                        return next.run(req).await;
                    }
                    Ok(None) => {
                        tracing::debug!("API key not found in database");
                        return ApiError::Unauthorized.into_response();
                    }
                    Err(e) => {
                        tracing::error!("Database error during API key lookup: {:?}", e);
                        return ApiError::InternalServerError.into_response();
                    }
                }
            }
            Err(_) => return ApiError::Unauthorized.into_response(),
        },
        PresentedCredential::Bearer(token) => token,
    };

    let (claims, access_token_expired) =
        match validate_access_token_for_auth(token, &data, TRANSPORT_V2_USER_ACCESS) {
            Ok(validation) => validation,
            Err(_) => return ApiError::InvalidJwt.into_response(),
        };

    let auth_context = match AuthContext::from_claims(&claims) {
        Ok(auth_context) => auth_context,
        Err(_) => return ApiError::InvalidJwt.into_response(),
    };

    let user_uuid: Uuid = match Uuid::parse_str(&claims.sub) {
        Ok(uuid) => uuid,
        Err(e) => {
            tracing::error!("Error parsing user uuid: {:?}", e);
            return ApiError::InvalidJwt.into_response();
        }
    };

    let user = match data.get_user(user_uuid).await {
        Ok(user) => user,
        Err(e) => {
            tracing::error!("Error getting user: {:?}", e);
            return ApiError::InternalServerError.into_response();
        }
    };

    if user.project_id != auth_context.project_id {
        tracing::error!("JWT auth context project does not match user project");
        return ApiError::InvalidJwt.into_response();
    }

    if let Err(e) = data.verify_seed_wrap_for_auth_context(&user, &auth_context) {
        tracing::error!(
            "OpenAI JWT auth context no longer unwraps an active seed wrap: {:?}",
            e
        );
        return ApiError::InvalidJwt.into_response();
    }

    if access_token_expired {
        return ApiError::AccessTokenExpired.into_response();
    }

    req.extensions_mut().insert(auth_context);
    req.extensions_mut().insert(user);
    req.extensions_mut().insert(AuthMethod::Jwt);
    next.run(req).await
}

/// Authenticate the dedicated models route when current OpenAI credentials
/// are present. The route's attested encryption session is validated
/// independently by its encryption middleware.
pub async fn validate_optional_openai_auth(
    state: State<Arc<crate::AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if req.extensions().get::<Credential>().is_none() {
        return next.run(req).await;
    }

    validate_openai_auth(state, req, next).await
}
