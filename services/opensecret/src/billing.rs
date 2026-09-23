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

const fn default_is_free() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChatBillingAccess {
    can_use: bool,
    is_free: bool,
}

impl ChatBillingAccess {
    fn from_frontend_usage(usage: UsageResponse) -> Self {
        Self {
            can_use: usage.can_use,
            is_free: usage.is_free,
        }
    }

    fn from_api_and_plan_usage(api_usage: UsageResponse, plan_usage: UsageResponse) -> Self {
        Self {
            can_use: api_usage.can_use,
            is_free: plan_usage.is_free,
        }
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
    #[error("http_status")]
    HttpStatus,
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
    account_deletion_client: Client,
    api_key: String,
    base_url: String,
}

impl BillingClient {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self {
            client: crate::http_client::client(),
            account_deletion_client: crate::http_client::client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()
                .expect("failed to build billing account-deletion HTTP client"),
            api_key,
            base_url,
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
                .account_deletion_client
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
                return Err(AccountDeletionError::HttpStatus);
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

    pub(crate) async fn chat_access(
        &self,
        user_id: Uuid,
        is_api: bool,
    ) -> Result<ChatBillingAccess, BillingError> {
        if !is_api {
            return self
                .check_usage(user_id, false)
                .await
                .map(ChatBillingAccess::from_frontend_usage);
        }

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
        for status in [202, 204, 401, 403, 409, 429, 500, 503] {
            cases.push((
                status,
                "sensitive-provider-error".into(),
                AccountDeletionError::HttpStatus,
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
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn account_deletion_never_redirects_the_admin_key() {
        use axum::{http::StatusCode, routing::post};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let (target_url, target) = deletion_server(Router::new().route(
            "/collect",
            post(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                async { StatusCode::OK }
            }),
        ))
        .await;
        let (url, server) = deletion_server(Router::new().route(
            "/v1/admin/account-deletions",
            post(move || {
                let target_url = format!("{target_url}/collect");
                async move { (StatusCode::TEMPORARY_REDIRECT, [("location", target_url)]) }
            }),
        ))
        .await;
        let client = BillingClient::new("synthetic-admin-key".into(), url);
        assert_eq!(
            client.prepare_account_deletion(Uuid::new_v4()).await,
            Err(AccountDeletionError::HttpStatus)
        );
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
