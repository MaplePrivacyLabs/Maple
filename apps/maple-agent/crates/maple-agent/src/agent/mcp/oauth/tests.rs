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
fn a_sign_in_takes_any_http_or_https_address() {
    for value in [
        "https://mcp.example.com/mcp?tenant=a",
        "http://127.0.0.1:49123/mcp",
        "http://localhost:1234/mcp",
        "http://[::1]:1234/mcp",
        "http://192.168.1.20:8123/api/mcp",
        "http://100.64.0.1:8123/api/mcp",
        "http://homeassistant:8123/api/mcp",
        "http://homeassistant.local:8123/api/mcp",
        "http://home.example.com:8123/api/mcp",
    ] {
        assert!(sign_in_endpoint(value).is_ok(), "{value}");
    }
}

#[test]
fn a_sign_in_address_cannot_carry_more_than_an_address() {
    for value in [
        "ftp://example.com/mcp",
        "file:///etc/passwd",
        "https://user@example.com/mcp",
        "http://user:secret@homeassistant.local:8123/api/mcp",
        "https://example.com/mcp#x",
        "http://127.0.0.1:1234/mcp#x",
        "not an address",
    ] {
        assert!(sign_in_endpoint(value).is_err(), "{value}");
    }
}

#[test]
fn a_plain_http_servers_metadata_stays_on_that_server() {
    let endpoint = sign_in_endpoint("http://homeassistant.local:8123/api/mcp").unwrap();
    let check = |value: &str| check_sign_in_url(&endpoint, &reqwest::Url::parse(value).unwrap());
    for value in [
        "http://other.local:8123/auth/authorize",
        "http://homeassistant.local:8124/auth/authorize",
        "https://homeassistant.local:8123/auth/authorize",
        "https://example.com/auth/authorize",
        "http://user@homeassistant.local:8123/auth/authorize",
    ] {
        assert!(check(value).is_err(), "{value}");
    }
    assert!(check("http://homeassistant.local:8123/auth/authorize").is_ok());
}

fn metadata_with(authorization: &str, token: &str, issuer: Option<&str>) -> AuthorizationMetadata {
    serde_json::from_value(serde_json::json!({
        "authorization_endpoint": authorization,
        "token_endpoint": token,
        "issuer": issuer,
    }))
    .unwrap()
}

#[test]
fn paths_in_metadata_mean_the_server_they_came_from() {
    let endpoint = sign_in_endpoint("http://homeassistant.local:8123/api/mcp").unwrap();
    let mut metadata = metadata_with("/auth/authorize", "/auth/token", None);
    assert!(fill_in_paths(&endpoint, &mut metadata));
    assert_eq!(
        metadata.authorization_endpoint,
        "http://homeassistant.local:8123/auth/authorize"
    );
    assert_eq!(
        metadata.token_endpoint,
        "http://homeassistant.local:8123/auth/token"
    );
    assert_eq!(
        metadata.issuer.as_deref(),
        Some("http://homeassistant.local:8123")
    );
}

#[test]
fn only_paths_are_filled_in() {
    let endpoint = sign_in_endpoint("http://homeassistant.local:8123/api/mcp").unwrap();
    for (authorization, token, issuer) in [
        // A name for another host is not a path.
        ("//example.com/authorize", "/auth/token", None),
        ("/auth/authorize", "//example.com/token", None),
        // An absolute address has to pass the checks that apply to it.
        ("https://example.com/authorize", "/auth/token", None),
        // A server that names an issuer has answered for itself.
        (
            "/auth/authorize",
            "/auth/token",
            Some("https://example.com"),
        ),
    ] {
        let mut metadata = metadata_with(authorization, token, issuer);
        assert!(!fill_in_paths(&endpoint, &mut metadata), "{authorization}");
        assert_eq!(metadata.authorization_endpoint, authorization);
        assert_eq!(metadata.token_endpoint, token);
    }
}

#[test]
fn a_remote_servers_metadata_may_only_send_the_browser_to_https() {
    let endpoint = sign_in_endpoint("https://mcp.example.com/mcp").unwrap();
    let check = |value: &str| check_sign_in_url(&endpoint, &reqwest::Url::parse(value).unwrap());
    for value in [
        "http://auth.example.com/authorize",
        "file:///etc/passwd",
        "maple://authorize",
        "https://user@auth.example.com/authorize",
    ] {
        assert!(check(value).is_err());
    }
    assert!(check("https://auth.example.com/authorize").is_ok());
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
    // A parallel test can take a freed port, so one refusal proves nothing.
    for _ in 0..5 {
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
        if TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok()
        {
            return;
        }
    }
    panic!("the listener outlived its timeout");
}

