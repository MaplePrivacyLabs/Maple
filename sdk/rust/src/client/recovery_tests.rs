use super::*;
use crate::transport_v2::client::{SessionResponder, TestV2ServerState};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, Respond, ResponseTemplate,
};

fn recovery_response(code: &'static str) -> ResponseTemplate {
    ResponseTemplate::new(400)
        .insert_header("x-opensecret-error-contract", "1")
        .insert_header("x-opensecret-error-code", code)
}

async fn mount_sessions(
    server: &MockServer,
    state: &TestV2ServerState,
    count: u64,
    delay_replacement: bool,
) {
    let responder = SessionResponder {
        server_secret: [0x91; 32],
        state: Some(state.clone()),
        delay: None,
    };
    let calls = AtomicUsize::new(0);
    Mock::given(method("POST"))
        .and(path("/v2/session"))
        .respond_with(move |request: &Request| {
            let response = responder.respond(request);
            if calls.fetch_add(1, Ordering::SeqCst) > 0 && delay_replacement {
                response.set_delay(Duration::from_millis(250))
            } else {
                response
            }
        })
        .expect(count)
        .mount(server)
        .await;
}

async fn mount_recovery_then_success(
    server: &MockServer,
    state: &TestV2ServerState,
    code: &'static str,
    delay_failure: bool,
    count: u64,
) {
    for _ in 0..count {
        state.queue_json_response(200, serde_json::json!({"object": "list", "data": []}));
    }
    let responder = state.request_responder();
    let calls = AtomicUsize::new(0);
    Mock::given(method("POST"))
        .and(path("/v2/request"))
        .respond_with(move |request: &Request| {
            let response = responder.respond(request);
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let failure = recovery_response(code);
                if delay_failure {
                    failure.set_delay(Duration::from_millis(250))
                } else {
                    failure
                }
            } else {
                response
            }
        })
        .expect(count)
        .mount(server)
        .await;
}

async fn wait_for_requests(server: &MockServer, target: &str, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path() == target)
                .count()
                >= count
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("expected request was not observed");
}

#[tokio::test]
async fn recovery_reseals_identical_binary_request_with_fresh_session_and_request_ids() {
    for code in ["session_not_found", "request_decryption_failed"] {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        mount_sessions(&server, &state, 2, false).await;
        mount_recovery_then_success(&server, &state, code, false, 2).await;
        let client =
            OpenSecretClient::new_with_api_key(server.uri(), "fixture-api-key".to_string())
                .unwrap();
        let bytes = Bytes::from_static(b"\0\xff\r\n--fixture-boundary\r\nbinary\0payload");
        let mut request = HttpRequest::post("/v1/audio/transcriptions?language=en")
            .header(
                header::CONTENT_TYPE,
                "multipart/form-data; boundary=fixture-boundary",
            )
            .header("x-client-metadata", "first")
            .body(bytes.clone())
            .unwrap();
        request
            .headers_mut()
            .append("x-client-metadata", "second".parse().unwrap());
        let response = client.send_inference_request(request).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        collect_response_body(response.into_body()).await.unwrap();
        let plaintexts = state.captured_request_plaintexts();
        assert_eq!(plaintexts.len(), 2);
        assert_eq!(
            plaintexts[0], plaintexts[1],
            "all encoded metadata and raw body must be identical"
        );
        assert_eq!(state.captured_request_bodies(), vec![bytes.clone(), bytes]);
        let logical = state.captured_requests();
        assert_eq!(logical[0]["credential"]["kind"], "api_key");
        assert_eq!(logical[0]["credential"]["value"], "fixture-api-key");
        assert!(logical[0]["cache_namespace_root"].is_string());
        let ids = state.captured_request_ids();
        assert_ne!(ids[0], ids[1]);
        let requests = server.received_requests().await.unwrap();
        let sends: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path() == "/v2/request")
            .collect();
        assert_ne!(
            sends[0].headers["x-session-id"],
            sends[1].headers["x-session-id"]
        );
        for request in requests {
            assert!(!request.headers.contains_key(header::AUTHORIZATION));
            assert!(!request.headers.contains_key(header::COOKIE));
        }
    }
}

