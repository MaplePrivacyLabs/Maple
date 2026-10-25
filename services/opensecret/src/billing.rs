use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::warn;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct UsageResponse {
    pub can_use: bool,
    #[serde(default = "default_is_free")]
    pub is_free: bool,
}

/// V2 reports API/frontend admission and plan identity together. Admission
/// fields are required; older responses without a tier keep legacy Free/Paid
/// behavior and never grant premium models. Malformed admission does not fall
/// back to another authorization evaluation.
#[derive(Debug, Deserialize, Clone, Copy)]
struct UsageResponseV2 {
    api_can_use: bool,
    frontend_can_use: bool,
    is_free: bool,
    #[serde(default)]
    plan: BillingPlan,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BillingPlan {
    Free,
    Pro,
    Max,
    Teams,
    #[default]
    #[serde(other)]
    Unknown,
}

const fn default_is_free() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChatBillingAccess {
    plan: BillingPlan,
    can_use: bool,
    is_free: bool,
}

impl ChatBillingAccess {
    fn from_frontend_usage(usage: UsageResponse) -> Self {
        Self {
            plan: BillingPlan::Unknown,
            can_use: usage.can_use,
            is_free: usage.is_free,
        }
    }

    fn from_api_and_plan_usage(api_usage: UsageResponse, plan_usage: UsageResponse) -> Self {
        Self {
            plan: BillingPlan::Unknown,
            can_use: api_usage.can_use,
            is_free: plan_usage.is_free,
        }
    }

    #[cfg(test)]
    pub(crate) const fn for_tests(can_use: bool, is_free: bool) -> Self {
        Self {
            plan: BillingPlan::Unknown,
            can_use,
            is_free,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests_with_plan(can_use: bool, is_free: bool, plan: &str) -> Self {
        Self {
            can_use,
            is_free,
            plan: serde_json::from_value(serde_json::json!(plan)).unwrap(),
        }
    }

    pub(crate) fn has_premium_models(self) -> bool {
        self.is_paid() && matches!(self.plan, BillingPlan::Max | BillingPlan::Teams)
    }

    pub(crate) const fn can_use(self) -> bool {
        self.can_use
    }

    pub(crate) const fn is_paid(self) -> bool {
        !self.is_free
    }

    pub(crate) fn check_with_tokens(self, input_tokens: i32) -> Result<(), BillingError> {
        if !self.can_use {
            return Err(BillingError::UsageLimitExceeded);
        }

        if self.is_free && input_tokens > 20_000 {
            return Err(BillingError::FreeTokenLimitExceeded);
        }

        Ok(())
    }
}

fn paid_feature_access(usage: &UsageResponse) -> bool {
    !usage.is_free && usage.can_use
}

#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    #[error("Request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("Failed to parse response: {0}")]
    ParseError(String),
    #[error("Service error: {0}")]
    ServiceError(String),
    #[error("Usage limit reached")]
    UsageLimitExceeded,
    #[error("Token limit exceeded on free plan")]
    FreeTokenLimitExceeded,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AccountDeletionError {
    #[error("timeout")]
    Timeout,
    #[error("transport")]
    Transport,
    #[error("http_status_{0}")]
    HttpStatus(u16),
    #[error("invalid_response")]
    InvalidResponse,
}

#[derive(Serialize)]
struct AccountDeletionRequest {
    user_id: Uuid,
}

#[derive(Deserialize)]
struct AccountDeletionResponse {
    user_id: Uuid,
    status: String,
}

#[derive(Clone)]
pub struct BillingClient {
    client: Client,
    api_key: String,
    base_url: String,
}

impl BillingClient {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self {
            client: crate::http_client::client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()
                .expect("failed to build billing HTTP client"),
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Billing owns cancellation/no-op decisions. A response only permits the
    /// caller to delete the account when billing confirms this exact user is ready.
    /// No retry, redirect, provider response body, or credential-bearing error is
    /// exposed across this boundary.
    pub(crate) async fn prepare_account_deletion(
        &self,
        user_id: Uuid,
    ) -> Result<(), AccountDeletionError> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut response = self
                .client
                .post(format!(
                    "{}/v1/admin/account-deletions",
                    self.base_url.trim_end_matches('/')
                ))
                .header("x-api-key", &self.api_key)
                .json(&AccountDeletionRequest { user_id })
                .send()
                .await
                .map_err(|_| AccountDeletionError::Transport)?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(AccountDeletionError::HttpStatus(response.status().as_u16()));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| AccountDeletionError::Transport)?
            {
                if body.len().saturating_add(chunk.len()) > 4096 {
                    return Err(AccountDeletionError::InvalidResponse);
                }
                body.extend_from_slice(&chunk);
            }
            let ready: AccountDeletionResponse =
                serde_json::from_slice(&body).map_err(|_| AccountDeletionError::InvalidResponse)?;
            if ready.user_id != user_id || ready.status != "ready" {
                return Err(AccountDeletionError::InvalidResponse);
            }
            Ok(())
        })
        .await
        .map_err(|_| AccountDeletionError::Timeout)?
    }

