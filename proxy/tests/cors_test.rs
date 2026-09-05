use axum::{
    body::{to_bytes, Body},
    http::{header, Method, Request, StatusCode},
};
use maple_proxy::{create_app, Config};
use tower::ServiceExt;

fn cors_config() -> Config {
    Config::new("127.0.0.1".into(), 0, "http://127.0.0.1:1".into())
        .with_cors(true)
        .with_api_key("saved-key-must-not-be-spent".into())
}

#[tokio::test]
async fn preflight_explicitly_allows_authorization_and_requested_headers() {
    let response = create_app(cors_config())
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/v1/chat/completions")
                .header(header::ORIGIN, "https://browser.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization,content-type,x-provider-beta",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    let allowed_headers = response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
        .to_str()
        .unwrap()
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>();
    for expected in ["authorization", "content-type", "x-provider-beta"] {
        assert!(allowed_headers.contains(&expected));
    }
    assert!(!allowed_headers.contains(&"*"));
    assert!(response.headers()[header::ACCESS_CONTROL_ALLOW_METHODS]
        .to_str()
        .unwrap()
        .split(',')
        .any(|method| method.trim() == "POST"));
}

#[tokio::test]
async fn browser_request_without_caller_key_cannot_spend_configured_default() {
    let response = create_app(cors_config())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header(header::ORIGIN, "https://browser.example")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("saved-key-must-not-be-spent"));
}
