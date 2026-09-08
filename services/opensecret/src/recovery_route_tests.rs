use crate::{
    db::setup_db,
    jwt::{AuthContext, AuthMethod, NewToken, TokenType},
    login_routes::RegisterCredentials,
    models::{oauth::NewUserOAuthConnection, org_projects::OrgProject, users::NewUser},
    private_key::generate_twelve_word_seed,
    seed_wrapping::CredentialKind,
    transport_v2::{crypto::SessionId, envelope::Credential},
    web::{encryption_middleware::TransportSession, protected_routes::recovery_router},
    AppMode, AppState, AppStateBuilder,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use diesel::{ExpressionMethods, QueryDsl, RunQueryDsl};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower::ServiceExt;
use uuid::Uuid;

const TEST_ROOT_KEY: [u8; 32] = [42u8; 32];

fn test_credential(label: &str) -> &'static str {
    Box::leak(format!("recovery-route-test-{label}").into_boxed_str())
}

async fn build_local_test_app_state(database_url: String) -> AppState {
    let db = setup_db(database_url);
    AppStateBuilder::default()
        .app_mode(AppMode::Local)
        .db(db)
        .enclave_key(TEST_ROOT_KEY.to_vec())
        .aws_credential_manager(Arc::new(RwLock::new(None)))
        .openai_api_base("http://localhost:9".to_string())
        .tinfoil_api_base("http://localhost:9".to_string())
        .jwt_secret([24u8; 32].to_vec())
        .build()
        .await
        .expect("local test app state should build")
}

fn first_active_project(app_state: &AppState) -> OrgProject {
    let conn = &mut app_state
        .db
        .get_pool()
        .get()
        .expect("test database connection should be available");

    crate::models::schema::org_projects::table
        .filter(crate::models::schema::org_projects::status.eq("active"))
        .order(crate::models::schema::org_projects::id.asc())
        .first::<OrgProject>(conn)
        .expect("test database should contain at least one active project")
}

fn test_database_url() -> Option<String> {
    std::env::var("RECOVERY_TEST_DATABASE_URL")
        .ok()
        .or_else(|| std::env::var("AEAD_TAMPER_TEST_DATABASE_URL").ok())
}

/// Registered password user with an active password seed wrap.
struct AuthenticatedFixture {
    app_state: Arc<AppState>,
    user: crate::models::users::User,
    auth_context: AuthContext,
    email: String,
    password: &'static str,
}

async fn authenticated_password_fixture(label: &str) -> AuthenticatedFixture {
    let Some(database_url) = test_database_url() else {
        panic!("requires a disposable migrated test database URL");
    };
    let app_state = build_local_test_app_state(database_url).await;
    let project = first_active_project(&app_state);
    let app_state = Arc::new(app_state);
    let marker = Uuid::new_v4();
    let email = format!("recovery-route-{label}-{marker}@example.com");
    let password = test_credential(label);

    app_state
        .register_user(RegisterCredentials {
            name: Some("Recovery Route Test".to_string()),
            email: Some(email.clone()),
            password: password.to_string(),
            client_id: project.client_id,
        })
        .await
        .expect("test password user should register");

    let authenticated = app_state
        .authenticate_user(Some(email), None, password.to_string(), project.id)
        .await
        .expect("password should verify")
        .expect("password credential should open the active seed wrap");

    AuthenticatedFixture {
        app_state,
        user: authenticated.user,
        auth_context: authenticated.auth_context,
        email: format!("recovery-route-{label}-{marker}@example.com"),
        password,
    }
}

fn v2_access_token(
    app_state: &AppState,
    user: &crate::models::users::User,
    auth_context: &AuthContext,
) -> String {
    NewToken::new_with_auth_context(
        user,
        TokenType::access_for_transport(true),
        app_state,
        auth_context,
    )
    .expect("test v2 access token should issue")
    .token
}

/// Reproduces the inner request the transport-v2 gateway hands to the
/// application router: a live session extension, an optional bearer credential,
/// and the already-decrypted JSON body.
fn v2_request(
    method: &'static str,
    uri: &str,
    body_json: Option<Value>,
    bearer_token: Option<String>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body_json {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };

    let (mut parts, body) = builder
        .body(body)
        .expect("request should build")
        .into_parts();
    parts
        .extensions
        .insert(TransportSession::v2(SessionId::from_bytes([0xAA; 16])));
    if let Some(token) = bearer_token {
        parts.extensions.insert(
            Credential::new(crate::transport_v2::envelope::CredentialKind::Bearer, token)
                .expect("bearer credential should build"),
        );
    }
    Request::from_parts(parts, body)
}