#[tokio::test]
async fn recovery_preserves_absent_body_and_serializes_typed_input_only_once() {
    #[derive(Clone)]
    struct Counted(Arc<AtomicUsize>);
    impl Serialize for Counted {
        fn serialize<S: serde::Serializer>(
            &self,
            serializer: S,
        ) -> std::result::Result<S::Ok, S::Error> {
            self.0.fetch_add(1, Ordering::SeqCst);
            serializer.serialize_str("fixture")
        }
    }
    for body_present in [false, true] {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        mount_sessions(&server, &state, 2, false).await;
        mount_recovery_then_success(&server, &state, "session_not_found", false, 2).await;
        let client = OpenSecretClient::new(server.uri()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let _: serde_json::Value = client
            .json_request(
                "/fixture",
                "POST",
                body_present.then(|| Counted(calls.clone())),
                ResolvedAuth {
                    token: None,
                    source: CredentialSource::Anonymous,
                },
            )
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(body_present));
        let logical = state.captured_requests();
        assert_eq!(logical[0]["body_present"], body_present);
        let plaintexts = state.captured_request_plaintexts();
        assert_eq!(plaintexts[0], plaintexts[1]);
    }
}

#[tokio::test]
async fn recovery_reasons_share_one_budget_and_second_hint_surfaces() {
    for codes in [
        ["session_not_found", "request_decryption_failed"],
        ["request_decryption_failed", "session_not_found"],
    ] {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        mount_sessions(&server, &state, 2, false).await;
        let calls = AtomicUsize::new(0);
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(move |_: &Request| {
                recovery_response(codes[calls.fetch_add(1, Ordering::SeqCst).min(1)])
            })
            .expect(2)
            .mount(&server)
            .await;
        let client =
            OpenSecretClient::new_with_api_key(server.uri(), "fixture-key".to_string()).unwrap();
        assert!(matches!(
            client.get_models().await,
            Err(Error::InvalidResponse(_))
        ));
        assert!(client.get_session_id().unwrap().is_none());
    }
}

#[tokio::test]
async fn recovery_ignores_unmarked_wrong_status_and_duplicate_outer_hints() {
    let mut failures = vec![
        ResponseTemplate::new(400),
        ResponseTemplate::new(503),
        ResponseTemplate::new(400).set_body_json(serde_json::json!({"error":"session_not_found"})),
        ResponseTemplate::new(503)
            .insert_header("x-opensecret-error-contract", "1")
            .insert_header("x-opensecret-error-code", "session_not_found"),
        ResponseTemplate::new(400).insert_header("x-opensecret-error-code", "session_not_found"),
        ResponseTemplate::new(400)
            .insert_header("x-opensecret-error-contract", "2")
            .insert_header("x-opensecret-error-code", "session_not_found"),
        recovery_response("request_rejected"),
        recovery_response("session_not_found, session_not_found"),
    ];
    failures.push(
        recovery_response("session_not_found").append_header("x-opensecret-error-contract", "1"),
    );
    failures.push(
        recovery_response("session_not_found")
            .append_header("x-opensecret-error-code", "session_not_found"),
    );
    for failure in failures {
        let server = MockServer::start().await;
        mount_sessions(&server, &TestV2ServerState::new(), 1, false).await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(failure)
            .expect(1)
            .mount(&server)
            .await;
        let client =
            OpenSecretClient::new_with_api_key(server.uri(), "fixture-key".to_string()).unwrap();
        assert!(matches!(
            client.get_models().await,
            Err(Error::InvalidResponse(_))
        ));
    }
}

#[tokio::test]
async fn recovery_never_classifies_handshake_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/session"))
        .respond_with(recovery_response("session_not_found"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/request"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let client = OpenSecretClient::new(server.uri()).unwrap();
    assert!(client.test_connection().await.is_err());
}

#[tokio::test]
async fn recovery_fences_credentials_changed_during_replacement_handshake() {
    for authority in ["bearer", "api_key", "resumption", "anonymous"] {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        mount_sessions(&server, &state, 2, true).await;
        mount_recovery_then_success(&server, &state, "request_decryption_failed", false, 1).await;
        let client = Arc::new(OpenSecretClient::new(server.uri()).unwrap());
        let auth = match authority {
            "api_key" => {
                client.set_api_key("old-fixture-key".to_string()).unwrap();
                client.resolve_auth(AuthHeaderMode::ApiKeyOrJwt).unwrap()
            }
            "anonymous" => ResolvedAuth {
                token: None,
                source: CredentialSource::Anonymous,
            },
            _ => {
                client
                    .set_tokens(
                        "old-fixture-access".to_string(),
                        Some("old-fixture-refresh".to_string()),
                    )
                    .unwrap();
                if authority == "bearer" {
                    client.resolve_auth(AuthHeaderMode::Jwt).unwrap()
                } else {
                    ResolvedAuth {
                        token: Some("old-fixture-refresh".to_string()),
                        source: CredentialSource::StoredRefreshToken {
                            generation: client
                                .session_manager
                                .get_credential_snapshot()
                                .unwrap()
                                .token_generation,
                        },
                    }
                }
            }
        };
        let pending = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .send_logical_request(
                        http::Method::POST,
                        "/fixture".to_string(),
                        Vec::new(),
                        Some(Bytes::from_static(b"fixture-body")),
                        auth,
                        true,
                    )
                    .await
            })
        };
        wait_for_requests(&server, "/v2/session", 2).await;
        match authority {
            "api_key" => client.set_api_key("new-fixture-key".to_string()).unwrap(),
            "resumption" => client.session_manager.clear_tokens().unwrap(),
            _ => client
                .set_tokens(
                    "new-fixture-access".to_string(),
                    Some("new-fixture-refresh".to_string()),
                )
                .unwrap(),
        }
        assert!(
            matches!(pending.await.unwrap(), Err(Error::Authentication(message)) if message == "Credential changed before Transport V2 request admission")
        );
        assert_eq!(state.captured_requests().len(), 1);
    }
}

