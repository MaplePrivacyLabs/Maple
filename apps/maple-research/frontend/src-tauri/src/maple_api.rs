use crate::native_transport_root::TransportRootState;
use crate::open_secret_config::{configured_pcr0_environment, normalize_api_url};
use maple_sdk::{
    InferenceRequest, InferenceResponse, InferenceSendBudget, OpenSecretClient, TransportV2CacheNamespaceRoot,
    WebExtractRequest, WebExtractResponse, WebSearchRequest, WebSearchResponse,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tauri::State;
use tokio::sync::{Mutex, RwLock, RwLockReadGuard};
use tokio_util::sync::CancellationToken;

const CREDENTIAL_VALIDATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
const TEST_CACHE_ROOT: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MapleApiAuthRequest {
    pub user_id: String,
    pub api_url: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub cache_namespace_root_base64: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MapleApiAuthSnapshot {
    pub user_id: String,
    pub native_instance_id: String,
    pub revision: u64,
}

struct CredentialedClient {
    api_url: String,
    client: Arc<OpenSecretClient>,
}

struct MapleApiSessionInner {
    active: bool,
    revision: u64,
    credentials: CredentialedClient,
}

/// A stable, account-scoped handle shared by Maple's native provider and tools.
///
/// Installing browser credentials swaps the underlying client atomically.
/// Calls already in flight retain their client snapshot; browser and native
/// clients refresh independently after installation.
pub(crate) struct MapleApiSession {
    user_id: String,
    account_scope: String,
    native_instance_id: String,
    inner: RwLock<MapleApiSessionInner>,
}

pub(crate) struct MapleApiAuthLease<'a> {
    _inner: RwLockReadGuard<'a, MapleApiSessionInner>,
}

struct CancelOperationOnDrop(CancellationToken);

impl Drop for CancelOperationOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TokenPair {
    access_token: String,
    refresh_token: Option<String>,
}

impl MapleApiSession {
    fn new(
        user_id: String,
        account_scope: String,
        native_instance_id: String,
        api_url: String,
        client: Arc<OpenSecretClient>,
    ) -> Result<Self, String> {
        capture_tokens(&client)?;
        Ok(Self {
            user_id,
            account_scope,
            native_instance_id,
            inner: RwLock::new(MapleApiSessionInner {
                active: true,
                revision: 1,
                credentials: CredentialedClient { api_url, client },
            }),
        })
    }

    pub(crate) fn account_scope(&self) -> &str {
        &self.account_scope
    }

    async fn replace_client(
        &self,
        api_url: String,
        client: Arc<OpenSecretClient>,
    ) -> Result<MapleApiAuthSnapshot, String> {
        let replacement_tokens = capture_tokens(&client)?;
        let mut inner = self.inner.write().await;
        if !inner.active {
            return Err("Maple API authentication is no longer active".to_string());
        }

        let current_tokens = capture_tokens(&inner.credentials.client)?;
        if inner.credentials.api_url == api_url && current_tokens == replacement_tokens {
            return snapshot_from_inner(&self.user_id, &self.native_instance_id, &inner);
        }

        inner.revision = inner
            .revision
            .checked_add(1)
            .ok_or_else(|| "Maple API authentication revision exhausted".to_string())?;
        inner.credentials = CredentialedClient { api_url, client };
        snapshot_from_inner(&self.user_id, &self.native_instance_id, &inner)
    }

    async fn invalidate(&self) {
        self.inner.write().await.active = false;
    }

    pub(crate) async fn active_lease(&self) -> Result<MapleApiAuthLease<'_>, String> {
        let inner = self.inner.read().await;
        if !inner.active {
            return Err("Maple API authentication is no longer active".to_string());
        }
        Ok(MapleApiAuthLease { _inner: inner })
    }

    #[cfg(test)]
    pub(crate) async fn invalidate_for_test(&self) {
        self.invalidate().await;
    }

    async fn client(&self) -> Result<Arc<OpenSecretClient>, String> {
        let inner = self.inner.read().await;
        if !inner.active {
            return Err("Maple API authentication is no longer active".to_string());
        }
        Ok(Arc::clone(&inner.credentials.client))
    }

    pub(crate) async fn auth_snapshot(&self) -> Result<MapleApiAuthSnapshot, String> {
        let inner = self.inner.read().await;
        if !inner.active {
            return Err("Maple API authentication is no longer active".to_string());
        }
        snapshot_from_inner(&self.user_id, &self.native_instance_id, &inner)
    }

    pub(crate) async fn validate_user(&self) -> Result<(), String> {
        let response = self
            .client()
            .await?
            .get_user()
            .await
            .map_err(map_sdk_error)?;
        if response.user.id.to_string() != self.user_id {
            return Err("Maple API authentication belongs to a different account".to_string());
        }
        Ok(())
    }

    pub(crate) async fn model_ids(&self) -> Result<Vec<String>, String> {
        let response = self
            .client()
            .await?
            .get_models()
            .await
            .map_err(map_sdk_error)?;
        Ok(response.data.into_iter().map(|model| model.id).collect())
    }

    pub(crate) async fn send_inference_request(
        self: Arc<Self>,
        request: InferenceRequest,
        send_budget: InferenceSendBudget,
        cancel_token: CancellationToken,
    ) -> Result<InferenceResponse, maple_sdk::Error> {
        let client = self
            .client()
            .await
            .map_err(maple_sdk::Error::Authentication)?;
        let operation_cancel = cancel_token.child_token();
        let _cancel_on_drop = CancelOperationOnDrop(operation_cancel.clone());
        let operation = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = operation_cancel.cancelled() => {
                    Err(maple_sdk::Error::Other("Inference request was cancelled".to_string()))
                }
                response = client.send_inference_request_with_budget(request, send_budget) => response,
            }
        });
        operation.await.map_err(map_operation_join_error)?
    }

    pub(crate) async fn web_search(
        self: Arc<Self>,
        request: WebSearchRequest,
        cancel_token: CancellationToken,
    ) -> Result<WebSearchResponse, maple_sdk::Error> {
        let client = self
            .client()
            .await
            .map_err(maple_sdk::Error::Authentication)?;
        let operation_cancel = cancel_token.child_token();
        let _cancel_on_drop = CancelOperationOnDrop(operation_cancel.clone());
        let operation = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = operation_cancel.cancelled() => {
                    Err(maple_sdk::Error::Other("Web search was cancelled".to_string()))
                }
                response = client.web_search(request) => response,
            }
        });
        operation.await.map_err(map_operation_join_error)?
    }

    pub(crate) async fn web_extract(
        self: Arc<Self>,
        request: WebExtractRequest,
        cancel_token: CancellationToken,
    ) -> Result<WebExtractResponse, maple_sdk::Error> {
        let client = self
            .client()
            .await
            .map_err(maple_sdk::Error::Authentication)?;
        let operation_cancel = cancel_token.child_token();
        let _cancel_on_drop = CancelOperationOnDrop(operation_cancel.clone());
        let operation = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = operation_cancel.cancelled() => {
                    Err(maple_sdk::Error::Other("Web extraction was cancelled".to_string()))
                }
                response = client.web_extract(request) => response,
            }
        });
        operation.await.map_err(map_operation_join_error)?
    }
}