    async fn check_usage(
        &self,
        user_id: Uuid,
        is_api: bool,
    ) -> Result<UsageResponse, BillingError> {
        let mut request = self
            .client
            .get(format!("{}/v1/admin/check-usage", self.base_url))
            .query(&[
                ("user_id", user_id.to_string()),
                ("product", "maple".to_string()),
            ])
            .header("x-api-key", &self.api_key);

        if is_api {
            request = request.query(&[("api", "true".to_string())]);
        }

        let response = request.send().await?;

        if response.status().is_success() {
            response
                .json::<UsageResponse>()
                .await
                .map_err(|e| BillingError::ParseError(e.to_string()))
        } else {
            let error = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            Err(BillingError::ServiceError(error))
        }
    }

    /// `None` means an older Billing deployment does not expose this endpoint.
    /// Only explicit endpoint/method absence permits the legacy fallback; a
    /// denial, dependency failure, or invalid response is authoritative for this
    /// attempt. The caller's existing total deadline covers both versions.
    async fn check_usage_v2(&self, user_id: Uuid) -> Result<Option<UsageResponseV2>, BillingError> {
        let mut response = self
            .client
            .get(format!(
                "{}/v2/admin/check-usage",
                self.base_url.trim_end_matches('/')
            ))
            .query(&[
                ("user_id", user_id.to_string()),
                ("product", "maple".to_string()),
            ])
            .header("x-api-key", &self.api_key)
            .send()
            .await?;

        match response.status() {
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED => {
                return Ok(None);
            }
            reqwest::StatusCode::OK => {}
            status => {
                // Provider/service bodies are not part of this contract and may
                // contain private operational details.
                return Err(BillingError::ServiceError(format!(
                    "billing usage v2 returned HTTP {}",
                    status.as_u16()
                )));
            }
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len().saturating_add(chunk.len()) > 4096 {
                return Err(BillingError::ParseError(
                    "billing usage v2 response exceeds limit".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|_| BillingError::ParseError("invalid billing usage v2 response".into()))
    }

    pub(crate) async fn chat_access(
        &self,
        user_id: Uuid,
        is_api: bool,
    ) -> Result<ChatBillingAccess, BillingError> {
        if let Some(usage) = self.check_usage_v2(user_id).await? {
            return Ok(ChatBillingAccess {
                can_use: if is_api {
                    usage.api_can_use
                } else {
                    usage.frontend_can_use
                },
                is_free: usage.is_free || (is_api && !usage.api_can_use),
                plan: usage.plan,
            });
        }
        if is_api {
            self.api_chat_access_v1(user_id).await
        } else {
            self.check_usage(user_id, false)
                .await
                .map(ChatBillingAccess::from_frontend_usage)
        }
    }

    async fn api_chat_access_v1(&self, user_id: Uuid) -> Result<ChatBillingAccess, BillingError> {
        // The API usage response intentionally reports `is_free: false` for
        // API-credit users, regardless of their subscription. Keep its quota
        // decision, but source plan identity from the frontend usage response.
        let api_usage = self.check_usage(user_id, true);
        let plan_usage = self.check_usage(user_id, false);
        tokio::pin!(api_usage);
        tokio::pin!(plan_usage);

        // Poll both requests concurrently, but never let a slow plan lookup
        // hide an authoritative API quota denial from the outer timeout.
        let (api_usage, plan_usage) = tokio::select! {
            api_usage = &mut api_usage => {
                let api_usage = api_usage?;
                if !api_usage.can_use {
                    return Ok(ChatBillingAccess {
                        plan: BillingPlan::Unknown,
                        can_use: false,
                        is_free: true,
                    });
                }
                (api_usage, plan_usage.await)
            }
            plan_usage = &mut plan_usage => {
                (api_usage.await?, plan_usage)
            }
        };
        if !api_usage.can_use {
            return Ok(ChatBillingAccess {
                plan: BillingPlan::Unknown,
                can_use: false,
                is_free: true,
            });
        }
        let plan_usage = plan_usage.unwrap_or_else(|error| {
            warn!(
                "Billing plan lookup failed for API request (user_id={}): {}; using free model access",
                user_id, error
            );
            UsageResponse {
                can_use: false,
                is_free: true,
            }
        });
        Ok(ChatBillingAccess::from_api_and_plan_usage(
            api_usage, plan_usage,
        ))
    }

    /// Check if a user is on a paid plan (not free)
    pub async fn is_user_paid(&self, user_id: Uuid) -> Result<bool, BillingError> {
        let usage = self.check_usage(user_id, false).await?;
        Ok(!usage.is_free)
    }

    /// Check whether a paid-plan user may currently use a metered upstream
    /// feature. Unlike `is_user_paid`, this also honors the billing service's
    /// current usage/entitlement decision without making a second request.
    pub async fn can_user_use_paid_features(&self, user_id: Uuid) -> Result<bool, BillingError> {
        let usage = self.check_usage(user_id, false).await?;
        Ok(paid_feature_access(&usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, routing::get, Json, Router};
    use std::{collections::HashMap, future::pending, time::Duration};
    use tokio::net::TcpListener;

    #[test]
    fn paid_feature_access_requires_paid_plan_and_current_usage_access() {
        assert!(paid_feature_access(&UsageResponse {
            can_use: true,
            is_free: false,
        }));
        assert!(!paid_feature_access(&UsageResponse {
            can_use: false,
            is_free: false,
        }));
        assert!(!paid_feature_access(&UsageResponse {
            can_use: true,
            is_free: true,
        }));
    }

    #[test]
    fn missing_plan_information_defaults_to_free() {
        let usage: UsageResponse = serde_json::from_value(serde_json::json!({
            "can_use": true
        }))
        .expect("usage response");

        assert!(usage.is_free);
        assert!(!paid_feature_access(&usage));
    }

    #[test]
    fn chat_access_enforces_quota_and_free_token_limit() {
        let free = ChatBillingAccess::from_frontend_usage(UsageResponse {
            can_use: true,
            is_free: true,
        });
        let paid = ChatBillingAccess::from_frontend_usage(UsageResponse {
            can_use: true,
            is_free: false,
        });
        let exhausted = ChatBillingAccess::from_frontend_usage(UsageResponse {
            can_use: false,
            is_free: false,
        });

        assert!(!free.is_paid());
        assert!(paid.is_paid());
        assert!(free.check_with_tokens(20_000).is_ok());
        assert!(matches!(
            free.check_with_tokens(20_001),
            Err(BillingError::FreeTokenLimitExceeded)
        ));
        assert!(paid.check_with_tokens(i32::MAX).is_ok());
        assert!(matches!(
            exhausted.check_with_tokens(0),
            Err(BillingError::UsageLimitExceeded)
        ));
    }

    #[test]
    fn api_quota_does_not_override_subscription_plan() {
        let access = ChatBillingAccess::from_api_and_plan_usage(
            UsageResponse {
                can_use: true,
                is_free: false,
            },
            UsageResponse {
                can_use: true,
                is_free: true,
            },
        );

        assert!(access.can_use());
        assert!(!access.is_paid());
    }

    #[tokio::test]
    async fn api_quota_denial_does_not_wait_for_plan_lookup() {
        async fn check_usage(
            Query(query): Query<HashMap<String, String>>,
        ) -> Json<serde_json::Value> {
            if query.get("api").is_some_and(|value| value == "true") {
                return Json(serde_json::json!({
                    "can_use": false,
                    "is_free": false
                }));
            }

            pending::<Json<serde_json::Value>>().await
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/admin/check-usage", get(check_usage)),
            )
            .await
            .unwrap();
        });
        let client = BillingClient::new("test-key".into(), format!("http://{address}"));

        let access = tokio::time::timeout(
            Duration::from_millis(500),
            client.chat_access(Uuid::new_v4(), true),
        )
        .await
        .expect("API quota denial should not wait for the plan lookup")
        .expect("billing access");

        assert!(!access.can_use());
        assert!(!access.is_paid());
        server.abort();
    }

    async fn deletion_server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{address}"), handle)
    }

    #[tokio::test]
    async fn api_chat_access_uses_one_complete_v2_response() {
        use axum::http::HeaderMap;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        // Purchased API credits grant API admission without changing Free plan
        // identity, even when the independent frontend allowance is exhausted.
        for (api_can_use, frontend_can_use, is_free) in [
            (true, true, false),
            (true, true, true),
            (true, false, true),
            (false, false, false),
            (false, false, true),
        ] {
            let user_id = Uuid::new_v4();
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let legacy_calls = Arc::new(AtomicUsize::new(0));
            let observed_legacy = legacy_calls.clone();
            let (url, server) = deletion_server(
                Router::new()
                    .route(
                        "/v2/admin/check-usage",
                        get(move |headers: HeaderMap, Query(query): Query<HashMap<String, String>>| {
                            let observed = observed.clone();
                            async move {
                                observed.fetch_add(1, Ordering::SeqCst);
                                assert_eq!(headers["x-api-key"], "synthetic-admin-key");
                                assert_eq!(query.len(), 2);
                                assert_eq!(query["user_id"], user_id.to_string());
                                assert_eq!(query["product"], "maple");
                                Json(serde_json::json!({
                                    "api_can_use": api_can_use,
                                    "frontend_can_use": frontend_can_use,
                                    "is_free": is_free,
                                }))
                            }
                        }),
                    )
                    .route(
                        "/v1/admin/check-usage",
                        get(move || {
                            observed_legacy.fetch_add(1, Ordering::SeqCst);
                            async { Json(serde_json::json!({"can_use": true,"is_free":false})) }
                        }),
                    ),
            )
            .await;
            let client = BillingClient::new("synthetic-admin-key".into(), format!("{url}/"));
            let access = client.chat_access(user_id, true).await.unwrap();
            assert_eq!(access.can_use(), api_can_use);
            assert_eq!(access.is_paid(), !is_free && api_can_use);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
            server.abort();
        }
    }

    #[tokio::test]
    async fn api_chat_access_falls_back_only_when_v2_is_absent() {
        use axum::http::StatusCode;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        for status in [StatusCode::NOT_FOUND, StatusCode::METHOD_NOT_ALLOWED] {
            let v2_calls = Arc::new(AtomicUsize::new(0));
            let observed_v2 = v2_calls.clone();
            let api_calls = Arc::new(AtomicUsize::new(0));
            let observed_api = api_calls.clone();
            let plan_calls = Arc::new(AtomicUsize::new(0));
            let observed_plan = plan_calls.clone();
            let (url, server) = deletion_server(
                Router::new()
                    .route(
                        "/v2/admin/check-usage",
                        get(move || {
                            observed_v2.fetch_add(1, Ordering::SeqCst);
                            async move { status }
                        }),
                    )
                    .route(
                        "/v1/admin/check-usage",
                        get(move |Query(query): Query<HashMap<String, String>>| {
                            let api = query.get("api").is_some_and(|value| value == "true");
                            if api {
                                observed_api.fetch_add(1, Ordering::SeqCst);
                            } else {
                                observed_plan.fetch_add(1, Ordering::SeqCst);
                            }
                            async move { Json(serde_json::json!({"can_use":true,"is_free":!api})) }
                        }),
                    ),
            )
            .await;
            // A slash-suffixed configured origin must also work when an older
            // Billing deployment requires the v1 compatibility path.
            let client = BillingClient::new("synthetic-admin-key".into(), format!("{url}/"));
            let access = client.chat_access(Uuid::new_v4(), true).await.unwrap();
            assert!(access.can_use());
            assert!(!access.is_paid());
            assert_eq!(v2_calls.load(Ordering::SeqCst), 1);
            assert_eq!(api_calls.load(Ordering::SeqCst), 1);
            assert_eq!(plan_calls.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn v2_http_failures_never_fan_out_to_v1_or_expose_service_bodies() {
        use axum::http::StatusCode;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        for status in [
            202, 204, 301, 302, 307, 400, 401, 403, 408, 409, 429, 500, 503,
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let legacy_calls = Arc::new(AtomicUsize::new(0));
            let observed_legacy = legacy_calls.clone();
            let (url, server) = deletion_server(
                Router::new()
                    .route(
                        "/v2/admin/check-usage",
                        get(move || {
                            observed.fetch_add(1, Ordering::SeqCst);
                            async move {
                                (
                                    StatusCode::from_u16(status).unwrap(),
                                    "private-service-detail",
                                )
                            }
                        }),
                    )
                    .route(
                        "/v1/admin/check-usage",
                        get(move || {
                            observed_legacy.fetch_add(1, Ordering::SeqCst);
                            async { StatusCode::OK }
                        }),
                    ),
            )
            .await;
            let client = BillingClient::new("synthetic-admin-key".into(), url);
            let error = client.chat_access(Uuid::new_v4(), true).await.unwrap_err();
            assert!(matches!(error, BillingError::ServiceError(_)));
            assert!(!error.to_string().contains("private-service-detail"));
            assert!(error.to_string().contains(&status.to_string()));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
            server.abort();
        }
    }

    #[tokio::test]
    async fn incomplete_malformed_or_oversized_v2_responses_do_not_fall_back() {
        use axum::http::StatusCode;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        for body in [
            "private-service-detail".to_string(),
            "x".repeat(4097),
            serde_json::json!({"api_can_use":true,"frontend_can_use":true}).to_string(),
            serde_json::json!({"api_can_use":true,"is_free":false}).to_string(),
            serde_json::json!({"frontend_can_use":true,"is_free":false}).to_string(),
            serde_json::json!({"api_can_use":"true","frontend_can_use":true,"is_free":false})
                .to_string(),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let legacy_calls = Arc::new(AtomicUsize::new(0));
            let observed_legacy = legacy_calls.clone();
            let (url, server) = deletion_server(
                Router::new()
                    .route(
                        "/v2/admin/check-usage",
                        get(move || {
                            observed.fetch_add(1, Ordering::SeqCst);
                            let body = body.clone();
                            async move { body }
                        }),
                    )
                    .route(
                        "/v1/admin/check-usage",
                        get(move || {
                            observed_legacy.fetch_add(1, Ordering::SeqCst);
                            async { StatusCode::OK }
                        }),
                    ),
            )
            .await;
            let client = BillingClient::new("synthetic-admin-key".into(), url);
            let error = client.chat_access(Uuid::new_v4(), true).await.unwrap_err();
            assert!(matches!(error, BillingError::ParseError(_)));
            assert!(!error.to_string().contains("private-service-detail"));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
            server.abort();
        }
    }

    #[tokio::test]
    async fn frontend_chat_access_uses_its_v2_admission_and_selected_plan() {
        let (url, server) = deletion_server(Router::new().route("/v2/admin/check-usage", get(|| async {
            Json(serde_json::json!({"api_can_use":true,"frontend_can_use":false,"is_free":false,"plan":"max"}))
        }))).await;
        let client = BillingClient::new("synthetic-admin-key".into(), url);
        let access = client.chat_access(Uuid::new_v4(), false).await.unwrap();
        assert!(!access.can_use());
        assert!(access.is_paid());
        assert!(access.has_premium_models());
        server.abort();
    }

    #[tokio::test]
    async fn slow_v2_preserves_the_callers_deadline_without_fallback() {
        use axum::http::StatusCode;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let v2_calls = Arc::new(AtomicUsize::new(0));
        let observed_v2 = v2_calls.clone();
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let observed_legacy = legacy_calls.clone();
        let (url, server) = deletion_server(
            Router::new()
                .route(
                    "/v2/admin/check-usage",
                    get(move || {
                        observed_v2.fetch_add(1, Ordering::SeqCst);
                        pending::<StatusCode>()
                    }),
                )
                .route(
                    "/v1/admin/check-usage",
                    get(move || {
                        observed_legacy.fetch_add(1, Ordering::SeqCst);
                        async { StatusCode::OK }
                    }),
                ),
        )
        .await;
        let client = BillingClient::new("synthetic-admin-key".into(), url);
        assert!(tokio::time::timeout(
            Duration::from_millis(200),
            client.chat_access(Uuid::new_v4(), true)
        )
        .await
        .is_err());
        assert_eq!(v2_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn included_paid_feature_does_not_depend_on_chat_credits() {
        let (url, server) = deletion_server(Router::new().route(
            "/v1/admin/check-usage",
            get(|| async { Json(serde_json::json!({ "can_use": false, "is_free": false })) }),
        ))
        .await;
        let client = BillingClient::new("synthetic-admin-key".into(), url);
        assert!(client.is_user_paid(Uuid::new_v4()).await.unwrap());
        assert!(!client
            .can_user_use_paid_features(Uuid::new_v4())
            .await
            .unwrap());
        server.abort();
    }

    #[tokio::test]
    async fn account_deletion_requires_exact_ready_ack_and_sends_only_user_id() {
        use axum::{http::HeaderMap, routing::post};
        let user_id = Uuid::new_v4();
        let (url, server) = deletion_server(Router::new().route(
            "/v1/admin/account-deletions",
            post(
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(headers["x-api-key"], "synthetic-admin-key");
                    assert_eq!(body, serde_json::json!({"user_id": user_id}));
                    Json(serde_json::json!({"user_id": user_id, "status": "ready"}))
                },
            ),
        ))
        .await;
        let client = BillingClient::new("synthetic-admin-key".into(), format!("{url}/"));
        assert_eq!(client.prepare_account_deletion(user_id).await, Ok(()));
        server.abort();
    }

    #[tokio::test]
    async fn account_deletion_rejects_failure_mismatched_or_pending_ack_without_retry() {
        use axum::{http::StatusCode, routing::post};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let user_id = Uuid::new_v4();
        let wrong_user =
            serde_json::json!({"user_id": Uuid::new_v4(), "status":"ready"}).to_string();
        let pending = serde_json::json!({"user_id": user_id, "status":"accepted"}).to_string();
        let mut cases = vec![
            (200, wrong_user, AccountDeletionError::InvalidResponse),
            (200, pending, AccountDeletionError::InvalidResponse),
            (200, "x".repeat(4097), AccountDeletionError::InvalidResponse),
            (
                200,
                "sensitive-provider-error".into(),
                AccountDeletionError::InvalidResponse,
            ),
        ];
        for status in [202, 204, 401, 403, 404, 409, 429, 500, 503] {
            cases.push((
                status,
                "sensitive-provider-error".into(),
                AccountDeletionError::HttpStatus(status),
            ));
        }
        for (status, body, expected) in cases {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let (url, server) = deletion_server(Router::new().route(
                "/v1/admin/account-deletions",
                post(move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                    let body = body.clone();
                    async move { (StatusCode::from_u16(status).unwrap(), body) }
                }),
            ))
            .await;
            let client = BillingClient::new("synthetic-admin-key".into(), url);
            let error = client.prepare_account_deletion(user_id).await.unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("sensitive-provider-error"));
            if status != 200 {
                assert_eq!(error.to_string(), format!("http_status_{status}"));
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn billing_requests_never_redirect_the_admin_key() {
        use axum::http::StatusCode;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let (target_url, target) = deletion_server(Router::new().fallback(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            async { StatusCode::OK }
        }))
        .await;
        let (url, server) = deletion_server(Router::new().fallback(move || {
            let target_url = format!("{target_url}/collect");
            async move { (StatusCode::TEMPORARY_REDIRECT, [("location", target_url)]) }
        }))
        .await;
        let client = BillingClient::new("synthetic-admin-key".into(), url);
        assert_eq!(
            client.prepare_account_deletion(Uuid::new_v4()).await,
            Err(AccountDeletionError::HttpStatus(307))
        );
        assert!(matches!(
            client.check_usage(Uuid::new_v4(), false).await,
            Err(BillingError::ServiceError(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
        target.abort();
    }

    #[tokio::test]
    async fn account_deletion_timeout_is_bounded_and_not_retried() {
        use axum::{http::StatusCode, routing::post};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let (url, server) = deletion_server(Router::new().route(
            "/v1/admin/account-deletions",
            post(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                pending::<StatusCode>()
            }),
        ))
        .await;
        let client = BillingClient::new("synthetic-admin-key".into(), url);
        let result = tokio::time::timeout(
            Duration::from_secs(12),
            client.prepare_account_deletion(Uuid::new_v4()),
        )
        .await
        .expect("billing request must respect its total deadline");
        assert_eq!(result, Err(AccountDeletionError::Timeout));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