#[tokio::test]
async fn recovery_keeps_explicit_key_when_managed_default_changes() {
    let server = MockServer::start().await;
    let state = TestV2ServerState::new();
    mount_sessions(&server, &state, 2, true).await;
    mount_recovery_then_success(&server, &state, "session_not_found", false, 2).await;
    let client = Arc::new(
        OpenSecretClient::new_with_api_key(server.uri(), "old-default".to_string()).unwrap(),
    );
    let pending = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .send_inference_request_with_api_key(
                    HttpRequest::get("/v1/models").body(Bytes::new()).unwrap(),
                    "fixture-explicit-key".to_string(),
                )
                .await
        })
    };
    wait_for_requests(&server, "/v2/session", 2).await;
    client.set_api_key("new-default".to_string()).unwrap();
    collect_response_body(pending.await.unwrap().unwrap().into_body())
        .await
        .unwrap();
    assert_eq!(
        client.session_manager.get_api_key().unwrap().as_deref(),
        Some("new-default")
    );
    for request in state.captured_requests() {
        assert_eq!(request["credential"]["value"], "fixture-explicit-key");
    }
    let plaintexts = state.captured_request_plaintexts();
    assert_eq!(plaintexts[0], plaintexts[1]);
}

#[tokio::test]
async fn recovery_reuses_concurrent_replacement_without_retiring_it() {
    let server = MockServer::start().await;
    let state = TestV2ServerState::new();
    mount_sessions(&server, &state, 2, false).await;
    mount_recovery_then_success(&server, &state, "session_not_found", true, 2).await;
    let client = Arc::new(
        OpenSecretClient::new_with_api_key(server.uri(), "fixture-key".to_string()).unwrap(),
    );
    let pending = {
        let client = client.clone();
        tokio::spawn(async move { client.get_models().await })
    };
    wait_for_requests(&server, "/v2/request", 1).await;
    client.perform_attestation_handshake().await.unwrap();
    let replacement = client.current_transport_session().unwrap().unwrap();
    assert!(pending.await.unwrap().is_ok());
    assert!(Arc::ptr_eq(
        &replacement,
        &client.current_transport_session().unwrap().unwrap()
    ));
}

#[tokio::test]
async fn recovery_excludes_session_bound_oauth_targets_with_queries() {
    for target in [
        "/auth/github/callback",
        "/auth/google/callback",
        "/auth/apple/callback",
        "/auth/native-handoff/redeem",
    ] {
        for suffix in ["", "?fixture=1"] {
            let server = MockServer::start().await;
            mount_sessions(&server, &TestV2ServerState::new(), 1, false).await;
            Mock::given(method("POST"))
                .and(path("/v2/request"))
                .respond_with(recovery_response("session_not_found"))
                .expect(1)
                .mount(&server)
                .await;
            let client = OpenSecretClient::new(server.uri()).unwrap();
            let result: Result<serde_json::Value> = client
                .json_request(
                    &format!("{target}{suffix}"),
                    "POST",
                    Some(serde_json::json!({"fixture":true})),
                    ResolvedAuth {
                        token: None,
                        source: CredentialSource::Anonymous,
                    },
                )
                .await;
            assert!(matches!(result, Err(Error::InvalidResponse(_))));
        }
    }
}

#[tokio::test]
async fn recovery_hint_does_not_replay_prepared_native_handoff() {
    for code in ["session_not_found", "request_decryption_failed"] {
        let server = MockServer::start().await;
        mount_sessions(&server, &TestV2ServerState::new(), 1, false).await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(recovery_response(code))
            .expect(1)
            .mount(&server)
            .await;
        let client = OpenSecretClient::new(server.uri()).unwrap();
        let prepared = client.prepare_native_oauth_handoff().await.unwrap();
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({"sid":prepared.session_id(), "rid":prepared.request_id()})
                .to_string(),
        );
        let grant = NativeOAuthHandoffGrant::new(format!("e30.{payload}.c2ln")).unwrap();
        assert!(matches!(
            client.redeem_native_oauth_handoff(prepared, grant).await,
            Err(Error::InvalidResponse(_))
        ));
        assert!(client.get_session_id().unwrap().is_none());
    }
}

