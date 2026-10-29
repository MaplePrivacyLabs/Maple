// Included in gateway::tests to reuse the real encrypted request/response harness.
// The recovery route tests hand the inner router a V2 session extension; these
// drive the production application composition through the real V2 gateway and
// the real V1 encryption middleware against a disposable database.

/// Sends one request through the V1 encryption middleware. Bodyless requests
/// still carry the session header so the middleware sees a V1 caller.
async fn recovery_v1_request(
    application: &Router<()>,
    session_id: uuid::Uuid,
    session_key: &[u8; 32],
    method: Method,
    target: &str,
    payload: Option<&serde_json::Value>,
    bearer: &str,
) -> (u16, serde_json::Value) {
    use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};

    let cipher = ChaCha20Poly1305::new_from_slice(session_key).unwrap();
    let mut builder = Request::builder()
        .method(method)
        .uri(target)
        .header("x-session-id", session_id.to_string())
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    let body = match payload {
        Some(payload) => {
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
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(
                serde_json::to_vec(&serde_json::json!({ "encrypted": STANDARD.encode(encrypted) }))
                    .unwrap(),
            )
        }
        None => Body::empty(),
    };
    let response = application
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

/// Sends one request through the V2 gateway and returns the authenticated
/// inner status and JSON body.
async fn recovery_v2_request(
    gateway: &Router<()>,
    session: &TestSession,
    request_number: &mut u8,
    method: &str,
    target: &str,
    payload: Option<&serde_json::Value>,
    bearer: Option<&str>,
) -> (u16, serde_json::Value) {
    *request_number += 1;
    let request_id = RequestId::from_bytes([*request_number; 16]);
    let headers = if payload.is_some() {
        vec![
            LogicalHeader::new("content-type".to_string(), "application/json".to_string()).unwrap(),
        ]
    } else {
        Vec::new()
    };
    let envelope = RequestEnvelope::new(
        request_id,
        bearer.map(|token| Credential::new(CredentialKind::Bearer, token.to_string()).unwrap()),
        None,
        method.to_string(),
        target.to_string(),
        headers,
        payload.map(|payload| serde_json::to_vec(payload).unwrap()),
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
        panic!("recovery response must begin with authenticated status");
    };
    assert!(matches!(records.last(), Some(ResponseRecord::End)));
    let mut body = Vec::new();
    for record in &records[1..records.len() - 1] {
        let ResponseRecord::Chunk(chunk) = record else {
            panic!("recovery response must contain only body chunks before its terminal record");
        };
        body.extend_from_slice(chunk);
    }
    (start.status(), serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
async fn db_recovery_flow_v1_rejected_v2_completes() {
    use crate::{
        db::setup_test_db,
        generate_reset_hash,
        jwt::{NewToken, TokenType},
        login_routes::RegisterCredentials,
        models::{
            org_projects::NewOrgProject, orgs::NewOrg, password_reset::NewPasswordResetRequest,
        },
        seed_wrapping::password_reset_code_mac,
        web::attestation_routes::SessionState,
        AppMode, AppStateBuilder,
    };

    let database_url = std::env::var("AEAD_TAMPER_TEST_DATABASE_URL")
        .expect("this test requires the disposable database harness");
    let database_host = url::Url::parse(&database_url).expect("database URL must parse");
    assert!(matches!(
        database_host.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]")
    ));
    let app_state = Arc::new(
        AppStateBuilder::default()
            .app_mode(AppMode::Local)
            .db(setup_test_db(database_url))
            .enclave_key([42u8; 32].to_vec())
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
        .create_org(NewOrg::new(format!("recovery-transport-{marker}")))
        .unwrap();
    let project = app_state
        .db
        .create_org_project(NewOrgProject::new(org.id, "recovery-transport".to_string()))
        .unwrap();
    let email = format!("recovery-transport-{marker}@example.com");
    let password = "recovery-transport-original-password";
    app_state
        .register_user(RegisterCredentials {
            name: Some("Recovery Transport Test".to_string()),
            email: Some(email.clone()),
            password: password.to_string(),
            client_id: project.client_id,
        })
        .await
        .unwrap();
    let authenticated = app_state
        .authenticate_user(Some(email.clone()), None, password.to_string(), project.id)
        .await
        .unwrap()
        .expect("the password credential should open the seed");
    let user = authenticated.user;
    let seed_before = app_state
        .decrypt_seed_for_auth_context(&user, &authenticated.auth_context)
        .unwrap();
    let token = |is_v2: bool| {
        NewToken::new_with_auth_context(
            &user,
            TokenType::access_for_transport(is_v2),
            &app_state,
            &authenticated.auth_context,
        )
        .unwrap()
        .token
    };
    let (v1_token, v2_token) = (token(false), token(true));

    let reset_code = "TRANSP12";
    let reset_secret = "recovery-transport-reset-secret";
    let reset_request = app_state
        .db
        .create_password_reset_request(NewPasswordResetRequest::new(
            user.uuid,
            generate_reset_hash(reset_secret.to_string()),
            password_reset_code_mac(&app_state.enclave_key, project.id, user.uuid, reset_code)
                .unwrap()
                .to_vec(),
            1,
        ))
        .unwrap();
    let proof = serde_json::json!({
        "email": email,
        "alphanumeric_code": reset_code,
        "plaintext_secret": reset_secret,
        "client_id": project.client_id,
    });
    let current_password = serde_json::json!({ "current_password": password });
    let new_password = "recovery-transport-new-password";

    // Production composition: the same application router serves V1 directly
    // and V2 behind the gateway, each with the error-contract layer.
    let application = crate::application_routes(Arc::clone(&app_state))
        .layer(axum::middleware::from_fn(crate::add_error_contract_header));
    let session_v1_id = uuid::Uuid::new_v4();
    let session_v1_key = crate::encrypt::generate_random::<32>();
    app_state
        .store_session_state(session_v1_id, SessionState::new(session_v1_key))
        .await
        .unwrap();
    let session_v2 = test_session(0xE1);
    let sessions = Arc::new(SessionStore::new(NonZeroUsize::new(2).unwrap()));
    sessions.insert(Arc::clone(&session_v2.server)).unwrap();
    let gateway = request_router(application.clone(), sessions);
    let mut request_number = 0;

    // Every recovery route rejects a valid V1 session generically, before
    // enrollment, proof verification, or consumption.
    let placeholder_code = serde_json::json!({
        "proof": proof,
        "new_password": new_password,
        "mode": { "preserve": { "recovery_code": "MPLRC1-NOT-A-CODE" } },
    });
    for (method, target, payload) in [
        (Method::GET, "/protected/recovery-code", None),
        (Method::POST, "/protected/recovery-code/enroll", Some(&current_password)),
        (Method::POST, "/protected/recovery-code/rotate", Some(&current_password)),
        (Method::DELETE, "/protected/recovery-code", Some(&current_password)),
        (
            Method::POST,
            "/password-reset/v2/options",
            Some(&serde_json::json!({ "proof": proof })),
        ),
        (Method::POST, "/password-reset/v2/complete", Some(&placeholder_code)),
    ] {
        let (status, body) = recovery_v1_request(
            &application,
            session_v1_id,
            &session_v1_key,
            method.clone(),
            target,
            payload,
            &v1_token,
        )
        .await;
        assert_eq!(status, 400, "{method} {target} must reject V1 sessions");
        assert_eq!(
            body,
            serde_json::json!({ "status": 400, "message": "Bad Request" }),
            "{method} {target} V1 rejection must stay generic"
        );
    }
    assert!(app_state.db.get_recovery_wrap(user.uuid).unwrap().is_none());
    assert!(app_state
        .db
        .get_password_reset_request_by_user_id_and_code(
            user.uuid,
            password_reset_code_mac(&app_state.enclave_key, project.id, user.uuid, reset_code)
                .unwrap()
                .to_vec(),
        )
        .unwrap()
        .is_some_and(|request| request.id == reset_request.id && !request.is_reset));

    // V2: status, enrollment, reset options, and preserving completion.
    let (status, body) = recovery_v2_request(
        &gateway,
        &session_v2,
        &mut request_number,
        "GET",
        "/protected/recovery-code",
        None,
        Some(&v2_token),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        serde_json::json!({ "enrolled": false, "enrolled_at": null })
    );

    let (status, body) = recovery_v2_request(
        &gateway,
        &session_v2,
        &mut request_number,
        "POST",
        "/protected/recovery-code/enroll",
        Some(&current_password),
        Some(&v2_token),
    )
    .await;
    assert_eq!(status, 200);
    let recovery_code = body["recovery_code"].as_str().unwrap().to_string();
    assert!(recovery_code.starts_with("MPLRC1-"));

    let (status, body) = recovery_v2_request(
        &gateway,
        &session_v2,
        &mut request_number,
        "GET",
        "/protected/recovery-code",
        None,
        Some(&v2_token),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["enrolled"], serde_json::json!(true));

    let (status, body) = recovery_v2_request(
        &gateway,
        &session_v2,
        &mut request_number,
        "POST",
        "/password-reset/v2/options",
        Some(&serde_json::json!({ "proof": proof })),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        serde_json::json!({ "recovery_enrolled": true, "destructive_reset_available": true })
    );

    let (status, body) = recovery_v2_request(
        &gateway,
        &session_v2,
        &mut request_number,
        "POST",
        "/password-reset/v2/complete",
        Some(&serde_json::json!({
            "proof": proof,
            "new_password": new_password,
            "mode": { "preserve": { "recovery_code": recovery_code } },
        })),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(body["access_token"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(body["refresh_token"].as_str().is_some_and(|t| !t.is_empty()));

    // The new password opens the same seed; the recovery code stays active.
    let reauthenticated = app_state
        .authenticate_user(Some(email), None, new_password.to_string(), project.id)
        .await
        .unwrap()
        .expect("the new password should open the preserved seed");
    let seed_after = app_state
        .decrypt_seed_for_auth_context(&reauthenticated.user, &reauthenticated.auth_context)
        .unwrap();
    assert_eq!(seed_after.as_slice(), seed_before.as_slice());
    assert!(app_state.db.get_recovery_wrap(user.uuid).unwrap().is_some());

    let _ = app_state.db.delete_user(&user);
    app_state.db.delete_org_project(&project).unwrap();
    app_state.db.delete_org(&org).unwrap();
}
