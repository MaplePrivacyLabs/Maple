// Included in gateway::tests to reuse the real encrypted request/response harness.
// These checks deliberately stop before an OAuth provider exchange: authorization
// URL construction and rejected selections need no provider credentials or egress.

async fn oauth_selection_v1_request(
    application: &Router<()>,
    session_id: uuid::Uuid,
    session_key: &[u8; 32],
    target: &str,
    payload: &serde_json::Value,
) -> (u16, serde_json::Value) {
    use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};

    let cipher = ChaCha20Poly1305::new_from_slice(session_key).unwrap();
    let nonce = crate::encrypt::generate_random::<12>();
    let mut encrypted = nonce.to_vec();
    encrypted.extend_from_slice(
        &cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                serde_json::to_vec(payload).unwrap().as_slice(),
            )
            .unwrap(),
    );
    let request = Request::builder()
        .method(Method::POST)
        .uri(target)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-session-id", session_id.to_string())
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "encrypted": STANDARD.encode(encrypted) }))
                .unwrap(),
        ))
        .unwrap();
    let response = application.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    if status != 200 {
        // V1 retains its existing plaintext error projection.
        return (status, body);
    }
    let encrypted = STANDARD
        .decode(body["encrypted"].as_str().unwrap())
        .unwrap();
    let (nonce, ciphertext) = encrypted.split_at(12);
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .unwrap();
    (status, serde_json::from_slice(&plaintext).unwrap())
}

