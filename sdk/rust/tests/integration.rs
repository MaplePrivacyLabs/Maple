mod common;

use maple_sdk::{Error, Result};
use std::env;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_client_initialization() -> Result<()> {
    let base_url = env::var("VITE_OPEN_SECRET_API_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    let client = common::new_test_client(base_url)?;
    assert!(client.get_session_id()?.is_none());
    Ok(())
}

#[tokio::test]
async fn test_v1_only_health_check_fails_closed() -> Result<()> {
    // A legacy server may expose plaintext health but no Transport V2 session endpoint.
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/health-check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "healthy",
            "timestamp": "2024-01-01T00:00:00Z"
        })))
        .mount(&mock_server)
        .await;

    let client = common::new_test_client(mock_server.uri())?;
    let error = client.test_connection().await.unwrap_err();
    assert!(matches!(error, Error::InvalidResponse(_)));

    Ok(())
}

#[tokio::test]
async fn test_full_flow_with_real_server() -> Result<()> {
    // Load .env.local from OpenSecret-SDK directory
    dotenvy::from_path("../.env.local").ok();

    // This test requires environment variables to be set
    let base_url = env::var("VITE_OPEN_SECRET_API_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());

    println!("Testing against: {}", base_url);

    let client = common::new_test_client(base_url)?;

    // 1. Test connection
    let health = client.test_connection().await?;
    println!("Health check: {}", health);

    // 2. Perform attestation
    client.perform_attestation_handshake().await?;
    println!("Attestation successful");

    // 3. Verify session
    let session_id = client.get_session_id()?;
    assert!(session_id.is_some(), "Should have session after handshake");
    println!("Session established: {}", session_id.unwrap());

    Ok(())
}