#[tokio::test]
async fn recovery_does_not_replay_response_aead_or_framing_failure() {
    fn corrupt_authentication(mut wire: Vec<u8>) -> Vec<u8> {
        wire[4] ^= 1;
        wire
    }
    fn corrupt_frame(mut wire: Vec<u8>) -> Vec<u8> {
        wire[..4].fill(0xff);
        wire
    }
    for transform in [corrupt_authentication, corrupt_frame] {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        state.queue_json_response(200, serde_json::json!({"object":"list", "data":[]}));
        mount_sessions(&server, &state, 1, false).await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(state.request_responder_with_wire_transform(transform))
            .expect(1)
            .mount(&server)
            .await;
        let client =
            OpenSecretClient::new_with_api_key(server.uri(), "fixture-key".to_string()).unwrap();
        assert!(
            matches!(client.get_models().await, Err(Error::InvalidResponse(message)) if message == "Transport V2 response authentication or framing failed")
        );
        assert_eq!(state.captured_requests().len(), 1);
    }
}

#[tokio::test]
async fn recovery_does_not_replay_after_response_body_progress_and_missing_terminal() {
    fn remove_terminal(mut wire: Vec<u8>) -> Vec<u8> {
        // Four-byte frame length, one-byte End tag, sixteen-byte AEAD tag.
        wire.truncate(wire.len() - 21);
        wire
    }
    let server = MockServer::start().await;
    let state = TestV2ServerState::new();
    state.queue_json_response(200, serde_json::json!({"fixture":"body-progress"}));
    mount_sessions(&server, &state, 1, false).await;
    Mock::given(method("POST"))
        .and(path("/v2/request"))
        .respond_with(state.request_responder_with_wire_transform(remove_terminal))
        .expect(1)
        .mount(&server)
        .await;
    let client =
        OpenSecretClient::new_with_api_key(server.uri(), "fixture-key".to_string()).unwrap();
    let response = client
        .send_inference_request(HttpRequest::get("/v1/models").body(Bytes::new()).unwrap())
        .await
        .unwrap();
    let mut body = response.into_body();
    let chunk = body.next().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&chunk).unwrap()["fixture"],
        "body-progress"
    );
    assert!(matches!(
        body.next().await.unwrap(),
        Err(Error::InvalidResponse(_))
    ));
    assert!(body.next().await.is_none());
    assert_eq!(state.captured_requests().len(), 1);
}

#[tokio::test]
async fn recovery_stops_when_replacement_handshake_fails() {
    let server = MockServer::start().await;
    let state = TestV2ServerState::new();
    let responder = SessionResponder {
        server_secret: [0x92; 32],
        state: Some(state.clone()),
        delay: None,
    };
    let calls = AtomicUsize::new(0);
    Mock::given(method("POST"))
        .and(path("/v2/session"))
        .respond_with(move |request: &Request| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                responder.respond(request)
            } else {
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"version":2, "attestation_document":"invalid"}),
                )
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/request"))
        .respond_with(recovery_response("request_decryption_failed"))
        .expect(1)
        .mount(&server)
        .await;
    let client =
        OpenSecretClient::new_with_api_key(server.uri(), "fixture-key".to_string()).unwrap();
    assert!(client.get_models().await.is_err());
    assert!(client.get_session_id().unwrap().is_none());
}

#[tokio::test]
async fn recovery_does_not_replay_authenticated_logical_errors() {
    for status in [400, 401, 503] {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        state.queue_json_response(status, serde_json::json!({"error":"session_not_found"}));
        mount_sessions(&server, &state, 1, false).await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(state.request_responder())
            .expect(1)
            .mount(&server)
            .await;
        let client =
            OpenSecretClient::new_with_api_key(server.uri(), "fixture-key".to_string()).unwrap();
        assert!(
            matches!(client.get_models().await, Err(Error::Api { status: actual, .. }) if actual == status)
        );
        assert_eq!(state.captured_requests().len(), 1);
    }
}

#[tokio::test]
async fn recovery_does_not_follow_request_redirects() {
    let server = MockServer::start().await;
    mount_sessions(&server, &TestV2ServerState::new(), 1, false).await;
    Mock::given(method("POST"))
        .and(path("/v2/request"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/redirect-target", server.uri()))
                .insert_header("x-opensecret-error-contract", "1")
                .insert_header("x-opensecret-error-code", "session_not_found"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/redirect-target"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let client =
        OpenSecretClient::new_with_api_key(server.uri(), "fixture-key".to_string()).unwrap();
    assert!(matches!(
        client.get_models().await,
        Err(Error::InvalidResponse(_))
    ));
}
