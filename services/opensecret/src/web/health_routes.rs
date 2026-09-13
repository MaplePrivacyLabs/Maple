use axum::{routing::get, Json, Router};
use serde::Serialize;

const API_VERSION: &str = "v1";

/// Process liveness only: an upstream outage must not remove a healthy enclave
/// from the load balancer.
pub fn router() -> Router {
    Router::new().route("/health-check", get(health_check))
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "pass",
        version: API_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header, StatusCode};
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn health_is_dependency_free_and_extended_health_is_removed() {
        // Deliberately construct only the production health router: no
        // AppState, database, provider client, credentials, or model service.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router()).await.unwrap();
        });
        let client = crate::http_client::client_builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let response = client
            .get(format!("http://{address}/health-check"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap(),
            json!({"status": "pass", "version": "v1"}),
            "liveness must not claim outbound connectivity or model health"
        );

        let removed = client
            .get(format!("http://{address}/health-check-extended"))
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), StatusCode::NOT_FOUND);

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
