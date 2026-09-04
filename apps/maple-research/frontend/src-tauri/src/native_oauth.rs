use crate::native_transport_root::TransportRootState;
use crate::open_secret_config::{configured_pcr0_environment, normalize_api_url};
use maple_sdk::{
    NativeOAuthHandoffGrant, OpenSecretClient, PreparedNativeOAuthHandoff,
    TransportV2CacheNamespaceRoot,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Instant};
use tauri::State;
use tokio::sync::Mutex;

const NATIVE_OAUTH_ATTEMPT_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const NATIVE_OAUTH_NETWORK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BeginNativeOAuthRequest {
    api_url: String,
    cache_namespace_root_base64: String,
}

#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeOAuthPreparation {
    native_oauth_attempt: String,
    session_id: String,
    request_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedeemNativeOAuthRequest {
    handoff_grant: String,
}

#[derive(Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeOAuthAuthentication {
    user_id: String,
    access_token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelNativeOAuthRequest {
    native_oauth_attempt: String,
}

struct PendingNativeOAuth {
    attempt_id: String,
    started_at: Instant,
    client: Arc<OpenSecretClient>,
    prepared: PreparedNativeOAuthHandoff,
}

pub struct NativeOAuthState {
    attempt: Mutex<Option<PendingNativeOAuth>>,
}

impl NativeOAuthState {
    pub fn new() -> Self {
        Self {
            attempt: Mutex::new(None),
        }
    }

    async fn begin(
        &self,
        request: BeginNativeOAuthRequest,
        cache_root: TransportV2CacheNamespaceRoot,
    ) -> Result<NativeOAuthPreparation, String> {
        // Serialize begin, redeem, and cancel. A slow attestation can never
        // publish over a newer local attempt.
        let mut current = self.attempt.lock().await;
        let api_url = normalize_api_url(&request.api_url)?;
        let client = Arc::new(
            OpenSecretClient::new_with_pcr0_environment(api_url, configured_pcr0_environment()?)
                .map_err(|error| map_sdk_error("prepare", &error))?
                .with_cache_namespace_root(cache_root),
        );
        let prepared = tokio::time::timeout(
            NATIVE_OAUTH_NETWORK_TIMEOUT,
            client.prepare_native_oauth_handoff(),
        )
        .await
        .map_err(|_| "Secure authentication setup timed out; please try again".to_string())?
        .map_err(|error| map_sdk_error("prepare", &error))?;
        let attempt_id = new_attempt_id()?;
        let response = NativeOAuthPreparation {
            native_oauth_attempt: attempt_id.clone(),
            session_id: prepared.session_id().to_string(),
            request_id: prepared.request_id().to_string(),
        };
        *current = Some(PendingNativeOAuth {
            attempt_id,
            started_at: Instant::now(),
            client,
            prepared,
        });
        Ok(response)
    }

    async fn redeem(
        &self,
        request: RedeemNativeOAuthRequest,
    ) -> Result<NativeOAuthAuthentication, String> {
        let grant = NativeOAuthHandoffGrant::new(request.handoff_grant)
            .map_err(|error| map_sdk_error("validate", &error))?;
        let mut current = self.attempt.lock().await;
        if current.as_ref().is_some_and(|attempt| {
            Instant::now()
                .checked_duration_since(attempt.started_at)
                .is_none_or(|age| age > NATIVE_OAUTH_ATTEMPT_TTL)
        }) {
            *current = None;
            return Err("Native authentication expired; restart sign-in".to_string());
        }

        let pending = current
            .as_ref()
            .ok_or_else(|| "Native authentication is not pending; restart sign-in".to_string())?;
        if !pending.prepared.matches_untrusted_grant_target(&grant) {
            // The grant payload is not trusted here. This comparison only
            // prevents an old or injected callback from spending a newer
            // prepared request. The enclave still verifies the signed grant.
            return Err(
                "Native authentication callback does not match the pending sign-in".to_string(),
            );
        }

        // Taking the opaque handle before the await is the one-use boundary.
        // Any timeout or ambiguous transport result consumes the attempt and
        // requires a new browser login rather than resending the request.
        let pending = current
            .take()
            .expect("pending native authentication disappeared while locked");
        let login = match tokio::time::timeout(
            NATIVE_OAUTH_NETWORK_TIMEOUT,
            pending
                .client
                .redeem_native_oauth_handoff(pending.prepared, grant),
        )
        .await
        {
            Ok(Ok(login)) => login,
            Ok(Err(error)) => return Err(map_sdk_error("redeem", &error)),
            Err(_) => {
                return Err(
                    "Secure authentication completion timed out; restart sign-in".to_string(),
                );
            }
        };
        Ok(NativeOAuthAuthentication {
            user_id: login.id.to_string(),
            access_token: login.access_token,
            refresh_token: login.refresh_token,
        })
    }