#[test]
fn oauth_http_cannot_follow_metadata_to_another_listener_or_public_service() {
    let http = PlainOAuthHttp::new(sign_in_endpoint("http://127.0.0.1:1234/mcp").unwrap()).unwrap();
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
    let response = mcp_client(&reqwest::Url::parse("http://127.0.0.1:1/mcp").unwrap())
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
    /// Issue a client secret at registration and require it on refreshes.
    confidential: AtomicBool,
    /// Advertise another token endpoint from now on.
    token_moved: AtomicBool,
    /// Name a document that claims to be the issuer, instead of the issuer.
    decoy_issuer: AtomicBool,
    moved_token_calls: AtomicUsize,
    secret_refreshes: AtomicUsize,
    challenge: Mutex<Option<String>>,
    /// No registration, and nothing to say a client's address will do.
    no_registration: AtomicBool,
    /// No registration: the server knows a client by its address.
    by_address: AtomicBool,
    /// Sign-in addresses given as paths, with no issuer.
    gives_paths: AtomicBool,
    /// No MCP server here, though sign-in metadata is still published.
    no_server: AtomicBool,
    /// Names its sign-in server by a private address, as a server does that is
    /// reached at its own one.
    names_private_address: AtomicBool,
    registrations: AtomicUsize,
    /// The client and callback the browser was sent with.
    browser_client: Mutex<Option<(String, String)>>,
    /// The client each token request named.
    token_clients: Mutex<Vec<String>>,
}

const CLIENT_SECRET: &str = "synthetic-client-secret";

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
            confidential: AtomicBool::new(false),
            token_moved: AtomicBool::new(false),
            decoy_issuer: AtomicBool::new(false),
            moved_token_calls: AtomicUsize::new(0),
            secret_refreshes: AtomicUsize::new(0),
            challenge: Mutex::new(None),
            no_registration: AtomicBool::new(false),
            by_address: AtomicBool::new(false),
            gives_paths: AtomicBool::new(false),
            no_server: AtomicBool::new(false),
            names_private_address: AtomicBool::new(false),
            registrations: AtomicUsize::new(0),
            browser_client: Mutex::new(None),
            token_clients: Mutex::new(Vec::new()),
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

    fn browser(&self) -> McpBrowser {
        self.browser_with_issuer(None)
    }

    fn browser_with_issuer(&self, overridden_issuer: Option<String>) -> McpBrowser {
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
            *state.browser_client.lock().unwrap() = Some((
                params.get("client_id").cloned().unwrap(),
                params.get("redirect_uri").cloned().unwrap(),
            ));
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

fn authorization_metadata(state: &FakeOAuth, token_path: &str) -> Value {
    json!({"issuer":state.base,"authorization_endpoint":format!("{}/mcp/oauth/authorize",state.base),"token_endpoint":format!("{}{token_path}",state.base),"registration_endpoint":format!("{}/mcp/oauth/register",state.base),"scopes_supported":["vana:read"],"response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"]})
}

