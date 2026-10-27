use super::*;

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[test]
fn rejects_nonlocal_or_ambiguous_endpoints() {
    for value in [
        "https://example.com/mcp",
        "http://localhost:1234/mcp",
        "http://127.0.0.2:1234/mcp",
        "http://0.0.0.0:1234/mcp",
        "http://127.0.0.1/mcp",
        "http://127.0.0.1:1234/mcp?token=x",
        "http://user@127.0.0.1:1234/mcp",
        "http://127.0.0.1:1234/mcp#x",
        "http://127.0.0.1:1234/other",
    ] {
        assert!(local_endpoint(value).is_err());
    }
    assert_eq!(
        local_endpoint("http://127.0.0.1:49123/mcp").unwrap().port(),
        Some(49123)
    );
}

#[test]
fn callback_rejects_wrong_state_origin_path_host_and_duplicate_parameters() {
    let callback = reqwest::Url::parse("http://127.0.0.1:1234/oauth/callback/nonce").unwrap();
    let request =
        |target: &str, host: &str| format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n");
    for (target, host) in [
        ("/oauth/callback/nonce?code=c&state=wrong", "127.0.0.1:1234"),
        ("/oauth/callback/other?code=c&state=valid", "127.0.0.1:1234"),
        (
            "http://evil.example/oauth/callback/nonce?code=c&state=valid",
            "127.0.0.1:1234",
        ),
        ("/oauth/callback/nonce?code=c&state=valid", "evil.example"),
        (
            "/oauth/callback/nonce?code=c&state=valid&state=valid",
            "127.0.0.1:1234",
        ),
    ] {
        assert!(parse_callback(&request(target, host), &callback, "valid").is_err());
    }
    let answer = parse_callback(
        &request(
            "/oauth/callback/nonce?code=c&state=valid&iss=http%3A%2F%2F127.0.0.1%3A4567",
            "127.0.0.1:1234",
        ),
        &callback,
        "valid",
    )
    .unwrap();
    assert_eq!(answer.code, "c");
    assert_eq!(answer.issuer.as_deref(), Some("http://127.0.0.1:4567"));
}

#[tokio::test]
async fn timeout_drops_the_callback_listener() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let callback =
        reqwest::Url::parse(&format!("http://127.0.0.1:{port}/oauth/callback/nonce")).unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            receive_callback(listener, &callback, "state")
        )
        .await
        .is_err()
    );
    assert!(
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok()
    );
}

#[test]
fn oauth_http_cannot_follow_metadata_to_another_listener_or_public_service() {
    let http = LocalOAuthHttp::new(local_endpoint("http://127.0.0.1:1234/mcp").unwrap()).unwrap();
    for value in [
        "http://127.0.0.1:1235/token",
        "https://account.vana.org/token",
        "http://localhost:1234/token",
    ] {
        assert!(
            http.check_url(&reqwest::Url::parse(value).unwrap())
                .is_err()
        );
    }
    assert!(
        http.check_url(&reqwest::Url::parse("http://127.0.0.1:1234/mcp/oauth/token").unwrap())
            .is_ok()
    );
}

#[tokio::test]
async fn local_mcp_stream_can_outlive_the_oauth_exchange_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let first = "data: ready\n\n";
    let last = "data: done\n\n";
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&chunk[..count]);
        }
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{first}",
            first.len() + last.len(),
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        // Deliberately cross the OAuth client's total timeout. MCP may retain
        // streams longer and applies its own request/progress deadlines.
        tokio::time::sleep(HTTP_TIMEOUT + Duration::from_millis(250)).await;
        stream.write_all(last.as_bytes()).await.unwrap();
    });
    let response = local_mcp_client()
        .unwrap()
        .get(format!("http://{address}/mcp"))
        .send()
        .await
        .unwrap();
    let body = tokio::time::timeout(HTTP_TIMEOUT + Duration::from_secs(5), response.text())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, format!("{first}{last}"));
    server.await.unwrap();
}

struct FakeOAuth {
    base: String,
    exchanges: AtomicUsize,
    refreshes: AtomicUsize,
    revoked: AtomicBool,
    challenge: Mutex<Option<String>>,
}

