use crate::jwt::{
    validate_access_token_for_auth, AuthContext, TRANSPORT_V2_USER_ACCESS, USER_ACCESS,
};
use crate::transport_v2::envelope::{Credential, CredentialKind};
use crate::web::encryption_middleware::TransportSession;
use crate::ApiError;
use axum::{
    body::Body,
    extract::State,
    http::{header, Request},
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
    let is_transport_v2 = req
        .extensions()
        .get::<TransportSession>()
        .is_some_and(TransportSession::is_v2);
    let presented = if is_transport_v2 {
        match req.extensions().get::<Credential>() {
            Some(credential) if credential.kind() == CredentialKind::ApiKey => {
                PresentedCredential::ApiKey(credential.value())
            }
            Some(credential) if credential.kind() == CredentialKind::Bearer => {
                PresentedCredential::Bearer(credential.value())
            }
            _ => return ApiError::InvalidJwt.into_response(),
        }
    } else {
        let Some(value) = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return ApiError::InvalidJwt.into_response();
        };
        if value.contains('.') {
            PresentedCredential::Bearer(value)
        } else {
            PresentedCredential::ApiKey(value)
        }
    };

    let token = match presented {
        PresentedCredential::ApiKey(api_key) => match Uuid::parse_str(api_key) {
            Ok(api_key_uuid) => {
                let mut hasher = Sha256::new();
                hasher.update(api_key_uuid.to_string().as_bytes());
                let key_hash = format!("{:x}", hasher.finalize());
                match data.db.get_user_by_api_key_hash(&key_hash) {
                    Ok(Some(user)) => {
                        // Best-effort usage tracking: record that this key
                        // authenticated a request, but never fail auth if the
                        // timestamp cannot be written.
                        if let Err(e) = data.db.touch_user_api_key_last_used(&key_hash) {
                            tracing::warn!("Failed to update API key last-used timestamp: {:?}", e);
                        }
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
            Err(_) if is_transport_v2 => return ApiError::Unauthorized.into_response(),
            // Preserve V1's legacy behavior: a non-UUID bearer without dots
            // falls through to JWT validation and returns InvalidJwt.
            Err(_) => api_key,
        },
        PresentedCredential::Bearer(token) => token,
    };

    let (claims, access_token_expired) = match validate_access_token_for_auth(
        token,
        &data,
        if is_transport_v2 {
            TRANSPORT_V2_USER_ACCESS
        } else {
            USER_ACCESS
        },
    ) {
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
    let is_transport_v2 = req
        .extensions()
        .get::<TransportSession>()
        .is_some_and(TransportSession::is_v2);
    let has_credential = if is_transport_v2 {
        req.extensions().get::<Credential>().is_some()
    } else {
        req.headers().contains_key(header::AUTHORIZATION)
    };
    if !has_credential {
        return next.run(req).await;
    }

    validate_openai_auth(state, req, next).await
}