    async fn cancel(&self, request: CancelNativeOAuthRequest) -> Result<(), String> {
        if !is_attempt_id(&request.native_oauth_attempt) {
            return Err("Native authentication state is invalid".to_string());
        }
        let mut current = self.attempt.lock().await;
        if current
            .as_ref()
            .is_some_and(|attempt| attempt.attempt_id == request.native_oauth_attempt)
        {
            *current = None;
        }
        Ok(())
    }
}

fn new_attempt_id() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    rand::thread_rng()
        .try_fill_bytes(&mut bytes)
        .map_err(|_| "Secure randomness is unavailable".to_string())?;
    if bytes.iter().all(|byte| *byte == 0) {
        return Err("Secure randomness returned an invalid value".to_string());
    }
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    ))
}

fn is_attempt_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
        && value != "00000000-0000-0000-0000-000000000000"
}

fn map_sdk_error(action: &'static str, error: &maple_sdk::Error) -> String {
    let category = match error {
        maple_sdk::Error::Http(_) => "http",
        maple_sdk::Error::Serialization(_) => "serialization",
        maple_sdk::Error::Cbor(_) => "cbor",
        maple_sdk::Error::Crypto(_) => "crypto",
        maple_sdk::Error::AttestationVerificationFailed(_) => "attestation",
        maple_sdk::Error::Session(_) => "session",
        maple_sdk::Error::KeyExchange(_) => "key_exchange",
        maple_sdk::Error::Encryption(_) => "encryption",
        maple_sdk::Error::Decryption(_) => "decryption",
        maple_sdk::Error::Authentication(_) => "authentication",
        maple_sdk::Error::InvalidResponse(_) => "invalid_response",
        maple_sdk::Error::Api { .. } => "api",
        maple_sdk::Error::Configuration(_) => "configuration",
        maple_sdk::Error::Io(_) => "io",
        maple_sdk::Error::Utf8(_) => "utf8",
        maple_sdk::Error::Base64Decode(_) => "base64",
        maple_sdk::Error::Other(_) => "other",
    };
    log::warn!("OpenSecret native authentication operation failed ({action}, {category})");
    "Native authentication failed; restart sign-in".to_string()
}

#[tauri::command]
pub async fn native_oauth_begin(
    state: State<'_, NativeOAuthState>,
    transport_roots: State<'_, TransportRootState>,
    request: BeginNativeOAuthRequest,
) -> Result<NativeOAuthPreparation, String> {
    let cache_root = transport_roots
        .require_exact(&request.api_url, &request.cache_namespace_root_base64)
        .await?;
    state.begin(request, cache_root).await
}

#[tauri::command]
pub async fn native_oauth_redeem(
    state: State<'_, NativeOAuthState>,
    request: RedeemNativeOAuthRequest,
) -> Result<NativeOAuthAuthentication, String> {
    state.redeem(request).await
}

#[tauri::command]
pub async fn native_oauth_cancel(
    state: State<'_, NativeOAuthState>,
    request: CancelNativeOAuthRequest,
) -> Result<(), String> {
    state.cancel(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_attempt_ids_are_canonical_and_non_nil() {
        let id = new_attempt_id().unwrap();
        assert!(is_attempt_id(&id));
        for invalid in [
            "",
            "00000000-0000-0000-0000-000000000000",
            "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa",
        ] {
            assert!(!is_attempt_id(invalid), "{invalid}");
        }
    }
}