struct Server {
    state: Arc<FakeOAuth>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = Arc::new(FakeOAuth {
            base: format!("http://{}", listener.local_addr().unwrap()),
            exchanges: AtomicUsize::new(0),
            refreshes: AtomicUsize::new(0),
            revoked: AtomicBool::new(false),
            challenge: Mutex::new(None),
        });
        let app = Router::new()
            .fallback(any(fake_http))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { state, task }
    }

    fn endpoint(&self) -> String {
        format!("{}/mcp", self.state.base)
    }

    fn browser(&self) -> LocalMcpBrowser {
        self.browser_with_issuer(None)
    }

    fn browser_with_issuer(&self, overridden_issuer: Option<String>) -> LocalMcpBrowser {
        let state = self.state.clone();
        Arc::new(move |authorization| {
            let url = reqwest::Url::parse(&authorization).unwrap();
            assert_eq!(url.origin().ascii_serialization(), state.base);
            let params: HashMap<_, _> = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            assert_eq!(
                params.get("code_challenge_method").map(String::as_str),
                Some("S256")
            );
            *state.challenge.lock().unwrap() = params.get("code_challenge").cloned();
            let mut callback = reqwest::Url::parse(params.get("redirect_uri").unwrap()).unwrap();
            let csrf = params.get("state").unwrap().clone();
            let issuer = overridden_issuer
                .clone()
                .unwrap_or_else(|| state.base.clone());
            tokio::spawn(async move {
                let client = reqwest::Client::builder().no_proxy().build().unwrap();
                callback
                    .query_pairs_mut()
                    .append_pair("code", "synthetic-approval-code")
                    .append_pair("state", "wrong")
                    .append_pair("iss", &issuer);
                assert_eq!(
                    client.get(callback.clone()).send().await.unwrap().status(),
                    StatusCode::BAD_REQUEST
                );
                callback.set_query(None);
                callback
                    .query_pairs_mut()
                    .append_pair("code", "synthetic-approval-code")
                    .append_pair("state", &csrf)
                    .append_pair("iss", &issuer);
                assert_eq!(
                    client.get(callback).send().await.unwrap().status(),
                    StatusCode::OK
                );
            });
            Ok(())
        })
    }
}

fn json_response(value: Value) -> Response {
    ([("content-type", "application/json")], value.to_string()).into_response()
}

fn challenge(state: &FakeOAuth) -> Response {
    (StatusCode::UNAUTHORIZED, [("www-authenticate", format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\", scope=\"vana:read\"", state.base))]).into_response()
}