async fn fake_http(
    State(state): State<Arc<FakeOAuth>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match (method, uri.path()) {
        (Method::GET, "/.well-known/oauth-protected-resource/mcp") => {
            let issuer = if state.decoy_issuer.load(Ordering::SeqCst) {
                format!("{}/.well-known/decoy", state.base)
            } else if state.names_private_address.load(Ordering::SeqCst) {
                "http://192.168.70.254:8123".to_string()
            } else {
                state.base.clone()
            };
            json_response(
                json!({"resource":format!("{}/mcp",state.base),"authorization_servers":[issuer],"scopes_supported":["vana:read"]}),
            )
        }
        (Method::GET, "/.well-known/oauth-authorization-server") => {
            let path = if state.token_moved.load(Ordering::SeqCst) {
                "/moved/token"
            } else {
                "/mcp/oauth/token"
            };
            let mut metadata = authorization_metadata(&state, path);
            let by_address = state.by_address.load(Ordering::SeqCst);
            if by_address || state.no_registration.load(Ordering::SeqCst) {
                metadata
                    .as_object_mut()
                    .unwrap()
                    .remove("registration_endpoint");
            }
            if by_address {
                metadata["client_id_metadata_document_supported"] = json!(true);
            }
            if state.gives_paths.load(Ordering::SeqCst) {
                metadata.as_object_mut().unwrap().remove("issuer");
                metadata["authorization_endpoint"] = json!("/mcp/oauth/authorize");
                metadata["token_endpoint"] = json!(path);
            }
            json_response(metadata)
        }
        (Method::GET, "/.well-known/decoy") => {
            json_response(authorization_metadata(&state, "/moved/token"))
        }
        (Method::POST, "/moved/token") => {
            state.moved_token_calls.fetch_add(1, Ordering::SeqCst);
            StatusCode::BAD_REQUEST.into_response()
        }
        (Method::POST, "/mcp/oauth/register") => {
            state.registrations.fetch_add(1, Ordering::SeqCst);
            let input: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(input["client_name"], "Maple");
            let mut client = json!({"client_id":"synthetic-maple-client","redirect_uris":input["redirect_uris"],"token_endpoint_auth_method":"none"});
            if state.confidential.load(Ordering::SeqCst) {
                client["client_secret"] = json!(CLIENT_SECRET);
            }
            json_response(client)
        }
        (Method::POST, "/mcp/oauth/token") => {
            let encoded = format!("http://unused/?{}", String::from_utf8_lossy(&body));
            let url = reqwest::Url::parse(&encoded).unwrap();
            let params: HashMap<_, _> = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            state
                .token_clients
                .lock()
                .unwrap()
                .extend(params.get("client_id").cloned());
            if params.get("grant_type").map(String::as_str) == Some("refresh_token") {
                state.refreshes.fetch_add(1, Ordering::SeqCst);
                if state.confidential.load(Ordering::SeqCst) {
                    let basic = base64::engine::general_purpose::STANDARD
                        .encode(format!("synthetic-maple-client:{CLIENT_SECRET}"));
                    let presented = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        == Some(format!("Basic {basic}").as_str());
                    if !presented {
                        return (
                            StatusCode::UNAUTHORIZED,
                            [("content-type", "application/json")],
                            "{\"error\":\"invalid_client\"}",
                        )
                            .into_response();
                    }
                    state.secret_refreshes.fetch_add(1, Ordering::SeqCst);
                }
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
        (_, "/mcp") if state.no_server.load(Ordering::SeqCst) => {
            StatusCode::NOT_FOUND.into_response()
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

fn sign_in(server: &Server) -> McpSignIn {
    McpSignIn {
        name: "vana".into(),
        description: String::new(),
        url: server.endpoint(),
        scopes: vec!["vana:read".into()],
    }
}

#[tokio::test]
async fn signing_in_again_leaves_an_app_off_if_it_was_turned_off() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    let definitions = harness
        .handle
        .sign_in_mcp_server(sign_in(&server), server.browser())
        .await
        .unwrap();
    let saved = &definitions[0];
    assert!(saved.enabled);
    harness
        .handle
        .save_mcp_definition_edits(vec![McpServerEdit {
            key: saved.key.clone(),
            name: saved.name.clone(),
            description: saved.description.clone(),
            enabled: false,
            timeout_seconds: saved.timeout_seconds,
            target: saved.target.clone(),
            environment: Vec::new(),
            headers: Vec::new(),
        }])
        .await
        .unwrap();
    let definitions = harness
        .handle
        .sign_in_mcp_server(sign_in(&server), server.browser())
        .await
        .unwrap();
    assert!(!definitions[0].enabled);
}

#[tokio::test]
async fn the_browser_lands_on_maples_page_without_the_code_in_it() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let callback =
        reqwest::Url::parse(&format!("http://127.0.0.1:{port}/oauth/callback/x")).unwrap();
    let waiting =
        tokio::spawn(async move { receive_callback(listener, &callback, "expected").await });
    let visit = |state: &'static str| async move {
        let mut browser = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!(
            "GET /oauth/callback/x?code=synthetic-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
        );
        browser.write_all(request.as_bytes()).await.unwrap();
        let mut page = String::new();
        browser.read_to_string(&mut page).await.unwrap();
        page
    };

    let rejected = visit("wrong").await;
    assert!(rejected.starts_with("HTTP/1.1 400"));
    assert!(rejected.contains("<h1 id=\"auth-title\">Sign-in could not finish</h1>"));

    let page = visit("expected").await;
    assert!(page.starts_with("HTTP/1.1 200"));
    assert!(page.contains("Content-Type: text/html"));
    assert!(page.contains("<h1 id=\"auth-title\">Return to Maple Work</h1>"));
    assert!(!page.contains("synthetic-code"));
    assert_eq!(waiting.await.unwrap().unwrap().code, "synthetic-code");
}

#[tokio::test]
async fn native_pkce_callback_persists_refreshes_and_revocation_requires_reconnect() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    let definitions = harness
        .handle
        .sign_in_mcp_server(sign_in(&server), server.browser())
        .await
        .unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "vana");
    assert!(definitions[0].enabled);
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
    let mut expired = decode_saved(&record).unwrap();
    expired.credentials.token_received_at = Some(0);
    context
        .store
        .rotate_mcp_oauth_credentials(
            context.account,
            "vana",
            &target,
            &record,
            saved_secret(&expired).unwrap(),
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
        decode_saved(&refreshed)
            .unwrap()
            .credentials
            .token_received_at
            .unwrap()
            > 0
    );
    // Revoked credentials are cleared, not retried indefinitely or sent to a
    // public listener; reconnect remains an explicit user operation.
    server.state.revoked.store(true, Ordering::SeqCst);
    let mut expired = decode_saved(&refreshed).unwrap();
    expired.credentials.token_received_at = Some(0);
    context
        .store
        .rotate_mcp_oauth_credentials(
            context.account,
            "vana",
            &target,
            &refreshed,
            saved_secret(&expired).unwrap(),
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
            .sign_in_mcp_server(sign_in(&server), server.browser())
            .await
            .is_ok()
    );
}

