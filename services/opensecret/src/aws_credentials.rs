use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use vsock::{VsockAddr, VsockStream};

pub(crate) const PARENT_CID: u32 = 3;
pub(crate) const CREDENTIAL_REQUESTER_PORT: u32 = 8003;
pub(crate) const MAX_VSOCK_RESPONSE_BYTES: u64 = 256 * 1024;

pub(crate) fn set_vsock_timeouts(
    stream: &VsockStream,
    timeout: Duration,
) -> Result<(), AwsCredentialError> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(())
}

pub(crate) fn read_vsock_response_string_limited(
    stream: &mut VsockStream,
    max_bytes: u64,
) -> Result<String, AwsCredentialError> {
    let mut limited = (&mut *stream).take(max_bytes.saturating_add(1));
    let mut response = String::new();
    limited.read_to_string(&mut response)?;

    if limited.limit() == 0 {
        return Err(AwsCredentialError::ResponseTooLarge { max_bytes });
    }

    Ok(response)
}

#[derive(Clone, Deserialize, Serialize)]
pub struct AwsCredentials {
    #[serde(rename = "AccessKeyId")]
    pub access_key_id: String,
    #[serde(rename = "SecretAccessKey")]
    pub secret_access_key: String,
    #[serde(rename = "Token")]
    pub token: String,
    #[serde(rename = "Region")]
    pub region: String,
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &"[redacted]")
            .field("secret_access_key", &"[redacted]")
            .field("token", &"[redacted]")
            .field("region", &self.region)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EnclaveRequest {
    pub request_type: String,
    pub key_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ParentResponse {
    pub response_type: String,
    pub response_value: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum AwsCredentialError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Authentication error")]
    Authentication,

    #[error("Timed out waiting for credentials")]
    Timeout,

    #[error("VSOCK response exceeded {max_bytes} bytes")]
    ResponseTooLarge { max_bytes: u64 },
}

#[derive(Clone, Default)]
pub struct AwsCredentialManager {
    credentials: Arc<RwLock<Option<AwsCredentials>>>,
}

impl AwsCredentialManager {
    pub fn new() -> Self {
        Self {
            credentials: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn get_credentials(&self) -> Option<AwsCredentials> {
        let creds = self.credentials.read().await;
        creds.clone()
    }

    pub async fn set_credentials(&self, credentials: AwsCredentials) {
        let mut creds = self.credentials.write().await;
        *creds = Some(credentials);
    }

    pub async fn fetch_credentials(&self) -> Result<AwsCredentials, AwsCredentialError> {
        let creds = Self::fetch_credentials_from_vsock().await?;
        self.set_credentials(creds.clone()).await;
        Ok(creds)
    }

    async fn fetch_credentials_from_vsock() -> Result<AwsCredentials, AwsCredentialError> {
        let cid = PARENT_CID;
        let port = CREDENTIAL_REQUESTER_PORT;

        let sock_addr = VsockAddr::new(cid, port);
        let mut stream = VsockStream::connect(&sock_addr)?;
        set_vsock_timeouts(&stream, Duration::from_secs(30))?;

        let request = EnclaveRequest {
            request_type: "credentials".to_string(),
            key_name: None,
        };
        let request_json = serde_json::to_string(&request)?;
        stream.write_all(request_json.as_bytes())?;

        let response = read_vsock_response_string_limited(&mut stream, MAX_VSOCK_RESPONSE_BYTES)?;

        let parent_response: ParentResponse = serde_json::from_str(&response)?;
        if parent_response.response_type == "credentials" {
            let creds: AwsCredentials = serde_json::from_value(parent_response.response_value)?;
            Ok(creds)
        } else {
            Err(AwsCredentialError::Authentication)
        }
    }

    pub async fn wait_for_credentials(&self) -> AwsCredentials {
        tracing::info!("Waiting for initial AWS credentials");
        let max_retries = 12; // 1 minute total with 5s delay
        let mut attempts = 0;

        loop {
            match self.fetch_credentials().await {
                Ok(c) => return c,
                Err(e) => {
                    attempts += 1;
                    if attempts >= max_retries {
                        tracing::error!(
                            "Failed to get credentials after {} attempts, giving up",
                            max_retries
                        );
                        panic!("Could not obtain AWS credentials after maximum retries");
                    }
                    tracing::error!("Failed to refresh AWS credentials: {:?}", e);
                    tracing::info!(
                        "Retrying in 5 seconds... (attempt {}/{})",
                        attempts,
                        max_retries
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_credentials_debug_output_redacts_secrets() {
        let credentials = AwsCredentials {
            access_key_id: "access-key-id-value".to_string(),
            secret_access_key: "secret-access-key-value".to_string(),
            token: "session-token-value".to_string(),
            region: "us-east-2".to_string(),
        };

        let debug = format!("{credentials:?}");

        assert!(debug.starts_with("AwsCredentials"));
        assert!(debug.contains("us-east-2"));
        for secret in [
            "access-key-id-value",
            "secret-access-key-value",
            "session-token-value",
        ] {
            assert!(!debug.contains(secret), "{secret} leaked into {debug}");
        }
    }
}