async fn oauth_selection_v2_request(
    gateway: &Router<()>,
    session: &TestSession,
    request_number: u8,
    target: &str,
    payload: &serde_json::Value,
) -> (u16, serde_json::Value) {
    let request_id = RequestId::from_bytes([request_number; 16]);
    let envelope = RequestEnvelope::new(
        request_id,
        None,
        None,
        "POST".to_string(),
        target.to_string(),
        vec![
            LogicalHeader::new("content-type".to_string(), "application/json".to_string()).unwrap(),
        ],
        Some(serde_json::to_vec(payload).unwrap()),
    )
    .unwrap();
    let ciphertext = session
        .client
        .encrypt_request(request_id, &envelope.encode().unwrap())
        .unwrap();
    let response = gateway
        .clone()
        .oneshot(outer_request(
            session.server.id(),
            &session.routing_key,
            ciphertext,
        ))
        .await
        .unwrap();
    let records = decrypt_records(&session.client, request_id, response).await;
    let Some(ResponseRecord::Start(start)) = records.first() else {
        panic!("OAuth response must begin with authenticated status");
    };
    assert!(matches!(records.last(), Some(ResponseRecord::End)));
    let mut body = Vec::new();
    for record in &records[1..records.len() - 1] {
        let ResponseRecord::Chunk(chunk) = record else {
            panic!("OAuth response must contain only body chunks before its terminal record");
        };
        body.extend_from_slice(chunk);
    }
    (start.status(), serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
async fn db_oauth_callback_selection_v1_v2() {
    use crate::{
        db::setup_db,
        models::{
            org_project_secrets::NewOrgProjectSecret,
            org_projects::NewOrgProject,
            orgs::NewOrg,
            project_settings::{AppleOAuthSettings, OAuthProviderSettings, OAuthSettings},
        },
        web::{
            attestation_routes::SessionState,
            oauth_routes,
            platform::common::{
                PROJECT_APPLE_OAUTH_SECRET, PROJECT_GITHUB_OAUTH_SECRET,
                PROJECT_GOOGLE_OAUTH_SECRET,
            },
        },
        AppMode, AppStateBuilder,
    };
    use openssl::{
        ec::{EcGroup, EcKey},
        nid::Nid,
        pkey::PKey,
    };

    let database_url = std::env::var("AEAD_TAMPER_TEST_DATABASE_URL")
        .expect("this test requires the disposable database harness");
    let database_host = url::Url::parse(&database_url).expect("database URL must parse");
    assert!(matches!(
        database_host.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]")
    ));
    let enclave_key = [42u8; 32];
    let app_state = Arc::new(
        AppStateBuilder::default()
            .app_mode(AppMode::Local)
            .db(setup_db(database_url))
            .enclave_key(enclave_key.to_vec())
            .aws_credential_manager(Arc::new(tokio::sync::RwLock::new(None)))
            .openai_api_base("http://127.0.0.1:9".to_string())
            .tinfoil_api_base("http://127.0.0.1:9".to_string())
            .jwt_secret([24u8; 32].to_vec())
            .build()
            .await
            .unwrap(),
    );
    let marker = uuid::Uuid::new_v4();
    let org = app_state
        .db
        .create_org(NewOrg::new(format!("oauth-selection-{marker}")))
        .unwrap();
    let project = app_state
        .db
        .create_org_project(NewOrgProject::new(org.id, "callback-selection".to_string()))
        .unwrap();
    let default_url = |provider: &str| format!("https://app.example.test/auth/{provider}/callback");
    let allowed_url = |provider: &str| {
        format!("https://auth.example.test/auth/{provider}/callback?channel=hosted")
    };
    let provider_settings = |provider: &str| OAuthProviderSettings {
        client_id: format!("oauth-selection-{provider}"),
        redirect_url: default_url(provider),
        additional_redirect_urls: Some(vec![allowed_url(provider)]),
    };
    app_state
        .db
        .update_project_oauth_settings(
            project.id,
            OAuthSettings {
                github_oauth_enabled: true,
                google_oauth_enabled: true,
                apple_oauth_enabled: true,
                github_oauth_settings: Some(provider_settings("github")),
                google_oauth_settings: Some(provider_settings("google")),
                apple_oauth_settings: Some(AppleOAuthSettings {
                    client_id: "oauth-selection.services".to_string(),
                    redirect_url: default_url("apple"),
                    additional_redirect_urls: Some(vec![allowed_url("apple")]),
                    team_id: Some("TEAM123456".to_string()),
                    key_id: Some("KEY1234567".to_string()),
                }),
            },
        )
        .unwrap();

    // Apple builds a JWT during initiation. Generate a throwaway signing key,
    // matching apple_signin's existing unit fixture, without any Apple account.
    let curve = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let apple_key = PKey::from_ec_key(EcKey::generate(&curve).unwrap()).unwrap();
    let apple_secret = STANDARD.encode(apple_key.private_key_to_pem_pkcs8().unwrap());
    let secret_key = secp256k1::SecretKey::from_slice(&enclave_key).unwrap();
    for (name, value) in [
        (PROJECT_GITHUB_OAUTH_SECRET, "github-fixture-secret"),
        (PROJECT_GOOGLE_OAUTH_SECRET, "google-fixture-secret"),
        (PROJECT_APPLE_OAUTH_SECRET, apple_secret.as_str()),
    ] {
        let ciphertext = crate::encrypt::encrypt_with_key(&secret_key, value.as_bytes()).await;
        app_state
            .db
            .create_org_project_secret(NewOrgProjectSecret::new(
                project.id,
                name.to_string(),
                ciphertext,
            ))
            .unwrap();
    }

    let application = oauth_routes(Arc::clone(&app_state));
    let session_v1_id = uuid::Uuid::new_v4();
    let session_v1_key = crate::encrypt::generate_random::<32>();
    app_state
        .store_session_state(session_v1_id, SessionState::new(session_v1_key))
        .await
        .unwrap();
    let session_v2 = test_session(0xD1);
    let sessions = Arc::new(SessionStore::new(NonZeroUsize::new(2).unwrap()));
    sessions.insert(Arc::clone(&session_v2.server)).unwrap();
    let gateway = request_router(application.clone(), sessions);
    let mut request_number = 0;

    for provider in ["github", "google", "apple"] {
        let target = format!("/auth/{provider}");
        let default = default_url(provider);
        let allowed = allowed_url(provider);
        let other_provider = if provider == "github" {
            "google"
        } else {
            "github"
        };
        for (selection, expected) in [
            (None, Some(default.as_str())),
            (Some(serde_json::Value::Null), Some(default.as_str())),
            (Some(serde_json::json!(default)), Some(default.as_str())),
            (Some(serde_json::json!(allowed)), Some(allowed.as_str())),
            (
                Some(serde_json::json!("https://unlisted.example.test/callback")),
                None,
            ),
            (Some(serde_json::json!(allowed_url(other_provider))), None),
        ] {
            let mut payload =
                serde_json::json!({ "client_id": project.client_id, "invite_code": "" });
            if let Some(selection) = selection {
                payload["redirect_url"] = selection;
            }
            let v1 = oauth_selection_v1_request(
                &application,
                session_v1_id,
                &session_v1_key,
                &target,
                &payload,
            )
            .await;
            request_number += 1;
            let v2 = oauth_selection_v2_request(
                &gateway,
                &session_v2,
                request_number,
                &target,
                &payload,
            )
            .await;
            for (transport, (status, response)) in [(1, v1), (2, v2)] {
                let Some(expected) = expected else {
                    assert_eq!(
                        status, 400,
                        "{provider} V{transport} must reject unlisted callbacks"
                    );
                    assert!(response.get("auth_url").is_none());
                    assert!(response.get("state").is_none());
                    continue;
                };
                assert_eq!(
                    status, 200,
                    "{provider} V{transport} initiation should succeed"
                );
                let auth_url = url::Url::parse(response["auth_url"].as_str().unwrap()).unwrap();
                let query: std::collections::HashMap<_, _> = auth_url.query_pairs().collect();
                assert_eq!(
                    query.get("redirect_uri").map(|value| value.as_ref()),
                    Some(expected)
                );
                let opaque_state = response["state"].as_str().unwrap();
                assert_eq!(
                    query.get("state").map(|value| value.as_ref()),
                    Some(opaque_state)
                );
                let state: serde_json::Value = serde_json::from_slice(
                    &base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(opaque_state)
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(state["client_id"], project.client_id.to_string());
                assert_eq!(state["redirect_url"], expected);
                assert!(!state["csrf_token"].as_str().unwrap().is_empty());
                if provider != "apple" {
                    assert_eq!(query.contains_key("code_challenge"), transport == 2);
                }
            }
        }
    }
    app_state.db.delete_org_project(&project).unwrap();
    app_state.db.delete_org(&org).unwrap();
}