async fn send(app: axum::Router, request: Request<Body>) -> axum::http::Response<Body> {
    app.oneshot(request).await.expect("router should respond")
}

async fn response_json(response: axum::http::Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect(
        "v2 success paths return plain JSON at the inner router; the gateway encrypts the carrier",
    )
}

async fn enroll_request(
    app: axum::Router,
    password: &'static str,
    token: String,
) -> axum::http::Response<Body> {
    send(
        app,
        v2_request(
            "POST",
            "/protected/recovery-code/enroll",
            Some(json!({ "current_password": password })),
            Some(token),
        ),
    )
    .await
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_status_reports_enrollment_state_for_a_password_user() {
    let fixture = authenticated_password_fixture("status").await;
    let app_state = &fixture.app_state;
    let token = v2_access_token(app_state, &fixture.user, &fixture.auth_context);
    let app = recovery_router(app_state.clone());

    let before = send(
        app.clone(),
        v2_request("GET", "/protected/recovery-code", None, Some(token.clone())),
    )
    .await;
    assert_eq!(before.status(), StatusCode::OK);
    let body = response_json(before).await;
    assert_eq!(body, json!({ "enrolled": false, "enrolled_at": null }));

    let enroll = enroll_request(app.clone(), fixture.password, token.clone()).await;
    assert_eq!(enroll.status(), StatusCode::OK);

    let after = send(
        app,
        v2_request("GET", "/protected/recovery-code", None, Some(token)),
    )
    .await;
    assert_eq!(after.status(), StatusCode::OK);
    let body = response_json(after).await;
    assert_eq!(body["enrolled"], json!(true));
    assert!(body["enrolled_at"].as_str().is_some());

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_enroll_returns_one_time_code_wrapping_the_existing_seed() {
    let fixture = authenticated_password_fixture("enroll").await;
    let app_state = &fixture.app_state;

    // The seed observed through the signed JWT auth context; enrollment must
    // wrap exactly this seed without regenerating or substituting it.
    let enrolled_seed = app_state
        .decrypt_seed_for_auth_context(&fixture.user, &fixture.auth_context)
        .expect("authenticated seed should open before enrollment");

    let token = v2_access_token(app_state, &fixture.user, &fixture.auth_context);
    let app = recovery_router(app_state.clone());

    let response = enroll_request(app.clone(), fixture.password, token.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;

    // The encrypted response carries the code exactly once in canonical form.
    assert_eq!(body.as_object().expect("single field response").len(), 1);
    let displayed = body["recovery_code"].as_str().expect("recovery code field");
    assert!(displayed.starts_with("MPLRC1-"), "canonical display prefix");
    let parts: Vec<&str> = displayed.split('-').collect();
    assert_eq!(
        parts.len(),
        16,
        "prefix + 13 secret groups + 2 checksum groups"
    );
    for part in parts.iter().skip(1) {
        assert_eq!(part.len(), 4, "canonical group size");
    }

    // A wrap exists over the exact enrolled seed (byte-for-byte identity is
    // enforced at crypto level by `verify_recovery_seed_wrapping` unit tests;
    // here we confirm the enrollment state and seed equality independently).
    let wrap = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap should load")
        .expect("wrap should exist after enrollment");
    assert_eq!(wrap.credential_kind, CredentialKind::Recovery.as_str());
    let seed_after_enrollment = app_state
        .decrypt_seed_for_auth_context(&fixture.user, &fixture.auth_context)
        .expect("authenticated seed should open after enrollment");
    assert_eq!(*seed_after_enrollment, *enrolled_seed);

    // Exactly one recovery wrap exists.
    let wraps = app_state
        .db
        .get_user_seed_wrappings_for_user_and_kind(
            fixture.user.uuid,
            CredentialKind::Recovery.as_str(),
        )
        .expect("wrap list should load");
    assert_eq!(wraps.len(), 1);

    // Enrollment already exists: second enrollment conflicts.
    let second = enroll_request(app.clone(), fixture.password, token.clone()).await;
    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(
        app_state
            .db
            .get_user_seed_wrappings_for_user_and_kind(
                fixture.user.uuid,
                CredentialKind::Recovery.as_str()
            )
            .unwrap()
            .len(),
        1
    );

    // A wrong password is rejected before any wrap is created or replaced.
    let wrong_password = enroll_request(app, "not-the-password", token).await;
    assert_eq!(wrong_password.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        app_state
            .db
            .get_user_seed_wrappings_for_user_and_kind(
                fixture.user.uuid,
                CredentialKind::Recovery.as_str()
            )
            .unwrap()
            .len(),
        1
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_rotate_and_disable_lifecycle_behaves_as_documented() {
    let fixture = authenticated_password_fixture("rotate").await;
    let app_state = &fixture.app_state;
    let token = v2_access_token(app_state, &fixture.user, &fixture.auth_context);
    let app = recovery_router(app_state.clone());

    // Rotation before enrollment is rejected.
    let rotate_missing = send(
        app.clone(),
        v2_request(
            "POST",
            "/protected/recovery-code/rotate",
            Some(json!({ "current_password": fixture.password })),
            Some(token.clone()),
        ),
    )
    .await;
    assert_eq!(rotate_missing.status(), StatusCode::BAD_REQUEST);

    let enroll = enroll_request(app.clone(), fixture.password, token.clone()).await;
    assert_eq!(enroll.status(), StatusCode::OK);
    let first_code = response_json(enroll).await["recovery_code"]
        .as_str()
        .unwrap()
        .to_string();
    let first_wrap = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .unwrap()
        .expect("wrap should exist");

    // Rotation replaces the wrap and returns a fresh one-time code.
    let rotate = send(
        app.clone(),
        v2_request(
            "POST",
            "/protected/recovery-code/rotate",
            Some(json!({ "current_password": fixture.password })),
            Some(token.clone()),
        ),
    )
    .await;
    assert_eq!(rotate.status(), StatusCode::OK);
    let rotated = response_json(rotate).await["recovery_code"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(
        rotated, first_code,
        "each rotation must return a fresh code"
    );
    let second_wrap = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .unwrap()
        .expect("replacement wrap should exist");
    assert_ne!(second_wrap.id, first_wrap.id, "CAS replaces the row");

    // The seed survives rotation: the password credential still opens it.
    let still_open = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            fixture.password.to_string(),
            fixture.user.project_id,
        )
        .await;
    assert!(
        still_open.is_ok() && still_open.unwrap().is_some(),
        "rotation must not damage the enrolled seed"
    );
    let rotated_parts: Vec<&str> = rotated.split('-').collect();
    assert_eq!(rotated_parts.len(), 16, "rotated code stays canonical");
    for part in rotated_parts.iter().skip(1) {
        assert_eq!(part.len(), 4, "canonical group size");
    }

    // Wrong-password rotation is rejected and keeps the current wrap.
    let wrong_rotate = send(
        app.clone(),
        v2_request(
            "POST",
            "/protected/recovery-code/rotate",
            Some(json!({ "current_password": "not-the-password" })),
            Some(token.clone()),
        ),
    )
    .await;
    assert_eq!(wrong_rotate.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        app_state
            .db
            .get_recovery_wrap(fixture.user.uuid)
            .unwrap()
            .expect("wrap survives")
            .id,
        second_wrap.id
    );

    // Disable removes the wrap; a second disable stays successful.
    let disable = send(
        app.clone(),
        v2_request(
            "DELETE",
            "/protected/recovery-code",
            Some(json!({ "current_password": fixture.password })),
            Some(token.clone()),
        ),
    )
    .await;
    assert_eq!(disable.status(), StatusCode::OK);
    assert!(
        app_state
            .db
            .get_recovery_wrap(fixture.user.uuid)
            .unwrap()
            .is_none(),
        "disable should remove the recovery wrap"
    );

    let disable_again = send(
        app,
        v2_request(
            "DELETE",
            "/protected/recovery-code",
            Some(json!({ "current_password": fixture.password })),
            Some(token),
        ),
    )
    .await;
    assert_eq!(disable_again.status(), StatusCode::OK);

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_management_rejects_unauthenticated_api_key_and_v1_contexts() {
    let fixture = authenticated_password_fixture("contexts").await;
    let app_state = &fixture.app_state;
    let app = recovery_router(app_state.clone());

    // Unauthenticated v2: no bearer credential at all.
    let unauthenticated = send(
        app.clone(),
        v2_request("GET", "/protected/recovery-code", None, None),
    )
    .await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    // API-key context inside a v2 envelope is not a bearer JWT.
    let (mut api_key_parts, api_key_body) =
        v2_request("GET", "/protected/recovery-code", None, None).into_parts();
    api_key_parts.extensions.insert(
        Credential::new(
            crate::transport_v2::envelope::CredentialKind::ApiKey,
            "opensecret-test-api-key".to_string(),
        )
        .expect("api key credential should build"),
    );
    let api_key_v2 = Request::from_parts(api_key_parts, api_key_body);
    assert_eq!(
        send(app.clone(), api_key_v2).await.status(),
        StatusCode::UNAUTHORIZED,
        "API-key contexts must not reach recovery management"
    );

    // v1 transport with a valid user JWT is rejected by the middleware gate
    // before any recovery logic runs.
    let v1_token = NewToken::new_with_auth_context(
        &fixture.user,
        TokenType::access_for_transport(false),
        app_state,
        &fixture.auth_context,
    )
    .expect("v1 access token should issue")
    .token;
    let v1_request_value = Request::builder()
        .method("GET")
        .uri("/protected/recovery-code")
        .header("authorization", format!("Bearer {v1_token}"))
        .body(Body::empty())
        .unwrap();
    let (mut v1_parts, v1_body) = v1_request_value.into_parts();
    v1_parts
        .extensions
        .insert(TransportSession::v1(Uuid::new_v4()));
    let v1_transport = Request::from_parts(v1_parts, v1_body);
    let v1_response = send(app.clone(), v1_transport).await;
    assert_eq!(
        v1_response.status(),
        StatusCode::BAD_REQUEST,
        "v1 transport sessions must be rejected at the middleware boundary"
    );

    // A v1 session with an unparseable token must still be rejected with 400
    // (the transport gate), not 401 (the JWT middleware). This pins the
    // middleware order: the v2-transport gate runs before JWT validation.
    let garbage_v1 = Request::builder()
        .method("GET")
        .uri("/protected/recovery-code")
        .header("authorization", "Bearer not-a-jwt")
        .body(Body::empty())
        .unwrap();
    let (mut garbage_parts, garbage_body) = garbage_v1.into_parts();
    garbage_parts
        .extensions
        .insert(TransportSession::v1(Uuid::new_v4()));
    let garbage_transport = Request::from_parts(garbage_parts, garbage_body);
    assert_eq!(
        send(app, garbage_transport).await.status(),
        StatusCode::BAD_REQUEST,
        "the v2-transport gate must run before the JWT middleware"
    );

    assert!(
        app_state
            .db
            .get_recovery_wrap(fixture.user.uuid)
            .unwrap()
            .is_none(),
        "rejected transports must not create recovery state"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_management_rejects_guest_and_oauth_only_users() {
    let app_state = build_local_test_app_state(test_database_url().unwrap()).await;
    let project = first_active_project(&app_state);
    let marker = Uuid::new_v4();

    // Guest account.
    let guest = app_state
        .db
        .create_user(NewUser::new(None, None, project.id))
        .expect("guest user should insert");

    // OAuth-only account with a live OAuth seed wrap.
    let oauth_email = format!("recovery-oauth-{marker}@example.com");
    let oauth_user = app_state
        .db
        .create_user(NewUser::new(Some(oauth_email.clone()), None, project.id))
        .expect("OAuth user should insert");
    let provider = app_state
        .db
        .get_oauth_provider_by_name("github")
        .expect("provider lookup should succeed")
        .expect("github provider should exist after AppState build");
    let provider_user_id = format!("oauth-sub-{marker}");
    app_state
        .db
        .create_user_oauth_connection(NewUserOAuthConnection {
            user_id: oauth_user.uuid,
            provider_id: provider.id,
            provider_user_id: provider_user_id.clone(),
            access_token_enc: Vec::new(),
            refresh_token_enc: None,
            expires_at: None,
        })
        .expect("OAuth connection should insert");
    let seed_words = generate_twelve_word_seed(app_state.aws_credential_manager.clone())
        .await
        .expect("test seed should generate");
    app_state
        .create_oauth_seed_wrap_for_user(
            &oauth_user,
            "github",
            &provider_user_id,
            seed_words.to_string().as_bytes(),
        )
        .expect("OAuth seed wrap should insert");
    let app_state = Arc::new(app_state);

    let app = recovery_router(app_state.clone());

    // Guests carry no credential that opens a seed wrap, so the JWT
    // middleware itself rejects them before recovery logic.
    let guest_auth_context = AuthContext::new(AuthMethod::Password, project.id, [0x11; 32]);
    let guest_token = v2_access_token(&app_state, &guest, &guest_auth_context);
    let guest_response = send(
        app.clone(),
        v2_request(
            "POST",
            "/protected/recovery-code/enroll",
            Some(json!({})),
            Some(guest_token),
        ),
    )
    .await;
    assert_eq!(
        guest_response.status(),
        StatusCode::UNAUTHORIZED,
        "guest accounts must fail JWT validation before recovery logic"
    );

    // OAuth-only accounts pass JWT validation but cannot enroll.
    let oauth_auth_context = app_state
        .oauth_auth_context_for_user(&oauth_user, "github", &provider_user_id)
        .expect("OAuth auth context should build");
    let oauth_token = v2_access_token(&app_state, &oauth_user, &oauth_auth_context);
    let oauth_response = send(
        app,
        v2_request(
            "POST",
            "/protected/recovery-code/enroll",
            Some(json!({ "current_password": "anything" })),
            Some(oauth_token),
        ),
    )
    .await;
    assert_eq!(
        oauth_response.status(),
        StatusCode::BAD_REQUEST,
        "OAuth-only users must be rejected at the handler boundary"
    );

    assert!(
        app_state
            .db
            .get_recovery_wrap(oauth_user.uuid)
            .unwrap()
            .is_none(),
        "rejected users must not gain recovery state"
    );

    let _ = app_state.db.delete_user(&guest);
    let _ = app_state.db.delete_user(&oauth_user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_handlers_reject_v1_session_before_any_database_side_effect() {
    let fixture = authenticated_password_fixture("handler-v1").await;
    let app_state = &fixture.app_state;

    // Bare routes with no middleware: reproduces a mis-wired router where a
    // v1 session reaches the handler. Each handler must still refuse before
    // any database work.
    let bare = |app_state: Arc<AppState>| {
        axum::Router::new()
            .route(
                "/protected/recovery-code",
                axum::routing::get(crate::web::protected_routes::recovery_status)
                    .delete(crate::web::protected_routes::disable_recovery),
            )
            .route(
                "/protected/recovery-code/enroll",
                axum::routing::post(crate::web::protected_routes::enroll_recovery),
            )
            .route(
                "/protected/recovery-code/rotate",
                axum::routing::post(crate::web::protected_routes::rotate_recovery),
            )
            .with_state(app_state)
    };

    // The decryption middleware stores the raw payload in extensions and the
    // `Decrypted<T>` extractor wraps it at extraction time.
    let cases: Vec<(&'static str, &str)> = vec![
        ("GET", "/protected/recovery-code"),
        ("DELETE", "/protected/recovery-code"),
        ("POST", "/protected/recovery-code/enroll"),
        ("POST", "/protected/recovery-code/rotate"),
    ];

    for (method, uri) in cases {
        let (mut parts, request_body) = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
            .into_parts();
        parts
            .extensions
            .insert(TransportSession::v1(Uuid::new_v4()));
        parts.extensions.insert(fixture.user.clone());
        parts.extensions.insert(fixture.auth_context.clone());
        parts
            .extensions
            .insert(crate::web::protected_routes::EnrollRecoveryRequest {
                current_password: fixture.password.to_string(),
            });
        parts
            .extensions
            .insert(crate::web::protected_routes::RotateRecoveryRequest {
                current_password: fixture.password.to_string(),
            });
        parts
            .extensions
            .insert(crate::web::protected_routes::DisableRecoveryRequest {
                current_password: fixture.password.to_string(),
            });
        // Status/disability paths carry no body; enroll/rotate payload types
        // above cover the two decrypting handlers. Inserting every payload
        // type is harmless: each handler consumes only its own.
        let request = Request::from_parts(parts, request_body);

        let response = send(bare(app_state.clone()), request).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{method} {uri}: the handler must independently reject v1 sessions"
        );
    }

    assert!(app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .unwrap()
        .is_none());

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_error_responses_are_sanitized() {
    let fixture = authenticated_password_fixture("sanitized").await;
    let app_state = &fixture.app_state;
    let token = v2_access_token(app_state, &fixture.user, &fixture.auth_context);
    let app = recovery_router(app_state.clone());

    // Rotation with no enrollment returns the generic sanitized body.
    let rotate_error = send(
        app.clone(),
        v2_request(
            "POST",
            "/protected/recovery-code/rotate",
            Some(json!({ "current_password": fixture.password })),
            Some(token.clone()),
        ),
    )
    .await;
    assert_eq!(rotate_error.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(rotate_error.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({ "status": 400, "message": "Bad Request" }),
        "public errors must stay generic"
    );

    // Wrong-password enrollment remains a generic 401 body.
    let unauthorized = enroll_request(app, "not-the-password", token).await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let bytes = axum::body::to_bytes(unauthorized.into_body(), usize::MAX)
        .await
        .unwrap();
    let error_body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        error_body.as_object().is_some_and(|object| {
            object.len() == 2 && object.contains_key("status") && object.contains_key("message")
        }),
        "unauthorized error responses must stay generic"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}
