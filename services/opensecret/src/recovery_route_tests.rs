use crate::{
    db::setup_db,
    generate_reset_hash,
    jwt::{validate_token, AuthContext, AuthMethod, NewToken, TokenType, TRANSPORT_V2_USER_ACCESS},
    login_routes::RegisterCredentials,
    models::{
        oauth::NewUserOAuthConnection,
        org_projects::{NewOrgProject, OrgProject},
        password_reset::{NewPasswordResetRequest, PasswordResetRequest},
        schema::password_reset_requests,
        user_kv::{NewUserKV, UserKV},
        user_seed_wrappings::{NewUserSeedWrapping, UserSeedWrapping},
        users::NewUser,
    },
    private_key::generate_twelve_word_seed,
    recovery_code::RecoveryCode,
    seed_wrapping::{
        compute_recovery_auth_binding, decrypt_seed_v1, new_recovery_seed_wrapping,
        password_reset_code_mac, verify_recovery_seed_wrapping, CredentialKind,
    },
    transport_v2::{crypto::SessionId, envelope::Credential},
    web::{
        encryption_middleware::TransportSession, login_routes::router as login_router,
        protected_routes::recovery_router,
    },
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
use zeroize::Zeroizing;

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

    // The displayed code re-parses and opens the stored wrap over the exact
    // enrolled seed, byte-for-byte.
    let parsed =
        RecoveryCode::parse(displayed).expect("the displayed code must re-parse and pass checksum");
    let wrap = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap should load")
        .expect("wrap should exist after enrollment");
    assert_eq!(wrap.credential_kind, CredentialKind::Recovery.as_str());
    let wrap_shape = NewUserSeedWrapping::new(
        wrap.user_id,
        wrap.credential_kind.clone(),
        wrap.credential_lookup_hash.clone(),
        wrap.wrapping_version,
        wrap.seed_enc.clone(),
    );
    verify_recovery_seed_wrapping(
        &TEST_ROOT_KEY,
        &fixture.user,
        &parsed,
        &enrolled_seed,
        &wrap_shape,
    )
    .expect("the stored wrap must open with the displayed code over the enrolled seed");

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
    RecoveryCode::parse(&rotated).expect("rotated code must parse");

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

// ---------------------------------------------------------------------------
// Phase 5: transport-v2 password reset options
// ---------------------------------------------------------------------------

/// A proof fixture for the options route: email coordinates, the emailed
/// alphanumeric code, the client reset secret, and the project client id.
struct ResetProofFixture {
    email: String,
    code: String,
    secret: String,
    client_id: Uuid,
}

impl ResetProofFixture {
    fn request_body(&self) -> Value {
        json!({
            "proof": {
                "email": self.email,
                "alphanumeric_code": self.code,
                "plaintext_secret": self.secret,
                "client_id": self.client_id,
            }
        })
    }
}

fn proof_fixture(email: &str, code: &str, secret: &str, client_id: Uuid) -> ResetProofFixture {
    ResetProofFixture {
        email: email.to_string(),
        code: code.to_string(),
        secret: secret.to_string(),
        client_id,
    }
}

/// Inserts an active password reset request with a known code/secret pair,
/// bypassing the email delivery path. `expiration_hours` may be negative to
/// place the request in the past.
fn insert_reset_request_fixture(
    app_state: &AppState,
    project: &OrgProject,
    user: &crate::models::users::User,
    code: &str,
    secret: &str,
    expiration_hours: i64,
) -> PasswordResetRequest {
    let reset_code_mac =
        password_reset_code_mac(&app_state.enclave_key, project.id, user.uuid, code)
            .expect("reset code mac should compute");
    app_state
        .db
        .create_password_reset_request(NewPasswordResetRequest::new(
            user.uuid,
            generate_reset_hash(secret.to_string()),
            reset_code_mac.to_vec(),
            expiration_hours,
        ))
        .expect("reset request should insert")
}

fn reset_request_by_id(app_state: &AppState, id: i32) -> PasswordResetRequest {
    let conn = &mut app_state
        .db
        .get_pool()
        .get()
        .expect("test database connection should be available");
    password_reset_requests::table
        .filter(password_reset_requests::id.eq(id))
        .first::<PasswordResetRequest>(conn)
        .expect("reset request row should load")
}

/// Inserts one enrollment-state wrap directly through the Phase 2/3 helpers,
/// the same way the DB lifecycle tests create recovery state.
fn insert_enrolled_recovery_wrap(app_state: &AppState, user: &crate::models::users::User) {
    let code = RecoveryCode {
        secret: Zeroizing::new([0x9Au8; 32]),
    };
    let seed = format!("recovery-options-fixture-seed-{}", user.uuid).into_bytes();
    let wrapping = new_recovery_seed_wrapping(&TEST_ROOT_KEY, user, &code, &seed)
        .expect("the options fixture wrap should seal");
    app_state
        .db
        .insert_recovery_wrap_if_absent(wrapping)
        .expect("the options fixture wrap should insert");
}

async fn register_password_user(
    app_state: &AppState,
    project: &OrgProject,
    email: String,
    label: &str,
) -> crate::models::users::User {
    app_state
        .register_user(RegisterCredentials {
            name: Some("Recovery Options Test".to_string()),
            email: Some(email),
            password: test_credential(label).to_string(),
            client_id: project.client_id,
        })
        .await
        .expect("test password user should register")
}

fn create_active_project(app_state: &AppState, org_id: i32) -> OrgProject {
    let conn = &mut app_state
        .db
        .get_pool()
        .get()
        .expect("test database connection should be available");
    NewOrgProject::new(org_id, format!("recovery-options-{}", Uuid::new_v4()))
        .insert(conn)
        .expect("test project should insert")
}

fn delete_test_project(app_state: &AppState, project: &OrgProject) {
    let conn = &mut app_state
        .db
        .get_pool()
        .get()
        .expect("test database connection should be available");
    project.delete(conn).expect("test project should delete");
}

async fn assert_generic_bad_request(response: axum::http::Response<Body>) {
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({ "status": 400, "message": "Bad Request" }),
        "probe rejections must stay generic"
    );
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_options_reveals_status_after_proof_without_consuming_the_request() {
    let fixture = authenticated_password_fixture("v2-options").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    let proof = proof_fixture(
        &fixture.email,
        "OABCDE12",
        "recovery-options-correct-secret",
        project.client_id,
    );
    insert_enrolled_recovery_wrap(app_state, &fixture.user);
    let active = insert_reset_request_fixture(
        app_state,
        &project,
        &fixture.user,
        "OABCDE12",
        &proof.secret,
        24,
    );

    // A wrong client secret reveals nothing and stays generic.
    let wrong_secret = proof_fixture(
        &fixture.email,
        "OABCDE12",
        "not-the-secret",
        project.client_id,
    );
    let wrong = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(wrong_secret.request_body()),
            None,
        ),
    )
    .await;
    assert_generic_bad_request(wrong).await;

    // After the full proof succeeds, the enrolled status is revealed.
    let revealed = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(proof.request_body()),
            None,
        ),
    )
    .await;
    assert_eq!(revealed.status(), StatusCode::OK);
    assert_eq!(
        response_json(revealed).await,
        json!({ "recovery_enrolled": true, "destructive_reset_available": true })
    );

    // Repeating the same proof is safe and changes nothing: the route is
    // read-only until the request expires or a completion consumes it.
    let repeat = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(proof.request_body()),
            None,
        ),
    )
    .await;
    assert_eq!(repeat.status(), StatusCode::OK);

    let reloaded = reset_request_by_id(app_state, active.id);
    assert_eq!(reloaded.user_id, active.user_id);
    assert_eq!(reloaded.hashed_secret, active.hashed_secret);
    assert_eq!(reloaded.encrypted_code, active.encrypted_code);
    assert_eq!(reloaded.expiration_time, active.expiration_time);
    assert!(
        !reloaded.is_reset,
        "the options route must never consume the reset request"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_options_rejects_copied_reset_rows_across_users_and_projects() {
    let fixture = authenticated_password_fixture("v2-copy").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    // A second registered account with its own reset request shares the
    // project with the enrolled account.
    let email_b = format!("recovery-route-v2-copy-b-{}@example.com", Uuid::new_v4());
    let user_b = register_password_user(app_state, &project, email_b.clone(), "v2-copy-b").await;

    // A second project under the same org holds another password account, so
    // copying can be attempted across project boundaries in both directions.
    let project_extra = create_active_project(app_state, project.org_id);
    let email_c = format!("recovery-route-v2-copy-c-{}@example.com", Uuid::new_v4());
    let user_c =
        register_password_user(app_state, &project_extra, email_c.clone(), "v2-copy-c").await;

    insert_enrolled_recovery_wrap(app_state, &fixture.user);
    let secret = "recovery-options-correct-secret";
    let active_a =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "OABCDE12", secret, 24);
    let active_b = insert_reset_request_fixture(
        app_state,
        &project,
        &user_b,
        "OF111111",
        "recovery-options-b-secret",
        24,
    );

    // B's own proof succeeds and reports B's un-enrolled status.
    let own_b = proof_fixture(
        &email_b,
        "OF111111",
        "recovery-options-b-secret",
        project.client_id,
    );
    let own_response = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(own_b.request_body()),
            None,
        ),
    )
    .await;
    assert_eq!(own_response.status(), StatusCode::OK);
    assert_eq!(
        response_json(own_response).await,
        json!({ "recovery_enrolled": false, "destructive_reset_available": true }),
        "each account learns only its own enrollment state"
    );

    // A's code and secret copied onto B's email must not reveal A's status.
    let copied_user = proof_fixture(&email_b, "OABCDE12", secret, project.client_id);
    let copied_user_response = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(copied_user.request_body()),
            None,
        ),
    )
    .await;
    assert_generic_bad_request(copied_user_response).await;

    // The full proof copied onto a foreign project scope also fails.
    let copied_project = proof_fixture(&fixture.email, "OABCDE12", secret, project_extra.client_id);
    let copied_project_response = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(copied_project.request_body()),
            None,
        ),
    )
    .await;
    assert_generic_bad_request(copied_project_response).await;

    // A wrong client secret fails even with the right code coordinates.
    let wrong_secret = proof_fixture(&email_b, "OF111111", secret, project.client_id);
    let wrong_secret_response = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(wrong_secret.request_body()),
            None,
        ),
    )
    .await;
    assert_generic_bad_request(wrong_secret_response).await;

    // No attempted copy may have consumed or re-bound either reset row, and
    // no attempt may have changed recovery state.
    for active in [&active_a, &active_b] {
        let reloaded = reset_request_by_id(app_state, active.id);
        assert_eq!(
            reloaded.hashed_secret, active.hashed_secret,
            "copied proofs must not re-bind a reset request to a new secret"
        );
        assert!(
            !reloaded.is_reset,
            "copied proofs must not consume a reset request"
        );
    }
    assert!(app_state
        .db
        .recovery_wrap_exists(fixture.user.uuid)
        .unwrap());
    assert!(
        !app_state.db.recovery_wrap_exists(user_b.uuid).unwrap(),
        "no probe may create recovery state for user B"
    );
    assert!(
        !app_state.db.recovery_wrap_exists(user_c.uuid).unwrap(),
        "copied proofs must not create recovery state on other accounts"
    );

    let _ = app_state.db.delete_user(&fixture.user);
    let _ = app_state.db.delete_user(&user_b);
    let _ = app_state.db.delete_user(&user_c);
    delete_test_project(app_state, &project_extra);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_options_rejects_v1_and_missing_transport_sessions() {
    let fixture = authenticated_password_fixture("v2-gate").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    let secret = "recovery-options-gate-secret";
    let active =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "OFFFFF12", secret, 24);
    let proof = proof_fixture(&fixture.email, "OFFFFF12", secret, project.client_id);

    // A v1 transport session presenting the entire valid proof is rejected
    // before the handler can verify anything or reveal status.
    let request_value = Request::builder()
        .method("POST")
        .uri("/password-reset/v2/options")
        .body(Body::from(proof.request_body().to_string()))
        .unwrap();
    let (mut v1_parts, v1_body) = request_value.into_parts();
    v1_parts
        .extensions
        .insert(TransportSession::v1(Uuid::new_v4()));
    let v1_response = send(app.clone(), Request::from_parts(v1_parts, v1_body)).await;
    assert_generic_bad_request(v1_response).await;

    // A request that carries no transport session never established
    // transport security and must not reach the handler either.
    let sessionless = Request::builder()
        .method("POST")
        .uri("/password-reset/v2/options")
        .body(Body::from(proof.request_body().to_string()))
        .unwrap();
    let sessionless_response = send(app, sessionless).await;
    assert_generic_bad_request(sessionless_response).await;

    let reloaded = reset_request_by_id(app_state, active.id);
    assert!(
        !reloaded.is_reset,
        "rejected transports must not consume state"
    );
    assert!(
        !app_state
            .db
            .recovery_wrap_exists(fixture.user.uuid)
            .unwrap(),
        "rejected transports must not create recovery state"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_options_probe_outcomes_stay_generic_and_safe() {
    let fixture = authenticated_password_fixture("v2-oracle").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    let secret = "recovery-options-oracle-secret";
    let active =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "OAAAAA11", secret, 24);

    // Unknown account coordinates: an email with no reset request, and an
    // unknown project client id, must be indistinguishable from a wrong code.
    let unknown_email = proof_fixture(
        &format!("recovery-route-v2-oracle-{}@example.com", Uuid::new_v4()),
        "OAAAAA11",
        secret,
        project.client_id,
    );
    let unknown_email_response = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(unknown_email.request_body()),
            None,
        ),
    )
    .await;
    assert_generic_bad_request(unknown_email_response).await;

    let unknown_client = proof_fixture(&fixture.email, "OAAAAA11", secret, Uuid::new_v4());
    let unknown_client_response = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(unknown_client.request_body()),
            None,
        ),
    )
    .await;
    assert_generic_bad_request(unknown_client_response).await;

    let wrong_code = proof_fixture(&fixture.email, "OBBBBB22", secret, project.client_id);
    let wrong_code_response = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(wrong_code.request_body()),
            None,
        ),
    )
    .await;
    assert_generic_bad_request(wrong_code_response).await;

    // An expired request rejects the otherwise-valid proof with the same
    // generic body.
    let expired =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "OCCCCCC3", secret, -1);
    let expired_proof = proof_fixture(&fixture.email, "OCCCCCC3", secret, project.client_id);
    let expired_response = send(
        app.clone(),
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(expired_proof.request_body()),
            None,
        ),
    )
    .await;
    assert_generic_bad_request(expired_response).await;
    assert!(
        !reset_request_by_id(app_state, expired.id).is_reset,
        "an expired probe must not consume any request"
    );

    // Once the request is consumed — the way a completed reset would consume
    // it — the same proof is rejected with the same generic body.
    let conn = &mut app_state
        .db
        .get_pool()
        .get()
        .expect("test database connection should be available");
    active
        .mark_as_reset(conn)
        .expect("simulated consumption should mark the request");
    let consumed_response = send(
        app,
        v2_request(
            "POST",
            "/password-reset/v2/options",
            Some(
                proof_fixture(&fixture.email, "OAAAAA11", secret, project.client_id).request_body(),
            ),
            None,
        ),
    )
    .await;
    assert_generic_bad_request(consumed_response).await;
    assert!(
        reset_request_by_id(app_state, active.id).is_reset,
        "options must not resurrect a consumed request"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

// ---------------------------------------------------------------------------
// Phase 6: transport-v2 password reset completion
// ---------------------------------------------------------------------------

fn preserve_mode(recovery_code: &str) -> Value {
    json!({ "preserve": { "recovery_code": recovery_code } })
}

fn destructive_mode(acknowledge_data_loss: bool) -> Value {
    json!({
        "destructive": { "acknowledge_data_loss": acknowledge_data_loss }
    })
}

fn complete_body(proof: &ResetProofFixture, new_password: &str, mode: Value) -> Value {
    json!({
        "proof": {
            "email": proof.email,
            "alphanumeric_code": proof.code,
            "plaintext_secret": proof.secret,
            "client_id": proof.client_id,
        },
        "new_password": new_password,
        "mode": mode,
    })
}

async fn complete_request(app: axum::Router, body: Value) -> axum::http::Response<Body> {
    send(
        app,
        v2_request("POST", "/password-reset/v2/complete", Some(body), None),
    )
    .await
}

/// Enrolls recovery over the user's authenticated seed through the Phase 2/3
/// helpers — the same seed the enroll route wraps — and returns the displayed
/// one-time code that opens the stored wrap.
async fn enroll_recovery_over_authenticated_seed(
    app_state: &AppState,
    user: &crate::models::users::User,
    auth_context: &AuthContext,
) -> String {
    let seed = app_state
        .decrypt_seed_for_auth_context(user, auth_context)
        .expect("authenticated seed should open before enrollment");
    let code = RecoveryCode::generate(None)
        .await
        .expect("recovery code should generate");
    let wrapping = new_recovery_seed_wrapping(&TEST_ROOT_KEY, user, &code, &seed)
        .expect("enrollment wrap should seal");
    app_state
        .db
        .insert_recovery_wrap_if_absent(wrapping)
        .expect("enrollment wrap should insert");
    code.display().to_string()
}

/// A user-private data row that the destructive reset deletes and the
/// preserving reset must leave untouched.
fn insert_kv_marker(app_state: &AppState, user: &crate::models::users::User) -> UserKV {
    let conn = &mut app_state
        .db
        .get_pool()
        .get()
        .expect("test database connection should be available");
    NewUserKV::new(
        user.uuid,
        b"recovery-preserve-marker".to_vec(),
        b"preserve-marker-value".to_vec(),
    )
    .insert(conn)
    .expect("marker row should insert")
}

fn kv_marker_exists(app_state: &AppState, user_id: Uuid, key_enc: &Vec<u8>) -> bool {
    let conn = &mut app_state
        .db
        .get_pool()
        .get()
        .expect("test database connection should be available");
    UserKV::get_by_user_and_key(conn, user_id, key_enc)
        .expect("marker lookup should work")
        .is_some()
}

fn wrap_rows_identical(a: &UserSeedWrapping, b: &UserSeedWrapping) -> bool {
    a.id == b.id
        && a.user_id == b.user_id
        && a.credential_kind == b.credential_kind
        && a.credential_lookup_hash == b.credential_lookup_hash
        && a.wrapping_version == b.wrapping_version
        && a.seed_enc == b.seed_enc
        && a.created_at == b.created_at
        && a.updated_at == b.updated_at
}

fn assert_recovery_wrap_unchanged(app_state: &AppState, user_id: Uuid, before: &UserSeedWrapping) {
    let after = app_state
        .db
        .get_recovery_wrap(user_id)
        .expect("wrap should load")
        .expect("wrap should still exist");
    assert!(
        wrap_rows_identical(before, &after),
        "the recovery wrap must remain byte-for-byte unchanged"
    );
}

fn open_recovery_wrap_with_code(
    user: &crate::models::users::User,
    wrap: &UserSeedWrapping,
    code: &RecoveryCode,
) -> Vec<u8> {
    let binding = compute_recovery_auth_binding(
        &TEST_ROOT_KEY,
        user.project_id,
        user.uuid,
        code.secret_bytes(),
    )
    .expect("recovery auth binding should compute");
    decrypt_seed_v1(
        &TEST_ROOT_KEY,
        &wrap.seed_enc,
        user.uuid,
        user.project_id,
        CredentialKind::Recovery,
        &binding,
    )
    .expect("wrap should open with the recovery code")
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_preserving_completion_preserves_seed_wrap_and_encrypted_data() {
    let fixture = authenticated_password_fixture("v2-complete").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    let seed_before = app_state
        .decrypt_seed_for_auth_context(&fixture.user, &fixture.auth_context)
        .expect("authenticated seed should open before the reset");
    let marker = insert_kv_marker(app_state, &fixture.user);
    let code_display =
        enroll_recovery_over_authenticated_seed(app_state, &fixture.user, &fixture.auth_context)
            .await;
    let wrap_before = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap should load")
        .expect("wrap should exist after enrollment");

    let secret = "recovery-complete-correct-secret";
    let selected =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PQRSTUV1", secret, 24);
    let other =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PWWWWWW2", secret, 24);
    let proof = proof_fixture(&fixture.email, "PQRSTUV1", secret, project.client_id);
    let new_password = "recovery-preserve-new-password";

    let response = complete_request(
        app,
        complete_body(&proof, new_password, preserve_mode(&code_display)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert!(body["message"].as_str().is_some());
    assert!(body["access_token"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(body["refresh_token"]
        .as_str()
        .is_some_and(|t| !t.is_empty()));

    // The issued access token carries the new password credential's signed
    // auth context and opens the exact same seed.
    let claims = validate_token(
        body["access_token"].as_str().expect("access token"),
        app_state,
        TRANSPORT_V2_USER_ACCESS,
    )
    .expect("the issued access token should validate");
    let token_auth_context =
        AuthContext::from_claims(&claims).expect("claims should carry an auth context");
    app_state
        .verify_seed_wrap_for_auth_context(&fixture.user, &token_auth_context)
        .expect("the issued token must open the seed through the new credential");
    assert_eq!(
        app_state
            .decrypt_seed_for_auth_context(&fixture.user, &token_auth_context)
            .expect("token auth context should open the seed"),
        seed_before,
        "preserving reset must keep the seed unchanged"
    );

    // The recovery wrap survives byte-for-byte.
    assert_recovery_wrap_unchanged(app_state, fixture.user.uuid, &wrap_before);

    // The old password stops authenticating; the new one authenticates and
    // opens the same seed.
    let old_login = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            fixture.password.to_string(),
            fixture.user.project_id,
        )
        .await
        .expect("authentication check should run");
    assert!(old_login.is_none(), "the old password must stop working");
    let new_login = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            new_password.to_string(),
            fixture.user.project_id,
        )
        .await
        .expect("authentication check should run")
        .expect("the new password must authenticate");
    assert_eq!(
        app_state
            .decrypt_seed_for_auth_context(&new_login.user, &new_login.auth_context)
            .expect("the new credential should open the seed"),
        seed_before
    );

    // Existing seed-key-encrypted data is untouched by a preserving reset.
    assert!(
        kv_marker_exists(app_state, fixture.user.uuid, &marker.key_enc),
        "preserving reset must not delete user-private data"
    );

    // Matching the current reset behavior, every active request is consumed.
    assert!(reset_request_by_id(app_state, selected.id).is_reset);
    assert!(reset_request_by_id(app_state, other.id).is_reset);

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_preserving_rejects_invalid_recovery_codes_without_consuming_the_request()
{
    let fixture = authenticated_password_fixture("v2-badcode").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    let code_display =
        enroll_recovery_over_authenticated_seed(app_state, &fixture.user, &fixture.auth_context)
            .await;
    let wrap_before = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap should load")
        .expect("wrap should exist");

    let secret = "recovery-badcode-secret";
    let selected =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PAAAAAA1", secret, 24);
    let other =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PBBBBBB2", secret, 24);
    let proof = proof_fixture(&fixture.email, "PAAAAAA1", secret, project.client_id);
    let new_password = "recovery-badcode-new-password";

    // Malformed input is rejected before any database work.
    let malformed = complete_request(
        app.clone(),
        complete_body(&proof, new_password, preserve_mode("not-a-code")),
    )
    .await;
    assert_generic_bad_request(malformed).await;
    assert!(
        !reset_request_by_id(app_state, selected.id).is_reset,
        "a malformed code must leave the selected request active"
    );
    assert_recovery_wrap_unchanged(app_state, fixture.user.uuid, &wrap_before);

    // A checksum-invalid code of the right shape is rejected the same way.
    let mut tampered = code_display.clone();
    let last = tampered.pop().expect("displayed code has a last char");
    tampered.push(if last == '0' { '1' } else { '0' });
    let checksum_invalid = complete_request(
        app.clone(),
        complete_body(&proof, new_password, preserve_mode(&tampered)),
    )
    .await;
    assert_generic_bad_request(checksum_invalid).await;
    assert!(
        !reset_request_by_id(app_state, selected.id).is_reset,
        "a checksum-invalid code must leave the selected request active"
    );
    assert_recovery_wrap_unchanged(app_state, fixture.user.uuid, &wrap_before);
    assert!(!reset_request_by_id(app_state, other.id).is_reset);

    // The still-active request completes with the correct code.
    let success = complete_request(
        app,
        complete_body(&proof, new_password, preserve_mode(&code_display)),
    )
    .await;
    assert_eq!(success.status(), StatusCode::OK);

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_well_formed_wrong_code_consumes_only_the_selected_request() {
    let fixture = authenticated_password_fixture("v2-wrongcode").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    enroll_recovery_over_authenticated_seed(app_state, &fixture.user, &fixture.auth_context).await;
    let wrap_before = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap should load")
        .expect("wrap should exist");

    let secret = "recovery-wrongcode-secret";
    let selected =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PCCCCCC1", secret, 24);
    let other =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PDDDDDD2", secret, 24);
    let proof = proof_fixture(&fixture.email, "PCCCCCC1", secret, project.client_id);
    let new_password = "recovery-wrongcode-new-password";

    // A well-formed code with a valid checksum that is simply wrong fails
    // AEAD and consumes exactly the selected request.
    let wrong_code = RecoveryCode::generate(None)
        .await
        .expect("a well-formed but wrong code should generate")
        .display()
        .to_string();
    let wrong = complete_request(
        app,
        complete_body(&proof, new_password, preserve_mode(&wrong_code)),
    )
    .await;
    assert_generic_bad_request(wrong).await;

    assert!(
        reset_request_by_id(app_state, selected.id).is_reset,
        "a well-formed wrong code must consume the selected request"
    );
    assert!(
        !reset_request_by_id(app_state, other.id).is_reset,
        "only the selected request may be consumed"
    );
    assert_recovery_wrap_unchanged(app_state, fixture.user.uuid, &wrap_before);

    // Nothing else changed: the old password still authenticates.
    let old_login = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            fixture.password.to_string(),
            fixture.user.project_id,
        )
        .await
        .expect("authentication check should run");
    assert!(
        old_login.is_some(),
        "a failed attempt must not change the password"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_correct_completion_racing_a_failed_attempt_has_one_winner() {
    let fixture = authenticated_password_fixture("v2-race-attempt").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    let code_display =
        enroll_recovery_over_authenticated_seed(app_state, &fixture.user, &fixture.auth_context)
            .await;
    let wrap_before = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap should load")
        .expect("wrap should exist");

    let secret = "recovery-race-attempt-secret";
    let selected =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PEEEEEE1", secret, 24);
    let proof = proof_fixture(&fixture.email, "PEEEEEE1", secret, project.client_id);
    let new_password = "recovery-race-attempt-password";

    let wrong_code = RecoveryCode::generate(None)
        .await
        .expect("a well-formed but wrong code should generate")
        .display()
        .to_string();
    let (correct, wrong) = tokio::join!(
        complete_request(
            app.clone(),
            complete_body(&proof, new_password, preserve_mode(&code_display))
        ),
        complete_request(
            app,
            complete_body(&proof, new_password, preserve_mode(&wrong_code))
        ),
    );

    assert_eq!(
        wrong.status(),
        StatusCode::BAD_REQUEST,
        "the wrong-code attempt must never succeed"
    );
    let correct_won = correct.status() == StatusCode::OK;

    // Exactly one operation consumed the selected request; the loser either
    // failed AEAD or lost the guarded consume, and nothing is left half-done.
    assert!(
        reset_request_by_id(app_state, selected.id).is_reset,
        "the selected request must be consumed by the single winner"
    );
    assert_recovery_wrap_unchanged(app_state, fixture.user.uuid, &wrap_before);

    let new_password_works = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            new_password.to_string(),
            fixture.user.project_id,
        )
        .await
        .expect("authentication check should run")
        .is_some();
    assert_eq!(
        new_password_works, correct_won,
        "the password must change exactly when the correct completion wins"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_destructive_completion_reuses_destructive_behavior_without_recovery() {
    let fixture = authenticated_password_fixture("v2-destructive").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    let seed_before = app_state
        .decrypt_seed_for_auth_context(&fixture.user, &fixture.auth_context)
        .expect("authenticated seed should open before the reset");
    let marker = insert_kv_marker(app_state, &fixture.user);
    // Recovery is enrolled, so destructive completion must remove it and
    // create no replacement.
    enroll_recovery_over_authenticated_seed(app_state, &fixture.user, &fixture.auth_context).await;

    let secret = "recovery-destructive-secret";
    let selected =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PFFFFFF1", secret, 24);
    let other =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PGGGGGG2", secret, 24);
    let proof = proof_fixture(&fixture.email, "PFFFFFF1", secret, project.client_id);
    let new_password = "recovery-destructive-new-password";

    // An unacknowledged destructive request is rejected before any proof or
    // account lookup.
    let unacknowledged_proof = proof_fixture(&fixture.email, "PHHHHHH3", secret, project.client_id);
    let unacknowledged_request =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PHHHHHH3", secret, 24);
    let unacknowledged = complete_request(
        app.clone(),
        complete_body(&unacknowledged_proof, new_password, destructive_mode(false)),
    )
    .await;
    assert_generic_bad_request(unacknowledged).await;
    assert!(
        !reset_request_by_id(app_state, unacknowledged_request.id).is_reset,
        "an unacknowledged request must not consume anything"
    );

    let response = complete_request(
        app,
        complete_body(&proof, new_password, destructive_mode(true)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert!(body["access_token"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(body["refresh_token"]
        .as_str()
        .is_some_and(|t| !t.is_empty()));

    // The old password stops working; the new one authenticates and opens a
    // fresh seed, not the preserved one.
    let old_login = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            fixture.password.to_string(),
            fixture.user.project_id,
        )
        .await
        .expect("authentication check should run");
    assert!(old_login.is_none());
    let new_login = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            new_password.to_string(),
            fixture.user.project_id,
        )
        .await
        .expect("authentication check should run")
        .expect("the new password must authenticate");
    let seed_after = app_state
        .decrypt_seed_for_auth_context(&new_login.user, &new_login.auth_context)
        .expect("the new credential should open the new seed");
    assert_ne!(
        seed_after, seed_before,
        "destructive reset must generate a fresh seed"
    );

    // Destructive cleanup ran: no recovery wrap (was enrolled), user-private
    // data deleted, and no recovery wrap created.
    assert!(
        app_state
            .db
            .get_recovery_wrap(fixture.user.uuid)
            .expect("wrap lookup should work")
            .is_none(),
        "destructive reset must delete the enrolled recovery wrap"
    );
    assert!(
        !kv_marker_exists(app_state, fixture.user.uuid, &marker.key_enc),
        "destructive reset must delete user-private data"
    );
    assert!(reset_request_by_id(app_state, selected.id).is_reset);
    assert!(reset_request_by_id(app_state, other.id).is_reset);

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_concurrent_destructive_completions_have_exactly_one_winner() {
    let fixture = authenticated_password_fixture("v2-destructive-race").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    let secret = "recovery-destructive-race-secret";
    let selected =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PIIIIII1", secret, 24);
    let proof = proof_fixture(&fixture.email, "PIIIIII1", secret, project.client_id);
    let new_password = "recovery-destructive-race-password";

    let (first, second) = tokio::join!(
        complete_request(
            app.clone(),
            complete_body(&proof, new_password, destructive_mode(true))
        ),
        complete_request(
            app,
            complete_body(&proof, new_password, destructive_mode(true))
        ),
    );

    let winners = [first.status(), second.status()]
        .into_iter()
        .filter(|status| *status == StatusCode::OK)
        .count();
    assert_eq!(
        winners, 1,
        "exactly one concurrent destructive completion may win"
    );

    let new_password_works = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            new_password.to_string(),
            fixture.user.project_id,
        )
        .await
        .expect("authentication check should run")
        .is_some();
    assert!(new_password_works, "the winning password must authenticate");
    assert!(
        reset_request_by_id(app_state, selected.id).is_reset,
        "the selected request must be consumed exactly once"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_completion_rejects_v1_and_missing_transport_sessions() {
    let fixture = authenticated_password_fixture("v2-complete-gate").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    let secret = "recovery-complete-gate-secret";
    let active =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PJJJJJJ1", secret, 24);
    let proof = proof_fixture(&fixture.email, "PJJJJJJ1", secret, project.client_id);
    let body = complete_body(&proof, "recovery-gate-password", preserve_mode("MPLRC1"));

    // A v1 transport session presenting the entire valid proof is rejected
    // before the handler can verify anything or consume state.
    let request_value = Request::builder()
        .method("POST")
        .uri("/password-reset/v2/complete")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (mut v1_parts, v1_body) = request_value.into_parts();
    v1_parts
        .extensions
        .insert(TransportSession::v1(Uuid::new_v4()));
    let v1_response = send(app.clone(), Request::from_parts(v1_parts, v1_body)).await;
    assert_generic_bad_request(v1_response).await;

    // A request that carries no transport session never established
    // transport security and must not reach the handler either.
    let sessionless = Request::builder()
        .method("POST")
        .uri("/password-reset/v2/complete")
        .body(Body::from(body.to_string()))
        .unwrap();
    let sessionless_response = send(app, sessionless).await;
    assert_generic_bad_request(sessionless_response).await;

    let reloaded = reset_request_by_id(app_state, active.id);
    assert!(
        !reloaded.is_reset,
        "rejected transports must not consume state"
    );
    assert!(
        !app_state
            .db
            .recovery_wrap_exists(fixture.user.uuid)
            .expect("wrap lookup should work"),
        "rejected transports must not create recovery state"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_preserving_reset_races_safely_with_password_change() {
    let fixture = authenticated_password_fixture("v2-race-change").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());

    let seed_before = app_state
        .decrypt_seed_for_auth_context(&fixture.user, &fixture.auth_context)
        .expect("authenticated seed should open before the race");
    let code_display =
        enroll_recovery_over_authenticated_seed(app_state, &fixture.user, &fixture.auth_context)
            .await;
    let wrap_before = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap should load")
        .expect("wrap should exist");

    let secret = "recovery-race-change-secret";
    let selected =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PKKKKKK1", secret, 24);
    let proof = proof_fixture(&fixture.email, "PKKKKKK1", secret, project.client_id);
    let new_password = "recovery-race-change-password";

    let (complete_res, change_res) = tokio::join!(
        complete_request(
            app,
            complete_body(&proof, new_password, preserve_mode(&code_display))
        ),
        app_state.update_user_password_and_seed_wrap(
            &fixture.user,
            &fixture.auth_context,
            "recovery-race-concurrent-change".to_string(),
        ),
    );

    assert_eq!(
        complete_res.status(),
        StatusCode::OK,
        "the preserving reset must not lose to a password change: neither touches the reset request or the recovery wrap"
    );
    match change_res {
        Ok(_) => {}
        // The password change lost its expected-verifier CAS to the reset's
        // user-row lock; that is the defined loser outcome.
        Err(crate::Error::AuthenticationError) => {}
        Err(e) => panic!("password change failed unexpectedly: {e:?}"),
    }

    // The final password is the preserving-reset password in either
    // interleaving, the seed is unchanged, and the recovery wrap is
    // byte-for-byte unchanged.
    let new_login = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            new_password.to_string(),
            fixture.user.project_id,
        )
        .await
        .expect("authentication check should run")
        .expect("the preserving password must authenticate");
    assert_eq!(
        app_state
            .decrypt_seed_for_auth_context(&new_login.user, &new_login.auth_context)
            .expect("the new credential should open the seed"),
        seed_before
    );
    assert!(
        app_state
            .authenticate_user(
                Some(fixture.email.clone()),
                None,
                fixture.password.to_string(),
                fixture.user.project_id
            )
            .await
            .expect("authentication check should run")
            .is_none(),
        "the old password must stop working"
    );
    assert!(
        app_state
            .authenticate_user(
                Some(fixture.email.clone()),
                None,
                "recovery-race-concurrent-change".to_string(),
                fixture.user.project_id
            )
            .await
            .expect("authentication check should run")
            .is_none(),
        "a password change that lost the race must not authenticate"
    );
    assert_recovery_wrap_unchanged(app_state, fixture.user.uuid, &wrap_before);
    assert!(
        reset_request_by_id(app_state, selected.id).is_reset,
        "the preserving reset consumed the selected request"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_preserving_reset_races_safely_with_rotation() {
    let fixture = authenticated_password_fixture("v2-race-rotate").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());
    let recovery_app = recovery_router(app_state.clone());

    let seed_before = app_state
        .decrypt_seed_for_auth_context(&fixture.user, &fixture.auth_context)
        .expect("authenticated seed should open before the race");
    let code_display =
        enroll_recovery_over_authenticated_seed(app_state, &fixture.user, &fixture.auth_context)
            .await;
    let wrap_before = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap should load")
        .expect("wrap should exist");
    let token = v2_access_token(app_state, &fixture.user, &fixture.auth_context);

    let secret = "recovery-race-rotate-secret";
    let selected =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PLLLLLL1", secret, 24);
    let proof = proof_fixture(&fixture.email, "PLLLLLL1", secret, project.client_id);
    let new_password = "recovery-race-rotate-password";

    let (complete_res, rotate_res) = tokio::join!(
        complete_request(
            app,
            complete_body(&proof, new_password, preserve_mode(&code_display))
        ),
        send(
            recovery_app,
            v2_request(
                "POST",
                "/protected/recovery-code/rotate",
                Some(json!({ "current_password": fixture.password })),
                Some(token),
            ),
        ),
    );

    let preserving_ok = complete_res.status() == StatusCode::OK;
    let rotate_ok = rotate_res.status() == StatusCode::OK;
    assert!(
        preserving_ok || rotate_ok,
        "at least one of the two operations must win: statuses {:?} / {:?}",
        complete_res.status(),
        rotate_res.status()
    );

    // The final recovery wrap opens over the same seed with whichever code
    // the last committed state carries.
    let final_wrap = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap should load")
        .expect("a recovery wrap must exist after the race");
    let opening_code = if rotate_ok {
        let rotated_display = response_json(rotate_res).await["recovery_code"]
            .as_str()
            .expect("rotation returns a one-time code")
            .to_string();
        RecoveryCode::parse(&rotated_display).expect("the rotated code must parse")
    } else {
        // Rotation lost its seed credential to the preserving commit; the
        // originally enrolled wrap is untouched.
        assert_eq!(final_wrap.id, wrap_before.id, "the enrolled wrap survives");
        RecoveryCode::parse(&code_display).expect("the enrolled code must parse")
    };
    assert_eq!(
        open_recovery_wrap_with_code(&fixture.user, &final_wrap, &opening_code),
        seed_before,
        "rotation must preserve the seed in every interleaving"
    );

    // The password changed exactly when the preserving reset won.
    let new_password_works = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            new_password.to_string(),
            fixture.user.project_id,
        )
        .await
        .expect("authentication check should run")
        .is_some();
    assert_eq!(new_password_works, preserving_ok);
    assert_eq!(
        reset_request_by_id(app_state, selected.id).is_reset,
        preserving_ok,
        "the preserving reset consumed the selected request only when it won"
    );

    let _ = app_state.db.delete_user(&fixture.user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn password_reset_v2_preserving_reset_races_safely_with_disablement() {
    let fixture = authenticated_password_fixture("v2-race-disable").await;
    let app_state = &fixture.app_state;
    let project = first_active_project(app_state);
    let app = login_router(app_state.clone());
    let recovery_app = recovery_router(app_state.clone());

    let seed_before = app_state
        .decrypt_seed_for_auth_context(&fixture.user, &fixture.auth_context)
        .expect("authenticated seed should open before the race");
    let code_display =
        enroll_recovery_over_authenticated_seed(app_state, &fixture.user, &fixture.auth_context)
            .await;
    let wrap_before = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap should load")
        .expect("wrap should exist");
    let token = v2_access_token(app_state, &fixture.user, &fixture.auth_context);

    let secret = "recovery-race-disable-secret";
    let selected =
        insert_reset_request_fixture(app_state, &project, &fixture.user, "PMMMMMM1", secret, 24);
    let proof = proof_fixture(&fixture.email, "PMMMMMM1", secret, project.client_id);
    let new_password = "recovery-race-disable-password";

    let (complete_res, disable_res) = tokio::join!(
        complete_request(
            app,
            complete_body(&proof, new_password, preserve_mode(&code_display))
        ),
        send(
            recovery_app,
            v2_request(
                "DELETE",
                "/protected/recovery-code",
                Some(json!({ "current_password": fixture.password })),
                Some(token),
            ),
        ),
    );

    let preserving_ok = complete_res.status() == StatusCode::OK;
    let disable_status = disable_res.status();
    assert!(
        disable_status == StatusCode::OK || disable_status == StatusCode::UNAUTHORIZED,
        "disablement either runs with its live credential or loses it to the preserving commit: got {disable_status:?}"
    );

    // The password changed exactly when the preserving reset won.
    let new_password_works = app_state
        .authenticate_user(
            Some(fixture.email.clone()),
            None,
            new_password.to_string(),
            fixture.user.project_id,
        )
        .await
        .expect("authentication check should run")
        .is_some();
    assert_eq!(new_password_works, preserving_ok);
    assert_eq!(
        reset_request_by_id(app_state, selected.id).is_reset,
        preserving_ok,
        "a losing preserving reset must leave the selected request active for a destructive retry"
    );

    // Disablement with a live credential always removes the wrap; a
    // disablement that lost its credential to the preserving commit leaves
    // the wrap byte-for-byte unchanged.
    let final_wrap = app_state
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .expect("wrap lookup should work");
    if disable_status == StatusCode::OK {
        assert!(
            final_wrap.is_none(),
            "a disablement that ran must remove the wrap"
        );
    } else {
        let wrap = final_wrap.expect("the enrolled wrap must survive a losing disablement");
        assert!(
            wrap_rows_identical(&wrap_before, &wrap),
            "the enrolled wrap must remain byte-for-byte unchanged"
        );
        assert_eq!(
            open_recovery_wrap_with_code(
                &fixture.user,
                &wrap,
                &RecoveryCode::parse(&code_display).expect("enrolled code parses"),
            ),
            seed_before
        );
    }

    let _ = app_state.db.delete_user(&fixture.user);
}
