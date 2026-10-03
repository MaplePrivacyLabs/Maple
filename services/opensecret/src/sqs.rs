use crate::aws_credentials::AwsCredentialManager;
use aws_sdk_sqs::{config::Credentials, Client as SqsClient};
use backoff::SystemClock;
use backoff::{exponential::ExponentialBackoff, future::retry, Error as BackoffError};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, error, info};
use uuid::Uuid;

const DEFAULT_REGION: &str = "us-east-2";
const INITIAL_INTERVAL_MS: u64 = 100;
const MAX_INTERVAL_MS: u64 = 10_000; // 10 seconds
const MAX_ELAPSED_TIME_SECS: u64 = 120; // 2 minutes

#[derive(Clone)]
pub struct SqsEventPublisher {
    queue_url: String,
    aws_credential_manager: Arc<RwLock<Option<AwsCredentialManager>>>,
    region: String,
    client_pool: Arc<RwLock<Option<(SqsClient, Instant)>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub event_id: Uuid,
    pub user_id: Uuid,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub estimated_cost: BigDecimal,
    pub chat_time: DateTime<Utc>,
    #[serde(default)]
    pub is_api_request: bool,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub model_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_sqs::{
        config::{retry::RetryConfig, Region},
        error::SdkError,
        operation::send_message::{SendMessageError, SendMessageOutput},
    };
    use axum::{http::header, routing::post, Router};
    use serde_json::json;
    use std::str::FromStr;

