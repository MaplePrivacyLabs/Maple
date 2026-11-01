use super::*;
use crate::transport_v2::client::{SessionResponder, TestV2ServerState};
use crate::ClientApp;
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer,
};

async fn server_with_responses(responses: Vec<Value>) -> (MockServer, TestV2ServerState) {
    let server = MockServer::start().await;
    let state = TestV2ServerState::new();
    let count = responses.len() as u64;
    for response in responses {
        state.queue_json_response(200, response);
    }
    Mock::given(method("POST"))
        .and(path("/v2/session"))
        .respond_with(SessionResponder {
            server_secret: [0x94; 32],
            state: Some(state.clone()),
            delay: None,
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/request"))
        .respond_with(state.request_responder())
        .expect(count)
        .mount(&server)
        .await;
    (server, state)
}

fn app_headers(request: &Value) -> Vec<&str> {
    request["headers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|header| {
            header["name"]
                .as_str()
                .unwrap()
                .eq_ignore_ascii_case("x-maple-client-app")
        })
        .map(|header| header["value"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn unconfigured_client_app_does_not_infer_an_application_from_api_credentials() {
    let (server, state) = server_with_responses(vec![json!({"object":"list","data":[]})]).await;
    let client = OpenSecretClient::new_with_api_key(server.uri(), "fixture-key".into()).unwrap();
    assert_eq!(client.client_app(), None);
    client.get_models().await.unwrap();
    let logical = state.captured_requests();
    assert!(app_headers(&logical[0]).is_empty());
    assert_eq!(logical[0]["credential"]["kind"], "api_key");
}

#[tokio::test]
async fn unconfigured_client_preserves_explicit_logical_application_context() {
    let (server, state) = server_with_responses(vec![json!({"ok":true})]).await;
    let client = OpenSecretClient::new(server.uri()).unwrap();
    let response = client
        .send_inference_request(
            HttpRequest::get("/v1/models")
                .header("x-maple-client-app", "chat")
                .body(Bytes::new())
                .unwrap(),
        )
        .await
        .unwrap();
    collect_response_body(response.into_body()).await.unwrap();
    assert_eq!(client.client_app(), None);
    assert_eq!(app_headers(&state.captured_requests()[0]), vec!["chat"]);
}

#[tokio::test]
async fn configured_client_app_overrides_conflicting_logical_headers_without_changing_credentials()
{
    for app in [ClientApp::Chat, ClientApp::Proxy, ClientApp::Work] {
        let (server, state) = server_with_responses(vec![json!({"ok":true})]).await;
        let client = OpenSecretClient::new(server.uri())
            .unwrap()
            .with_client_app(app);
        assert_eq!(client.client_app(), Some(app));
        let mut request = HttpRequest::post("/v1/chat/completions")
            .header("X-Maple-Client-App", "proxy")
            .header("x-client-metadata", "preserved")
            .body(Bytes::from_static(b"original request body"))
            .unwrap();
        request
            .headers_mut()
            .append("x-maple-client-app", "work".parse().unwrap());
        let response = client
            .send_inference_request_with_api_key(request, "fixture-explicit-key".into())
            .await
            .unwrap();
        collect_response_body(response.into_body()).await.unwrap();
        let logical = state.captured_requests();
        assert_eq!(app_headers(&logical[0]), vec![app.as_str()]);
        assert_eq!(logical[0]["credential"]["kind"], "api_key");
        assert_eq!(logical[0]["credential"]["value"], "fixture-explicit-key");
        assert!(logical[0]["headers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| { h["name"] == "x-client-metadata" && h["value"] == "preserved" }));
        assert_eq!(
            state.captured_request_bodies(),
            vec![Bytes::from_static(b"original request body")]
        );
        for request in server.received_requests().await.unwrap() {
            assert!(!request.headers.contains_key("x-maple-client-app"));
            assert!(!request.headers.contains_key(header::AUTHORIZATION));
        }
    }
}

#[tokio::test]
async fn client_app_reaches_account_api_streaming_and_both_system_one_auth_paths() {
    let system_one_response = json!({
        "id":"fixture", "model":"fixture", "answers":{},
        "usage":{"input_tokens":0,"output_tokens":0,"cached_tokens":0,"requests":0}
    });
    let (server, state) = server_with_responses(vec![
        json!({"user": {
            "id":Uuid::nil(), "name":null, "email":null, "email_verified":false,
            "login_method":"email", "created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z"
        }}),
        system_one_response.clone(),
        system_one_response,
        json!({}),
    ])
    .await;
    let client = OpenSecretClient::new(server.uri())
        .unwrap()
        .with_client_app(ClientApp::Work);
    client.set_tokens("fixture-access".into(), None).unwrap();
    client.get_user().await.unwrap();
    client
        .system_one(SystemOneRequest::new(json!({})))
        .await
        .unwrap();
    client
        .system_one_with_api_key(SystemOneRequest::new(json!({})), "fixture-key".into())
        .await
        .unwrap();
    // Header delivery is established when the public streaming method returns.
    let stream = client
        .create_chat_completion_stream(
            serde_json::from_value(json!({"model":"fixture","messages":[]})).unwrap(),
        )
        .await
        .unwrap();
    drop(stream);
    let logical = state.captured_requests();
    assert_eq!(logical.len(), 4);
    assert_eq!(
        logical
            .iter()
            .map(|request| request["target"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "/protected/user",
            "/v1/systemone",
            "/v1/systemone",
            "/v1/chat/completions"
        ]
    );
    assert_eq!(logical[0]["credential"]["kind"], "bearer");
    assert_eq!(logical[1]["credential"]["kind"], "bearer");
    assert_eq!(logical[2]["credential"]["kind"], "api_key");
    assert_eq!(logical[3]["credential"]["kind"], "bearer");
    for request in logical {
        assert_eq!(app_headers(&request), vec!["work"]);
    }
    for request in server.received_requests().await.unwrap() {
        assert!(!request.headers.contains_key("x-maple-client-app"));
    }
}

#[test]
fn client_app_wire_values_are_canonical_and_reject_unknown_values() {
    for app in [ClientApp::Chat, ClientApp::Proxy, ClientApp::Work] {
        assert_eq!(serde_json::to_value(app).unwrap(), app.as_str());
        assert_eq!(
            serde_json::from_value::<ClientApp>(json!(app.as_str())).unwrap(),
            app
        );
    }
    for invalid in ["agent", "Chat", "", "unknown"] {
        assert!(serde_json::from_value::<ClientApp>(json!(invalid)).is_err());
    }
}