/// Age the saved sign-in, then refresh it from the store alone, as after a
/// restart.
async fn refresh_after_restart(harness: &crate::agent::tests::Harness, server: &Server) {
    let context = OAuthAccountContext::new(harness.handle.store().unwrap());
    let target = McpTarget::StreamableHttp {
        url: server.endpoint(),
    };
    let record = context
        .store
        .mcp_oauth_credentials(context.account, "vana", &target)
        .unwrap()
        .unwrap();
    let mut expired = decode_saved(&record).unwrap();
    expired.credentials.token_received_at = Some(0);
    context
        .store
        .rotate_mcp_oauth_credentials(
            context.account,
            "vana",
            &target,
            &record,
            saved_secret(&expired).unwrap(),
        )
        .unwrap();
    runtime_client(&context, "vana", &server.endpoint())
        .await
        .unwrap()
        .unwrap()
        .access_token(None)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_server_with_no_registration_knows_maple_by_its_address() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    server.state.by_address.store(true, Ordering::SeqCst);
    let definitions = harness
        .handle
        .sign_in_mcp_server(sign_in(&server), server.browser())
        .await
        .unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(server.state.registrations.load(Ordering::SeqCst), 0);

    // The client is the callback's own origin, which is how a server like this
    // checks the way back without fetching anything from this computer.
    let (client, callback) = server.state.browser_client.lock().unwrap().clone().unwrap();
    let callback = reqwest::Url::parse(&callback).unwrap();
    assert_eq!(
        client,
        format!("{}/", callback.origin().ascii_serialization())
    );
    assert!(client.starts_with("http://127.0.0.1:"));

    refresh_after_restart(&harness, &server).await;
    assert_eq!(server.state.refreshes.load(Ordering::SeqCst), 1);
    let named = server.state.token_clients.lock().unwrap().clone();
    assert_eq!(named, [client.clone(), client]);
}

#[tokio::test]
async fn a_server_that_gives_paths_signs_in_and_refreshes() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    server.state.by_address.store(true, Ordering::SeqCst);
    server.state.gives_paths.store(true, Ordering::SeqCst);
    harness
        .handle
        .sign_in_mcp_server(sign_in(&server), server.browser())
        .await
        .unwrap();
    assert_eq!(server.state.exchanges.load(Ordering::SeqCst), 1);
    refresh_after_restart(&harness, &server).await;
    assert_eq!(server.state.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_server_on_the_local_network_signs_in_at_its_own_address() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    server
        .state
        .names_private_address
        .store(true, Ordering::SeqCst);
    server.state.by_address.store(true, Ordering::SeqCst);
    harness
        .handle
        .sign_in_mcp_server(sign_in(&server), server.browser())
        .await
        .unwrap();
    assert_eq!(server.state.exchanges.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_server_that_says_nothing_about_addresses_still_needs_a_registered_app() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    server.state.no_registration.store(true, Ordering::SeqCst);
    let error = harness
        .handle
        .sign_in_mcp_server(sign_in(&server), server.browser())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), NEEDS_REGISTERED_APP);
    assert!(server.state.browser_client.lock().unwrap().is_none());
}

#[tokio::test]
async fn nobody_is_sent_to_sign_in_for_a_server_that_is_not_there() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    server.state.no_server.store(true, Ordering::SeqCst);
    let error = harness
        .handle
        .sign_in_mcp_server(sign_in(&server), server.browser())
        .await
        .unwrap_err();
    assert_eq!(error, McpSignInError::NoServerHere);
    assert!(server.state.browser_client.lock().unwrap().is_none());

    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let mut unreachable = sign_in(&server);
    unreachable.url = format!("http://{address}/mcp");
    let error = harness
        .handle
        .sign_in_mcp_server(unreachable, server.browser())
        .await
        .unwrap_err();
    assert_eq!(error, McpSignInError::Unreachable);
}

