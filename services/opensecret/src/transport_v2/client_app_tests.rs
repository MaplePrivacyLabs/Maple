// Included in gateway::tests to exercise the real encrypted transport boundary.

async fn assert_client_app_gateway_roundtrip(
    logical_values: &[&str],
    expected_app: Option<crate::client_app::ClientApp>,
    credential: Option<Credential>,
    outer_value: Option<&str>,
) {
    use crate::client_app::{ClientApp, CLIENT_APP_HEADER};

    let test = test_session(0x61);
    let sessions = Arc::new(SessionStore::new(NonZeroUsize::new(2).unwrap()));
    sessions.insert(Arc::clone(&test.server)).unwrap();
    let expected_credential = credential.clone();
    let expected_header_count = logical_values.len();
    let application = Router::new().route(
        "/inspect-client-app",
        any(move |request: Request<Body>| {
            let expected_credential = expected_credential.clone();
            async move {
                assert_eq!(ClientApp::from_headers(request.headers()), expected_app);
                assert_eq!(
                    request.headers().get_all(CLIENT_APP_HEADER).iter().count(),
                    expected_header_count,
                );
                // Attribution is telemetry only: transport credential identity
                // and kind must survive unchanged, including an absent credential.
                assert!(request.extensions().get::<Credential>() == expected_credential.as_ref());
                assert!(request
                    .extensions()
                    .get::<TransportSession>()
                    .is_some_and(TransportSession::is_v2));
                (StatusCode::OK, "attribution-inspected")
            }
        }),
    );
    let router = request_router(application, sessions);
    let request_id = RequestId::from_bytes([0x62; 16]);
    let envelope = RequestEnvelope::new(
        request_id,
        credential,
        None,
        "POST".into(),
        "/inspect-client-app".into(),
        logical_values
            .iter()
            .map(|value| LogicalHeader::new(CLIENT_APP_HEADER.into(), (*value).into()).unwrap())
            .collect(),
        None,
    )
    .unwrap();
    let record = test
        .client
        .encrypt_request(request_id, &envelope.encode().unwrap())
        .unwrap();
    assert!(!record
        .windows(CLIENT_APP_HEADER.len())
        .any(|part| part == CLIENT_APP_HEADER.as_bytes()));
    let mut request = outer_request(test.server.id(), &test.routing_key, record);
    if let Some(value) = outer_value {
        request
            .headers_mut()
            .insert(CLIENT_APP_HEADER, HeaderValue::from_str(value).unwrap());
    }
    let records = decrypt_records(
        &test.client,
        request_id,
        router.oneshot(request).await.unwrap(),
    )
    .await;
    assert!(matches!(&records[0], ResponseRecord::Start(start) if start.status() == 200));
    assert!(matches!(
        &records[1],
        ResponseRecord::Chunk(body) if body.as_ref() == b"attribution-inspected"
    ));
    assert!(matches!(&records[2], ResponseRecord::End));
}

#[tokio::test]
async fn client_app_encrypted_logical_headers_preserve_labels_and_credential_kinds() {
    use crate::client_app::ClientApp;

    for kind in [CredentialKind::Bearer, CredentialKind::ApiKey] {
        for (label, expected) in [
            ("chat", ClientApp::Chat),
            ("proxy", ClientApp::Proxy),
            ("work", ClientApp::Work),
        ] {
            assert_client_app_gateway_roundtrip(
                &[label],
                Some(expected),
                Some(Credential::new(kind, "gateway-test-credential".into()).unwrap()),
                None,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn client_app_missing_invalid_and_repeated_logical_headers_are_unknown() {
    let cases: &[&[&str]] = &[
        &[],
        &[""],
        &["unknown"],
        &["Chat"],
        &[" work "],
        &["chat,work"],
        &["chat", "chat"],
        &["chat", "work"],
    ];
    for kind in [CredentialKind::Bearer, CredentialKind::ApiKey] {
        for values in cases {
            assert_client_app_gateway_roundtrip(
                values,
                None,
                Some(Credential::new(kind, "gateway-test-credential".into()).unwrap()),
                None,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn client_app_outer_headers_cannot_supply_or_override_logical_attribution() {
    use crate::client_app::ClientApp;

    for (logical, expected) in [(&[][..], None), (&["proxy"][..], Some(ClientApp::Proxy))] {
        assert_client_app_gateway_roundtrip(
            logical,
            expected,
            Some(Credential::new(CredentialKind::Bearer, "gateway-test-credential".into()).unwrap()),
            Some("work"),
        )
        .await;
    }
}

#[tokio::test]
async fn client_app_logical_labels_do_not_create_a_credential() {
    use crate::client_app::ClientApp;

    for (label, expected) in [
        ("chat", ClientApp::Chat),
        ("proxy", ClientApp::Proxy),
        ("work", ClientApp::Work),
    ] {
        assert_client_app_gateway_roundtrip(&[label], Some(expected), None, None).await;
    }
}