async fn fake_http(
    State(state): State<Arc<FakeOAuth>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match (method, uri.path()) {
        (Method::GET, "/.well-known/oauth-protected-resource/mcp") => json_response(
            json!({"resource":format!("{}/mcp",state.base),"authorization_servers":[state.base],"scopes_supported":["vana:read"]}),
        ),
        (Method::GET, "/.well-known/oauth-authorization-server") => json_response(
            json!({"issuer":state.base,"authorization_endpoint":format!("{}/mcp/oauth/authorize",state.base),"token_endpoint":format!("{}/mcp/oauth/token",state.base),"registration_endpoint":format!("{}/mcp/oauth/register",state.base),"scopes_supported":["vana:read"],"response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"]}),
        ),
        (Method::POST, "/mcp/oauth/register") => {
            let input: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(input["client_name"], "Maple Agent");
            json_response(
                json!({"client_id":"synthetic-maple-client","redirect_uris":input["redirect_uris"],"token_endpoint_auth_method":"none"}),
            )
        }
        (Method::POST, "/mcp/oauth/token") => {
            let encoded = format!("http://unused/?{}", String::from_utf8_lossy(&body));
            let url = reqwest::Url::parse(&encoded).unwrap();
            let params: HashMap<_, _> = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            if params.get("grant_type").map(String::as_str) == Some("refresh_token") {
                state.refreshes.fetch_add(1, Ordering::SeqCst);
                if state.revoked.load(Ordering::SeqCst) {
                    return (
                        StatusCode::BAD_REQUEST,
                        [("content-type", "application/json")],
                        "{\"error\":\"invalid_grant\"}",
                    )
                        .into_response();
                }
            } else {
                assert_eq!(
                    params.get("code").map(String::as_str),
                    Some("synthetic-approval-code")
                );
                let verifier = params.get("code_verifier").unwrap();
                let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(Sha256::digest(verifier));
                assert_eq!(state.challenge.lock().unwrap().as_ref(), Some(&challenge));
                state.exchanges.fetch_add(1, Ordering::SeqCst);
            }
            let n = state.exchanges.load(Ordering::SeqCst) + state.refreshes.load(Ordering::SeqCst);
            json_response(
                json!({"access_token":format!("synthetic-access-{n}"),"token_type":"Bearer","expires_in":3600,"refresh_token":format!("synthetic-refresh-{n}"),"scope":"vana:read"}),
            )
        }
        (Method::GET, "/mcp") => challenge(&state),
        (Method::POST, "/mcp") => {
            if state.revoked.load(Ordering::SeqCst)
                || !headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.starts_with("Bearer synthetic-access-"))
            {
                return challenge(&state);
            }
            let input: Value = serde_json::from_slice(&body).unwrap();
            let result = match input["method"].as_str().unwrap_or_default() {
                "initialize" => {
                    json!({"protocolVersion":input["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"synthetic-vana","version":"1"}})
                }
                "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
                "tools/list" => {
                    json!({"tools":[{"name":"list_granted_scopes","inputSchema":{"type":"object"}}]})
                }
                "tools/call" => {
                    assert_eq!(input["params"]["arguments"], json!({}));
                    json!({"content":[{"type":"text","text":"{\"scopes\":[{\"scope\":\"chatgpt.memories\",\"dataStatus\":\"ready\"}]}"}],"isError":false})
                }
                _ => return StatusCode::NOT_FOUND.into_response(),
            };
            json_response(json!({"jsonrpc":"2.0","id":input["id"],"result":result}))
        }
        (Method::DELETE, "/mcp") => StatusCode::NO_CONTENT.into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[tokio::test]
async fn native_pkce_callback_persists_refreshes_and_revocation_requires_reconnect() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    let definitions = harness
        .handle
        .connect_vana_local(server.endpoint(), server.browser())
        .await
        .unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "vana");
    assert!(!definitions[0].enabled);
    assert!(definitions[0].headers.is_empty());
    assert!(definitions[0].environment.is_empty());
    assert_eq!(server.state.exchanges.load(Ordering::SeqCst), 1);
    let context = OAuthAccountContext::new(harness.handle.store().unwrap());
    let target = McpTarget::StreamableHttp {
        url: server.endpoint(),
    };
    let record = context
        .store
        .mcp_oauth_credentials(context.account, "vana", &target)
        .unwrap()
        .unwrap();
    let mut expired = decode_credentials(&record).unwrap();
    expired.token_received_at = Some(0);
    context
        .store
        .rotate_mcp_oauth_credentials(
            context.account,
            "vana",
            &target,
            &record,
            credentials_secret(&expired).unwrap(),
        )
        .unwrap();
    // Two independently opened tasks coordinate the same persisted refresh.
    let first = runtime_client(&context, "vana", &server.endpoint())
        .await
        .unwrap()
        .unwrap();
    let second = runtime_client(&context, "vana", &server.endpoint())
        .await
        .unwrap()
        .unwrap();
    let (a, b) = tokio::join!(first.access_token(None), second.access_token(None));
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(server.state.refreshes.load(Ordering::SeqCst), 1);
    let refreshed = context
        .store
        .mcp_oauth_credentials(context.account, "vana", &target)
        .unwrap()
        .unwrap();
    assert!(
        decode_credentials(&refreshed)
            .unwrap()
            .token_received_at
            .unwrap()
            > 0
    );
    // Revoked credentials are cleared, not retried indefinitely or sent to a
    // public listener; reconnect remains an explicit user operation.
    server.state.revoked.store(true, Ordering::SeqCst);
    let mut expired = decode_credentials(&refreshed).unwrap();
    expired.token_received_at = Some(0);
    context
        .store
        .rotate_mcp_oauth_credentials(
            context.account,
            "vana",
            &target,
            &refreshed,
            credentials_secret(&expired).unwrap(),
        )
        .unwrap();
    assert!(
        runtime_client(&context, "vana", &server.endpoint())
            .await
            .unwrap()
            .unwrap()
            .access_token(None)
            .await
            .is_err()
    );
    assert!(
        context
            .store
            .mcp_oauth_credentials(context.account, "vana", &target)
            .unwrap()
            .is_none()
    );
    assert!(
        runtime_client(&context, "vana", &server.endpoint())
            .await
            .is_err()
    );
    assert!(
        context
            .store
            .mcp_oauth_managed(context.account, "vana", &target)
            .unwrap()
    );
    server.state.revoked.store(false, Ordering::SeqCst);
    assert!(
        harness
            .handle
            .connect_vana_local(server.endpoint(), server.browser())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn forged_issuer_never_redeems_the_code_or_publishes_a_definition() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    assert!(
        harness
            .handle
            .connect_vana_local(
                server.endpoint(),
                server.browser_with_issuer(Some("http://127.0.0.1:1".into()))
            )
            .await
            .is_err()
    );
    assert_eq!(server.state.exchanges.load(Ordering::SeqCst), 0);
    assert!(
        harness
            .handle
            .list_mcp_definitions()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn transient_http_bindings_keep_their_static_header_path() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = crate::agent::mcp::fake_server::FakeServer::start(None).await;
    let context = OAuthAccountContext::new(harness.handle.store().unwrap());
    let config = crate::agent::AgentMcpServer {
        name: "transient".into(),
        description: String::new(),
        enabled: true,
        timeout_seconds: 5,
        transport: crate::agent::AgentMcpTransport::StreamableHttp {
            url: server.url.clone(),
            environment: Vec::new(),
            headers: vec![crate::agent::AgentMcpKeyValue {
                key: "Authorization".into(),
                value: "Bearer synthetic-transient".into(),
            }],
        },
    };
    let client = crate::agent::mcp::connection::McpServer::new(
        config,
        &std::env::temp_dir(),
        None,
        Arc::new(|| {}),
    )
    .with_oauth_context(Some(context));
    assert!(client.connect().await.is_ok());
    assert!(
        server
            .authorization()
            .iter()
            .any(|value| value.as_deref() == Some("Bearer synthetic-transient"))
    );
}

#[tokio::test]
async fn credential_adapter_rechecks_revocation_after_waiting_on_its_mutex() {
    for operation in ["save", "clear", "load"] {
        let harness = crate::agent::tests::Harness::new().await;
        let server = Server::start().await;
        harness
            .handle
            .connect_vana_local(server.endpoint(), server.browser())
            .await
            .unwrap();
        let context = OAuthAccountContext::new(harness.handle.store().unwrap());
        let target = McpTarget::StreamableHttp {
            url: server.endpoint(),
        };
        let record = context
            .store
            .mcp_oauth_credentials(context.account, "vana", &target)
            .unwrap()
            .unwrap();
        let payload = decode_credentials(&record).unwrap();
        let adapter =
            HostCredentialStore::new(context.clone(), "vana", target.clone(), record.clone());
        let held = adapter.current.lock().await;
        let future: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), AuthError>> + Send + '_>,
        > = match operation {
            "save" => Box::pin(adapter.save(payload)),
            "clear" => Box::pin(adapter.clear()),
            _ => Box::pin(async { adapter.load().await.map(|_| ()) }),
        };
        tokio::pin!(future);
        assert!(futures_util::poll!(future.as_mut()).is_pending());
        context.owner.revoke();
        drop(held);
        assert!(future.await.is_err());
        let after = context
            .store
            .mcp_oauth_credentials(context.account, "vana", &target)
            .unwrap()
            .unwrap();
        assert_eq!(after.generation, record.generation);
        assert_eq!(after.revision, record.revision);
        assert_eq!(after.payload, record.payload);
    }
}

#[test]
fn oauth_errors_do_not_echo_token_or_callback_material() {
    let marker = "synthetic-private-oauth-value";
    let auth = sanitized_auth(AuthError::TokenExchangeFailed(marker.into()));
    assert!(!format!("{auth:?}").contains(marker));
    let transport = sanitized_transport(StreamableHttpError::UnexpectedContentType(Some(
        marker.into(),
    )));
    assert!(!format!("{transport:?}").contains(marker));
}

#[tokio::test]
async fn a_delayed_rejection_does_not_clear_a_newer_persisted_token() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    harness
        .handle
        .connect_vana_local(server.endpoint(), server.browser())
        .await
        .unwrap();
    let context = OAuthAccountContext::new(harness.handle.store().unwrap());
    let target = McpTarget::StreamableHttp {
        url: server.endpoint(),
    };
    let old_client = runtime_client(&context, "vana", &server.endpoint())
        .await
        .unwrap()
        .unwrap();
    let rejected = old_client.access_token(None).await.unwrap();
    let old_record = context
        .store
        .mcp_oauth_credentials(context.account, "vana", &target)
        .unwrap()
        .unwrap();
    let mut expired = decode_credentials(&old_record).unwrap();
    expired.token_received_at = Some(0);
    context
        .store
        .rotate_mcp_oauth_credentials(
            context.account,
            "vana",
            &target,
            &old_record,
            credentials_secret(&expired).unwrap(),
        )
        .unwrap();
    let new_client = runtime_client(&context, "vana", &server.endpoint())
        .await
        .unwrap()
        .unwrap();
    let fresh = new_client.access_token(None).await.unwrap();
    assert_ne!(rejected, fresh);
    let refreshed = context
        .store
        .mcp_oauth_credentials(context.account, "vana", &target)
        .unwrap()
        .unwrap();
    old_client.clear_rejected_token(&rejected).await.unwrap();
    let retained = context
        .store
        .mcp_oauth_credentials(context.account, "vana", &target)
        .unwrap()
        .unwrap();
    assert_eq!(retained.revision, refreshed.revision);
    assert_eq!(retained.payload, refreshed.payload);
}