#[tokio::test]
async fn a_client_secret_the_server_issued_outlives_a_restart() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    server.state.confidential.store(true, Ordering::SeqCst);
    harness
        .handle
        .sign_in_mcp_server(sign_in(&server), server.browser())
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
    let mut expired = decode_saved(&record).unwrap();
    assert_eq!(expired.client_secret.as_deref(), Some(CLIENT_SECRET));
    expired.credentials.token_received_at = Some(0);
    context
        .store
        .rotate_mcp_oauth_credentials(
            context.account,
            "vana",
            &target,
            &record,
            saved_secret(&expired).unwrap(),
        )
        .unwrap();
    // A client built from the store alone, as after a restart.
    runtime_client(&context, "vana", &server.endpoint())
        .await
        .unwrap()
        .unwrap()
        .access_token(None)
        .await
        .unwrap();
    assert_eq!(server.state.secret_refreshes.load(Ordering::SeqCst), 1);
    let refreshed = context
        .store
        .mcp_oauth_credentials(context.account, "vana", &target)
        .unwrap()
        .unwrap();
    assert_eq!(
        decode_saved(&refreshed).unwrap().client_secret.as_deref(),
        Some(CLIENT_SECRET)
    );
}

#[test]
fn an_issuers_metadata_is_read_from_its_own_address() {
    let urls = |issuer: &str| -> Vec<String> {
        issuer_metadata_urls(&reqwest::Url::parse(issuer).unwrap())
            .iter()
            .map(ToString::to_string)
            .collect()
    };
    assert_eq!(
        urls("https://as.example/"),
        [
            "https://as.example/.well-known/oauth-authorization-server",
            "https://as.example/.well-known/openid-configuration",
        ]
    );
    assert_eq!(
        urls("https://as.example/tenant"),
        [
            "https://as.example/.well-known/oauth-authorization-server/tenant",
            "https://as.example/.well-known/openid-configuration/tenant",
            "https://as.example/tenant/.well-known/openid-configuration",
        ]
    );
}

#[tokio::test]
async fn a_refresh_token_only_goes_where_the_person_signed_in() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    harness
        .handle
        .sign_in_mcp_server(sign_in(&server), server.browser())
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
    let mut expired = decode_saved(&record).unwrap();
    expired.credentials.token_received_at = Some(0);
    context
        .store
        .rotate_mcp_oauth_credentials(
            context.account,
            "vana",
            &target,
            &record,
            saved_secret(&expired).unwrap(),
        )
        .unwrap();
    server.state.token_moved.store(true, Ordering::SeqCst);
    runtime_client(&context, "vana", &server.endpoint())
        .await
        .unwrap()
        .unwrap()
        .access_token(None)
        .await
        .unwrap();
    assert_eq!(server.state.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(server.state.moved_token_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_document_claiming_to_be_the_issuer_cannot_sign_in() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    server.state.decoy_issuer.store(true, Ordering::SeqCst);
    assert!(
        harness
            .handle
            .sign_in_mcp_server(sign_in(&server), server.browser())
            .await
            .is_err()
    );
    assert_eq!(server.state.exchanges.load(Ordering::SeqCst), 0);
    assert_eq!(server.state.moved_token_calls.load(Ordering::SeqCst), 0);
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
async fn a_sign_in_can_be_stopped_and_tried_again() {
    let harness = crate::agent::tests::Harness::new().await;
    let server = Server::start().await;
    let (stopped, ()) = tokio::join!(
        harness
            .handle
            .sign_in_mcp_server(sign_in(&server), Arc::new(|_| Ok(()))),
        async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            harness.handle.cancel_mcp_sign_in("vana");
        }
    );
    assert_eq!(stopped.unwrap_err().to_string(), SIGN_IN_CANCELLED);
    assert!(
        harness
            .handle
            .list_mcp_definitions()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        harness
            .handle
            .sign_in_mcp_server(sign_in(&server), server.browser())
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
            .sign_in_mcp_server(
                sign_in(&server),
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
            .sign_in_mcp_server(sign_in(&server), server.browser())
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
        let payload = decode_saved(&record).unwrap().credentials;
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
        .sign_in_mcp_server(sign_in(&server), server.browser())
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
    let mut expired = decode_saved(&old_record).unwrap();
    expired.credentials.token_received_at = Some(0);
    context
        .store
        .rotate_mcp_oauth_credentials(
            context.account,
            "vana",
            &target,
            &old_record,
            saved_secret(&expired).unwrap(),
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
