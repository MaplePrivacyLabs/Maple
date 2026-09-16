use reqwest::StatusCode;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use url::Url;
use uuid::Uuid;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const USER_FLAGS_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

// Keep canonical feature-flag keys together so callers do not duplicate
// externally configured identifiers.
#[allow(dead_code)]
pub const AGENT_FEATURE_FLAG_KEY: &str = "agent";
pub const PAID_POWERFUL_GLM_5_3_ALIAS_FLAG_KEY: &str = "model-alias.paid.powerful.glm-5-3";
pub const PAID_MODEL_ALIAS_FLAG_KEYS: &[&str] = &[PAID_POWERFUL_GLM_5_3_ALIAS_FLAG_KEY];
pub const GLM_5_3_TINFOIL_FLAG_KEY: &str = "provider-routing.glm-5-3.tinfoil";
pub const GLM_5_3_FLASH_CONTINUUM_FLAG_KEY: &str = "provider-routing.glm-5-3-flash.continuum";
pub const INFERENCE_ROUTER_V2_FLAG_KEY: &str = "inference.router-v2";

#[derive(Debug, thiserror::Error)]
pub enum OsFlagsError {
    #[error("Invalid base URL: {0}")]
    InvalidBaseUrl(#[from] url::ParseError),

    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("Service error: {status} - {message}")]
    Service { status: StatusCode, message: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserFlagsResponse {
    pub user_uuid: Uuid,
    pub flags: HashMap<String, bool>,
}

#[derive(Clone)]
pub struct OsFlagsClient {
    client: reqwest::Client,
    base_url: Url,
    api_key: Option<Arc<String>>,
    cache: UserFlagsCache,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UserFlagsCacheKey {
    user_uuid: Uuid,
    keys: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct UserFlagsCacheEntry {
    response: UserFlagsResponse,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
struct UserFlagsCache {
    inner: Arc<RwLock<HashMap<UserFlagsCacheKey, UserFlagsCacheEntry>>>,
    ttl: Duration,
}

impl UserFlagsCacheKey {
    fn new(user_uuid: Uuid, keys: Option<&[&str]>) -> Self {
        Self {
            user_uuid,
            keys: normalize_keys(keys),
        }
    }
}

impl Default for UserFlagsCache {
    fn default() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            ttl: USER_FLAGS_CACHE_TTL,
        }
    }
}

impl UserFlagsCache {
    #[cfg(test)]
    fn new(ttl: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            ttl,
        }
    }

    async fn get(&self, key: &UserFlagsCacheKey) -> Option<UserFlagsResponse> {
        let now = Instant::now();
        {
            let inner = self.inner.read().await;
            let entry = inner.get(key)?;
            if entry.expires_at > now {
                return Some(entry.response.clone());
            }
        }

        let mut inner = self.inner.write().await;
        if inner.get(key).is_some_and(|entry| entry.expires_at <= now) {
            inner.remove(key);
        }
        None
    }

    async fn insert(&self, key: UserFlagsCacheKey, response: UserFlagsResponse) {
        let now = Instant::now();
        let mut inner = self.inner.write().await;
        evict_expired(&mut inner, now);
        inner.insert(
            key,
            UserFlagsCacheEntry {
                response,
                expires_at: now + self.ttl,
            },
        );
    }
}

impl OsFlagsClient {
    pub fn new(base_url: String, api_key: Option<String>) -> Result<Self, OsFlagsError> {
        let base_url = format!("{}/", base_url.trim_end_matches('/'));
        let base_url = Url::parse(&base_url)?;

        let client = crate::http_client::client_builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_max_idle_per_host(100)
            .user_agent("opensecret/os-flags-client")
            .build()?;

        Ok(Self {
            client,
            base_url,
            api_key: api_key.map(Arc::new),
            cache: UserFlagsCache::default(),
        })
    }

    pub async fn get_user_flags(
        &self,
        user_uuid: Uuid,
        keys: Option<&[&str]>,
    ) -> Result<UserFlagsResponse, OsFlagsError> {
        let cache_key = UserFlagsCacheKey::new(user_uuid, keys);
        if let Some(response) = self.cache.get(&cache_key).await {
            return Ok(response);
        }

        let mut url = self
            .base_url
            .join(&format!("v1/users/{}/flags", user_uuid))?;

        {
            let mut qp = url.query_pairs_mut();
            if let Some(keys) = cache_key.keys.as_ref() {
                if !keys.is_empty() {
                    qp.append_pair("keys", &keys.join(","));
                }
            }
        }

        let mut req = self.client.get(url);
        if let Some(api_key) = &self.api_key {
            req = req.bearer_auth(api_key.as_str());
        }

        let resp = req.send().await?;
        let status = resp.status();

        if status.is_success() {
            let response = resp.json::<UserFlagsResponse>().await?;
            self.cache.insert(cache_key, response.clone()).await;
            Ok(response)
        } else {
            let text = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.as_str())
                        .map(ToOwned::to_owned)
                })
                .unwrap_or(text);

            Err(OsFlagsError::Service { status, message })
        }
    }

    pub async fn is_enabled(&self, user_uuid: Uuid, key: &str) -> Result<bool, OsFlagsError> {
        Ok(self.get_bool_flag(user_uuid, key).await?.unwrap_or(false))
    }

    pub async fn get_bool_flag(
        &self,
        user_uuid: Uuid,
        key: &str,
    ) -> Result<Option<bool>, OsFlagsError> {
        let keys = [key];
        let resp = self.get_user_flags(user_uuid, Some(&keys)).await?;
        let value = resp.flags.get(key).copied();
        tracing::debug!(
            user_uuid = %user_uuid,
            flag_key = %key,
            enabled = ?value,
            "os-flags bool flag evaluated"
        );
        Ok(value)
    }

    /// Return a cached flag value without contacting os-flags. The outer
    /// option distinguishes a cache miss from a cached response in which the
    /// requested flag was absent.
    pub(crate) async fn get_cached_bool_flag(
        &self,
        user_uuid: Uuid,
        key: &str,
    ) -> Option<Option<bool>> {
        let keys = [key];
        let cache_key = UserFlagsCacheKey::new(user_uuid, Some(&keys));
        self.cache
            .get(&cache_key)
            .await
            .map(|response| response.flags.get(key).copied())
    }
}