#[cfg(test)]
pub(crate) fn test_maple_api_session(user_id: &str) -> Arc<MapleApiSession> {
    let user_id = normalized_user_id(user_id).unwrap();
    let account_scope = account_scope(&user_id).unwrap();
    let client = build_client(
        "http://127.0.0.1:1",
        "test-access-token".to_string(),
        None,
        TEST_CACHE_ROOT,
    )
    .unwrap();
    Arc::new(
        MapleApiSession::new(
            user_id,
            account_scope,
            "test-native-instance".to_string(),
            "http://127.0.0.1:1".to_string(),
            client,
        )
        .unwrap(),
    )
}

fn map_operation_join_error(error: tokio::task::JoinError) -> maple_sdk::Error {
    log::warn!("Maple API operation task failed: {error}");
    maple_sdk::Error::Other("Maple API operation failed".to_string())
}

#[async_trait::async_trait]
pub(crate) trait MapleWebTransport: Send + Sync {
    async fn web_search(
        self: Arc<Self>,
        request: WebSearchRequest,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<WebSearchResponse>;

    async fn web_extract(
        self: Arc<Self>,
        request: WebExtractRequest,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<WebExtractResponse>;
}

#[async_trait::async_trait]
impl MapleWebTransport for MapleApiSession {
    async fn web_search(
        self: Arc<Self>,
        request: WebSearchRequest,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<WebSearchResponse> {
        MapleApiSession::web_search(self, request, cancel_token).await
    }

    async fn web_extract(
        self: Arc<Self>,
        request: WebExtractRequest,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<WebExtractResponse> {
        MapleApiSession::web_extract(self, request, cancel_token).await
    }
}

#[async_trait::async_trait]
impl crate::agent::provider::MapleInferenceTransport for MapleApiSession {
    async fn send_inference_request(
        self: Arc<Self>,
        request: InferenceRequest,
        send_budget: InferenceSendBudget,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<InferenceResponse> {
        MapleApiSession::send_inference_request(self, request, send_budget, cancel_token).await
    }
}

fn snapshot_from_inner(
    user_id: &str,
    native_instance_id: &str,
    inner: &MapleApiSessionInner,
) -> Result<MapleApiAuthSnapshot, String> {
    Ok(MapleApiAuthSnapshot {
        user_id: user_id.to_string(),
        native_instance_id: native_instance_id.to_string(),
        revision: inner.revision,
    })
}

fn capture_tokens(client: &OpenSecretClient) -> Result<TokenPair, String> {
    let tokens = client
        .get_tokens()
        .map_err(map_sdk_error)?
        .ok_or_else(|| "Maple API access token is missing".to_string())?;
    let access_token = (!tokens.access_token.trim().is_empty())
        .then_some(tokens.access_token)
        .ok_or_else(|| "Maple API access token is missing".to_string())?;
    let refresh_token = tokens
        .refresh_token
        .filter(|token| !token.trim().is_empty());
    Ok(TokenPair {
        access_token,
        refresh_token,
    })
}

fn map_sdk_error(error: maple_sdk::Error) -> String {
    log::warn!(
        "OpenSecret SDK authentication operation failed ({})",
        crate::agent::provider::opensecret_error_category(&error)
    );
    "Maple API authentication failed".to_string()
}

pub(crate) fn account_scope(user_id: &str) -> Result<String, String> {
    let user_id = normalized_user_id(user_id)?;
    let digest = Sha256::digest(user_id.as_bytes());
    Ok(format!("{digest:x}"))
}

fn normalized_user_id(user_id: &str) -> Result<String, String> {
    let user_id = user_id.trim().to_ascii_lowercase();
    if user_id.is_empty() {
        return Err("Maple API access requires a signed-in account".to_string());
    }
    Ok(user_id)
}

fn build_client(
    api_url: &str,
    access_token: String,
    refresh_token: Option<String>,
    cache_namespace_root_base64: &str,
) -> Result<Arc<OpenSecretClient>, String> {
    if access_token.trim().is_empty() {
        return Err("Maple API access token is missing".to_string());
    }
    let refresh_token = refresh_token.filter(|token| !token.trim().is_empty());
    let cache_root = TransportV2CacheNamespaceRoot::from_base64(cache_namespace_root_base64)
        .map_err(map_sdk_error)?;
    let client = OpenSecretClient::new_with_pcr0_environment(
        api_url.to_string(),
        configured_pcr0_environment()?,
    )
    .map_err(map_sdk_error)?
    .with_cache_namespace_root(cache_root);
    client
        .set_tokens(access_token, refresh_token)
        .map_err(map_sdk_error)?;
    Ok(Arc::new(client))
}

pub struct MapleApiAuthState {
    inner: Mutex<Option<Arc<MapleApiSession>>>,
    mutation: Mutex<()>,
    native_instance_id: String,
    credential_validator: Arc<dyn MapleApiCredentialValidator>,
}

#[async_trait::async_trait]
trait MapleApiCredentialValidator: Send + Sync {
    async fn validate(
        &self,
        client: &OpenSecretClient,
        expected_user_id: &str,
    ) -> Result<(), String>;
}

struct BackendCredentialValidator;

#[async_trait::async_trait]
impl MapleApiCredentialValidator for BackendCredentialValidator {
    async fn validate(
        &self,
        client: &OpenSecretClient,
        expected_user_id: &str,
    ) -> Result<(), String> {
        let response = client.get_user().await.map_err(map_sdk_error)?;
        let actual_user_id = normalized_user_id(&response.user.id.to_string())?;
        if actual_user_id != expected_user_id {
            return Err("Maple API authentication belongs to a different account".to_string());
        }
        Ok(())
    }
}

fn new_native_instance_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

impl MapleApiAuthState {
    pub fn new() -> Self {
        Self::with_validator(Arc::new(BackendCredentialValidator))
    }

    fn with_validator(credential_validator: Arc<dyn MapleApiCredentialValidator>) -> Self {
        Self {
            inner: Mutex::new(None),
            mutation: Mutex::new(()),
            native_instance_id: new_native_instance_id(),
            credential_validator,
        }
    }

    async fn set_auth(&self, request: MapleApiAuthRequest) -> Result<MapleApiAuthSnapshot, String> {
        // Keep set/clear ordering intact across the candidate-validation await.
        // Agent calls can continue using the prior client until validation
        // succeeds and the replacement is published atomically.
        let _mutation = self.mutation.lock().await;
        let user_id = normalized_user_id(&request.user_id)?;
        let requested_scope = account_scope(&user_id)?;
        let api_url = normalize_api_url(&request.api_url)?;
        let cache_namespace_root_base64 = request.cache_namespace_root_base64;
        let client = build_client(
            &api_url,
            request.access_token,
            request.refresh_token,
            &cache_namespace_root_base64,
        )?;
        tokio::time::timeout(
            CREDENTIAL_VALIDATION_TIMEOUT,
            self.credential_validator.validate(&client, &user_id),
        )
        .await
        .map_err(|_| "Maple API authentication validation timed out".to_string())??;

        let mut current = self.inner.lock().await;
        if let Some(session) = current.as_ref() {
            if session.account_scope() != requested_scope {
                return Err(
                    "Maple API authentication belongs to a different signed-in account".to_string(),
                );
            }
            return session.replace_client(api_url, client).await;
        }

        let session = Arc::new(MapleApiSession::new(
            user_id,
            requested_scope,
            self.native_instance_id.clone(),
            api_url,
            client,
        )?);
        let snapshot = session.auth_snapshot().await?;
        *current = Some(session);
        Ok(snapshot)
    }

    pub(crate) async fn session_for(&self, user_id: &str) -> Result<Arc<MapleApiSession>, String> {
        let requested_scope = account_scope(user_id)?;
        let current = self.inner.lock().await;
        let session = current
            .as_ref()
            .ok_or_else(|| "Maple API authentication is not initialized".to_string())?;
        if session.account_scope() != requested_scope {
            return Err(
                "Maple API authentication belongs to a different signed-in account".to_string(),
            );
        }
        Ok(Arc::clone(session))
    }

    async fn clear_auth(&self, user_id: &str) -> Result<(), String> {
        let _mutation = self.mutation.lock().await;
        let requested_scope = account_scope(user_id)?;
        let session = {
            let mut current = self.inner.lock().await;
            let Some(session) = current.as_ref() else {
                return Ok(());
            };
            if session.account_scope() != requested_scope {
                return Err(
                    "Maple API authentication belongs to a different signed-in account".to_string(),
                );
            }
            current.take().expect("Maple API session disappeared")
        };
        session.invalidate().await;
        Ok(())
    }
}

#[tauri::command]
pub async fn maple_api_set_auth(
    state: State<'_, MapleApiAuthState>,
    transport_roots: State<'_, TransportRootState>,
    request: MapleApiAuthRequest,
) -> Result<MapleApiAuthSnapshot, String> {
    transport_roots
        .require_exact(&request.api_url, &request.cache_namespace_root_base64)
        .await?;
    state.set_auth(request).await
}

#[tauri::command]
pub async fn maple_api_clear_auth(
    state: State<'_, MapleApiAuthState>,
    user_id: String,
) -> Result<(), String> {
    state.clear_auth(&user_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Notify;

    struct TokenPrefixCredentialValidator;

    #[async_trait::async_trait]
    impl MapleApiCredentialValidator for TokenPrefixCredentialValidator {
        async fn validate(
            &self,
            client: &OpenSecretClient,
            expected_user_id: &str,
        ) -> Result<(), String> {
            let tokens = capture_tokens(client)?;
            let actual_user_id = tokens
                .access_token
                .split_once('|')
                .map(|(user_id, _)| user_id)
                .ok_or_else(|| "test credential is missing its account prefix".to_string())?;
            if actual_user_id != expected_user_id {
                return Err("Maple API authentication belongs to a different account".to_string());
            }
            if tokens.access_token.ends_with("refresh-during-validation") {
                client
                    .set_tokens(
                        format!("{expected_user_id}|validated-access"),
                        Some(format!("{expected_user_id}|validated-refresh")),
                    )
                    .map_err(map_sdk_error)?;
            }
            Ok(())
        }
    }

    struct BlockingCredentialValidator {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl MapleApiCredentialValidator for BlockingCredentialValidator {
        async fn validate(
            &self,
            client: &OpenSecretClient,
            expected_user_id: &str,
        ) -> Result<(), String> {
            self.entered.notify_one();
            self.release.notified().await;
            TokenPrefixCredentialValidator
                .validate(client, expected_user_id)
                .await
        }
    }

    fn test_state() -> MapleApiAuthState {
        MapleApiAuthState::with_validator(Arc::new(TokenPrefixCredentialValidator))
    }

    fn auth_request(user_id: &str, access_token: &str) -> MapleApiAuthRequest {
        MapleApiAuthRequest {
            user_id: user_id.to_string(),
            api_url: "https://enclave.trymaple.ai".to_string(),
            access_token: format!("{user_id}|{access_token}"),
            refresh_token: Some(format!("{user_id}|refresh-{access_token}")),
            cache_namespace_root_base64: TEST_CACHE_ROOT.to_string(),
        }
    }

    #[test]
    fn api_url_requires_https_or_loopback_http() {
        assert_eq!(
            normalize_api_url("https://enclave.trymaple.ai/").unwrap(),
            "https://enclave.trymaple.ai"
        );
        assert_eq!(
            normalize_api_url("http://127.0.0.1:31745").unwrap(),
            "http://127.0.0.1:31745"
        );
        assert_eq!(
            normalize_api_url("http://localhost:31745/").unwrap(),
            "http://localhost:31745"
        );
        assert!(normalize_api_url("http://enclave.trymaple.ai").is_err());
        assert!(normalize_api_url("https://user:pass@example.com").is_err());
        assert!(normalize_api_url("https://example.com/v1").is_err());
    }

    #[test]
    fn account_scopes_are_normalized_opaque_and_isolated() {
        assert_eq!(
            account_scope(" USER-A ").unwrap(),
            account_scope("user-a").unwrap()
        );
        assert_ne!(
            account_scope("user-a").unwrap(),
            account_scope("user-b").unwrap()
        );
        assert!(!account_scope("user-a").unwrap().contains("user-a"));
        assert!(account_scope("  ").is_err());
    }

    #[tokio::test]
    async fn same_account_updates_are_atomic_and_clear_invalidates_retained_handles() {
        let state = test_state();

        let first = state
            .set_auth(auth_request("user-a", "access-one"))
            .await
            .unwrap();
        assert_eq!(first.revision, 1);
        let retained = state.session_for("user-a").await.unwrap();

        let unchanged = state
            .set_auth(auth_request("user-a", "access-one"))
            .await
            .unwrap();
        assert_eq!(unchanged.revision, 1);

        let replaced = state
            .set_auth(auth_request("user-a", "access-two"))
            .await
            .unwrap();
        assert_eq!(replaced.revision, 2);
        assert!(state
            .set_auth(auth_request("user-b", "other"))
            .await
            .is_err());

        state.clear_auth("user-a").await.unwrap();
        assert!(retained.auth_snapshot().await.is_err());
        assert!(state.session_for("user-a").await.is_err());

        let next_account = state
            .set_auth(auth_request("user-b", "other"))
            .await
            .unwrap();
        assert_eq!(next_account.user_id, "user-b");
        assert_eq!(next_account.revision, 1);
    }

    #[tokio::test]
    async fn cancelled_web_calls_stop_inside_account_scoped_transport() {
        let state = test_state();
        state
            .set_auth(auth_request("user-a", "access-one"))
            .await
            .unwrap();
        let session = state.session_for("user-a").await.unwrap();
        let before = session.auth_snapshot().await.unwrap();

        let search_cancel = CancellationToken::new();
        search_cancel.cancel();
        let search = Arc::clone(&session)
            .web_search(WebSearchRequest::new("maple privacy"), search_cancel)
            .await;
        assert!(
            matches!(search, Err(maple_sdk::Error::Other(message)) if message.contains("cancelled"))
        );

        let extract_cancel = CancellationToken::new();
        extract_cancel.cancel();
        let extract = Arc::clone(&session)
            .web_extract(
                WebExtractRequest::new(["https://example.com"]),
                extract_cancel,
            )
            .await;
        assert!(
            matches!(extract, Err(maple_sdk::Error::Other(message)) if message.contains("cancelled"))
        );

        let after = session.auth_snapshot().await.unwrap();
        assert_eq!(after.revision, before.revision);
    }

    #[tokio::test]
    async fn candidate_identity_is_verified_before_replacing_live_credentials() {
        let state = test_state();
        state
            .set_auth(auth_request("user-a", "access-one"))
            .await
            .unwrap();

        let mut wrong_account = auth_request("user-a", "access-two");
        wrong_account.access_token = "user-b|access-two".to_string();
        wrong_account.refresh_token = Some("user-b|refresh-access-two".to_string());
        let error = state
            .set_auth(wrong_account)
            .await
            .expect_err("cross-account replacement must be rejected");

        assert!(error.contains("different account"));
        let current = state
            .session_for("user-a")
            .await
            .unwrap()
            .auth_snapshot()
            .await
            .unwrap();
        assert_eq!(current.revision, 1);
        let tokens = capture_tokens(
            &state
                .session_for("user-a")
                .await
                .unwrap()
                .client()
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(tokens.access_token, "user-a|access-one");
    }

    #[tokio::test]
    async fn validation_refresh_stays_native_and_receipt_contains_no_secrets() {
        let state = test_state();
        let snapshot = state
            .set_auth(auth_request("user-a", "refresh-during-validation"))
            .await
            .unwrap();

        let client = state
            .session_for("user-a")
            .await
            .unwrap()
            .client()
            .await
            .unwrap();
        let tokens = capture_tokens(&client).unwrap();
        assert_eq!(tokens.access_token, "user-a|validated-access");
        assert_eq!(
            tokens.refresh_token.as_deref(),
            Some("user-a|validated-refresh")
        );
        assert!(!snapshot.native_instance_id.is_empty());
        let encoded = serde_json::to_value(snapshot).unwrap();
        assert!(encoded.get("accessToken").is_none());
        assert!(encoded.get("refreshToken").is_none());
        assert!(encoded.get("cacheNamespaceRootBase64").is_none());
    }

    #[tokio::test]
    async fn clear_ordering_cannot_be_overtaken_by_in_flight_validation() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let state = Arc::new(MapleApiAuthState::with_validator(Arc::new(
            BlockingCredentialValidator {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
        )));
        let setter_state = Arc::clone(&state);
        let setter = tokio::spawn(async move {
            setter_state
                .set_auth(auth_request("user-a", "access-one"))
                .await
        });

        entered.notified().await;
        let clearer_state = Arc::clone(&state);
        let clearer = tokio::spawn(async move { clearer_state.clear_auth("user-a").await });
        release.notify_one();

        setter.await.unwrap().unwrap();
        clearer.await.unwrap().unwrap();
        assert!(state.session_for("user-a").await.is_err());
    }
}