    async fn send_message_with_nested_response(
        depth: usize,
    ) -> Result<SendMessageOutput, SdkError<SendMessageError>> {
        // Build raw JSON so serde_json's depth limit does not preempt the AWS parser.
        let response_body = format!(
            r#"{{"UnknownField":{}0{},"MessageId":"test-message","MD5OfMessageBody":"5d41402abc4b2a76b9719d911017c592"}}"#,
            "[".repeat(depth),
            "]".repeat(depth),
        );
        let app = Router::new().route(
            "/",
            post(move || {
                let response_body = response_body.clone();
                async move {
                    (
                        [(header::CONTENT_TYPE, "application/x-amz-json-1.0")],
                        response_body,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake SQS server");
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // Construct the service config directly: no environment, credentials chain, or IMDS.
        let config = aws_sdk_sqs::Config::builder()
            .behavior_version_latest()
            .region(Region::new(DEFAULT_REGION))
            .credentials_provider(Credentials::new(
                "test-key",
                "test-secret",
                None,
                None,
                "test",
            ))
            .endpoint_url(&endpoint)
            .retry_config(RetryConfig::disabled())
            .build();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            SqsClient::from_conf(config)
                .send_message()
                .queue_url(format!("{endpoint}/test-queue"))
                .message_body("hello")
                .send(),
        )
        .await;
        server.abort();
        let _ = server.await;
        result.expect("SQS response parsing must finish within five seconds")
    }

    #[tokio::test]
    async fn sqs_send_message_accepts_normal_nested_response_fields() {
        let response = send_message_with_nested_response(8)
            .await
            .expect("ordinary unknown response fields must remain compatible");

        assert_eq!(response.message_id(), Some("test-message"));
        assert_eq!(
            response.md5_of_message_body(),
            Some("5d41402abc4b2a76b9719d911017c592")
        );
    }

    #[tokio::test]
    async fn sqs_send_message_rejects_excessively_nested_response_fields() {
        // Exceed the patched parser's 512-level limit without a stack-overflow payload.
        let error = send_message_with_nested_response(600)
            .await
            .expect_err("the SDK must reject response fields beyond its nesting limit");

        assert!(
            error
                .raw_response()
                .is_some_and(|response| response.status().is_success()),
            "the error must come from parsing a successful fake SQS response: {error:?}"
        );
    }

    #[test]
    fn usage_event_omits_missing_cached_input_tokens() {
        let event = UsageEvent {
            event_id: Uuid::parse_str("8c6c975a-1f33-439d-98f1-7dd26d4d3e89").unwrap(),
            user_id: Uuid::parse_str("6142db59-fc0c-413d-8792-579fc1457fe2").unwrap(),
            input_tokens: 100,
            output_tokens: 20,
            estimated_cost: BigDecimal::from_str("0.001").unwrap(),
            chat_time: Utc::now(),
            is_api_request: false,
            provider_name: "continuum".to_string(),
            model_name: "kimi-k2-6".to_string(),
            cached_input_tokens: None,
        };

        let serialized = serde_json::to_value(event).unwrap();

        assert!(serialized.get("cached_input_tokens").is_none());
    }

    #[test]
    fn usage_event_serializes_cached_input_tokens_when_present() {
        let event = UsageEvent {
            event_id: Uuid::parse_str("8c6c975a-1f33-439d-98f1-7dd26d4d3e89").unwrap(),
            user_id: Uuid::parse_str("6142db59-fc0c-413d-8792-579fc1457fe2").unwrap(),
            input_tokens: 100,
            output_tokens: 20,
            estimated_cost: BigDecimal::from_str("0.001").unwrap(),
            chat_time: Utc::now(),
            is_api_request: true,
            provider_name: "continuum".to_string(),
            model_name: "kimi-k2-6".to_string(),
            cached_input_tokens: Some(42),
        };

        let serialized = serde_json::to_value(event).unwrap();

        assert_eq!(serialized.get("cached_input_tokens"), Some(&json!(42)));
    }

    #[test]
    fn usage_event_deserializes_missing_cached_input_tokens_as_none() {
        let event: UsageEvent = serde_json::from_value(json!({
            "event_id": "8c6c975a-1f33-439d-98f1-7dd26d4d3e89",
            "user_id": "6142db59-fc0c-413d-8792-579fc1457fe2",
            "input_tokens": 100,
            "output_tokens": 20,
            "estimated_cost": "0.001",
            "chat_time": Utc::now(),
            "is_api_request": false,
            "provider_name": "continuum",
            "model_name": "kimi-k2-6"
        }))
        .unwrap();

        assert_eq!(event.cached_input_tokens, None);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SqsError {
    #[error("AWS SDK error: {0}")]
    AwsSdk(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("No credentials available")]
    NoCredentials,
}

impl SqsEventPublisher {
    pub async fn new(
        queue_url: String,
        region: Option<String>,
        aws_credential_manager: Arc<RwLock<Option<AwsCredentialManager>>>,
    ) -> Self {
        let region = region.unwrap_or_else(|| DEFAULT_REGION.to_string());
        Self {
            queue_url,
            aws_credential_manager,
            region,
            client_pool: Arc::new(RwLock::new(None)),
        }
    }

    async fn get_or_create_client(&self) -> Result<SqsClient, SqsError> {
        const CLIENT_MAX_AGE: Duration = Duration::from_secs(5 * 60 * 60); // 5 hours

        // Check if we have a valid cached client
        {
            let pool = self.client_pool.read().await;
            if let Some((client, created_at)) = &*pool {
                if created_at.elapsed() < CLIENT_MAX_AGE {
                    debug!("Reusing existing SQS client");
                    return Ok(client.clone());
                }
                debug!("SQS client expired, creating new one");
            }
        }

        // Need to create a new client
        let mut pool = self.client_pool.write().await;

        // Double-check in case another thread already created one
        if let Some((client, created_at)) = &*pool {
            if created_at.elapsed() < CLIENT_MAX_AGE {
                return Ok(client.clone());
            }
        }

        info!("Creating new SQS client");

        let creds = if let Some(manager) = self.aws_credential_manager.read().await.as_ref() {
            // Fetch fresh credentials when creating new client
            manager
                .fetch_credentials()
                .await
                .map_err(|_| SqsError::NoCredentials)?
        } else {
            debug!("Using default AWS credential chain");
            let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_types::region::Region::new(self.region.clone()))
                .load()
                .await;
            let client = SqsClient::new(&config);
            *pool = Some((client.clone(), Instant::now()));
            return Ok(client);
        };

        let aws_creds = Credentials::new(
            creds.access_key_id,
            creds.secret_access_key,
            Some(creds.token),
            None,
            "sqs-publisher",
        );

        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_types::region::Region::new(self.region.clone()))
            .credentials_provider(aws_creds)
            .load()
            .await;

        let client = SqsClient::new(&config);
        *pool = Some((client.clone(), Instant::now()));

        info!("Created new SQS client with fresh credentials");
        Ok(client)
    }

    pub async fn publish_event(&self, event: UsageEvent) -> Result<(), SqsError> {
        let event_id = event.event_id;
        let user_id = event.user_id;

        info!("Publishing event {} for user {}", event_id, user_id);

        let backoff = ExponentialBackoff::<SystemClock> {
            initial_interval: Duration::from_millis(INITIAL_INTERVAL_MS),
            max_interval: Duration::from_millis(MAX_INTERVAL_MS),
            multiplier: 2.0,
            max_elapsed_time: Some(Duration::from_secs(MAX_ELAPSED_TIME_SECS)),
            ..ExponentialBackoff::default()
        };

        let result = retry(backoff, || async {
            let client = match self.get_or_create_client().await {
                Ok(client) => client,
                Err(e) => return Err(BackoffError::transient(e)),
            };

            let message_body = serde_json::to_string(&event)
                .map_err(|e| BackoffError::permanent(SqsError::Serialization(e)))?;

            match client
                .send_message()
                .queue_url(&self.queue_url)
                .message_body(&message_body)
                .send()
                .await
            {
                Ok(_) => Ok(()),
                Err(e) => Err(BackoffError::transient(SqsError::AwsSdk(e.to_string()))),
            }
        })
        .await;

        match result {
            Ok(_) => {
                info!(
                    "Successfully published event {} for user {} to SQS",
                    event_id, user_id
                );
                Ok(())
            }
            Err(e) => {
                error!(
                    "Failed to publish event {} for user {} after retries: {}",
                    event_id, user_id, e
                );
                Err(SqsError::AwsSdk(
                    "Failed to publish after retries".to_string(),
                ))
            }
        }
    }
}