impl std::fmt::Debug for OsFlagsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OsFlagsClient")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("cache_ttl", &self.cache.ttl)
            .finish()
    }
}

fn normalize_keys(keys: Option<&[&str]>) -> Option<Vec<String>> {
    let mut keys = keys?
        .iter()
        .map(|key| key.trim())
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if keys.is_empty() {
        return None;
    }

    keys.sort_unstable();
    keys.dedup();
    Some(keys)
}

fn evict_expired(inner: &mut HashMap<UserFlagsCacheKey, UserFlagsCacheEntry>, now: Instant) {
    inner.retain(|_, entry| entry.expires_at > now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Path, Query, State},
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::get,
        Json, Router,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[derive(Clone, Default)]
    struct FlagServerState {
        calls: Arc<AtomicUsize>,
    }

    async fn cached_router_flag(
        Path(user_uuid): Path<Uuid>,
        Query(query): Query<HashMap<String, String>>,
        State(state): State<FlagServerState>,
    ) -> Json<serde_json::Value> {
        assert_eq!(
            query.get("keys").map(String::as_str),
            Some(INFERENCE_ROUTER_V2_FLAG_KEY)
        );
        let call = state.calls.fetch_add(1, Ordering::SeqCst);
        Json(json!({
            "user_uuid": user_uuid,
            "flags": { (INFERENCE_ROUTER_V2_FLAG_KEY): call > 0 }
        }))
    }

    async fn transient_router_flag_error(
        Path(user_uuid): Path<Uuid>,
        Query(query): Query<HashMap<String, String>>,
        State(state): State<FlagServerState>,
    ) -> Response {
        assert_eq!(
            query.get("keys").map(String::as_str),
            Some(INFERENCE_ROUTER_V2_FLAG_KEY)
        );
        if state.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }

        Json(json!({
            "user_uuid": user_uuid,
            "flags": { (INFERENCE_ROUTER_V2_FLAG_KEY): true }
        }))
        .into_response()
    }

    async fn spawn_flag_server(
        state: FlagServerState,
        handler: axum::routing::MethodRouter<FlagServerState>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/users/:user_uuid/flags", handler)
                    .with_state(state),
            )
            .await
            .unwrap();
        });
        (format!("http://{address}"), server)
    }

    fn response(user_uuid: Uuid) -> UserFlagsResponse {
        UserFlagsResponse {
            user_uuid,
            flags: HashMap::from([("flag_a".to_string(), true)]),
        }
    }

    #[test]
    fn cache_key_normalizes_keys() {
        let user_uuid = Uuid::new_v4();
        let a = UserFlagsCacheKey::new(user_uuid, Some(&["flag_b", "flag_a", "flag_a", ""]));
        let b = UserFlagsCacheKey::new(user_uuid, Some(&["flag_a", "flag_b"]));

        assert_eq!(a, b);
    }

    #[test]
    fn cache_key_treats_empty_keys_as_all_flags() {
        let user_uuid = Uuid::new_v4();
        let a = UserFlagsCacheKey::new(user_uuid, None);
        let b = UserFlagsCacheKey::new(user_uuid, Some(&[]));
        let c = UserFlagsCacheKey::new(user_uuid, Some(&["", " "]));

        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[tokio::test]
    async fn cache_entries_expire() {
        let cache = UserFlagsCache::new(Duration::from_millis(5));
        let user_uuid = Uuid::new_v4();
        let key = UserFlagsCacheKey::new(user_uuid, Some(&["flag_a"]));

        cache.insert(key.clone(), response(user_uuid)).await;
        assert!(cache.get(&key).await.is_some());

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(cache.get(&key).await.is_none());
    }

    #[tokio::test]
    async fn router_v2_flag_is_cached_by_user_including_false() {
        let state = FlagServerState::default();
        let calls = state.calls.clone();
        let (base_url, server) = spawn_flag_server(state, get(cached_router_flag)).await;
        let client = OsFlagsClient::new(base_url, None).unwrap();
        let first_user = Uuid::new_v4();
        let second_user = Uuid::new_v4();

        assert_eq!(
            client
                .get_cached_bool_flag(first_user, INFERENCE_ROUTER_V2_FLAG_KEY)
                .await,
            None
        );

        assert_eq!(
            client
                .get_bool_flag(first_user, INFERENCE_ROUTER_V2_FLAG_KEY)
                .await
                .unwrap(),
            Some(false)
        );
        assert_eq!(
            client
                .get_cached_bool_flag(first_user, INFERENCE_ROUTER_V2_FLAG_KEY)
                .await,
            Some(Some(false))
        );
        assert_eq!(
            client
                .get_bool_flag(first_user, INFERENCE_ROUTER_V2_FLAG_KEY)
                .await
                .unwrap(),
            Some(false)
        );
        assert_eq!(
            client
                .get_bool_flag(second_user, INFERENCE_ROUTER_V2_FLAG_KEY)
                .await
                .unwrap(),
            Some(true)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn router_v2_flag_errors_are_not_cached() {
        let state = FlagServerState::default();
        let calls = state.calls.clone();
        let (base_url, server) = spawn_flag_server(state, get(transient_router_flag_error)).await;
        let client = OsFlagsClient::new(base_url, None).unwrap();
        let user_uuid = Uuid::new_v4();

        assert!(client
            .get_bool_flag(user_uuid, INFERENCE_ROUTER_V2_FLAG_KEY)
            .await
            .is_err());
        assert_eq!(
            client
                .get_bool_flag(user_uuid, INFERENCE_ROUTER_V2_FLAG_KEY)
                .await
                .unwrap(),
            Some(true)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }
}
