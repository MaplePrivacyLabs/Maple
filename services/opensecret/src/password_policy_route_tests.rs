//! Disposable-database route tests for the server-side password policy.
//!
//! Each test proves a policy-violating new password is rejected with
//! `PasswordTooShort` before any account-password effect, while a valid
//! password keeps working. Run them with a disposable migrated local
//! Postgres, for example through the guarded disposable-DB helper pattern:
//! `RECOVERY_TEST_DATABASE_URL=postgres://... cargo test --locked
//! --all-features password_policy_route_tests -- --ignored --test-threads=1`.

use crate::{
    db::setup_test_db,
    jwt::{validate_jwt, NewToken, TokenType},
    models::{org_projects::OrgProject, schema::org_projects},
    transport_v2::{crypto::SessionId, envelope::Credential},
    web::login_routes::RegisterCredentials,
    web::{
        encryption_middleware::TransportSession, login_routes::router as login_router,
        protected_routes::router as protected_router,
    },
    AppMode, AppState, AppStateBuilder,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware::from_fn_with_state,
};
use diesel::{ExpressionMethods, QueryDsl, RunQueryDsl};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower::ServiceExt;
use uuid::Uuid;

const TEST_ROOT_KEY: [u8; 32] = [42u8; 32];

fn test_database_url() -> Option<String> {
    std::env::var("RECOVERY_TEST_DATABASE_URL")
        .ok()
        .or_else(|| std::env::var("AEAD_TAMPER_TEST_DATABASE_URL").ok())
}

async fn build_local_test_app_state(database_url: String) -> AppState {
    let db = setup_test_db(database_url);
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
        .filter(org_projects::status.eq("active"))
        .order(org_projects::id.asc())
        .first::<OrgProject>(conn)
        .expect("test database should contain at least one active project")
}

/// Reproduces the inner request the transport-v2 gateway hands to the
/// application router: a live session extension, an optional bearer
/// credential, and the already-decrypted JSON body.
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

async fn assert_password_rejected(response: axum::http::Response<Body>, message: &str) {
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(
        body,
        json!({
            "status": 400,
            "message": message,
        })
    );
}

const TOO_SHORT_MESSAGE: &str = "Password must be at least 8 characters";
const TOO_LONG_MESSAGE: &str = "Password must not exceed 64 characters";

fn assert_user_absent(app_state: &AppState, email: &str, project: &OrgProject) {
    match app_state
        .db
        .get_user_by_email(email.to_string(), project.id)
    {
        Err(crate::db::DBError::UserNotFound) => {}
        other => panic!("short-password registration must not create a user; got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn db_register_rejects_empty_and_short_passwords_without_creating_a_user() {
    let Some(database_url) = test_database_url() else {
        panic!("requires a disposable migrated test database URL");
    };
    let app_state = Arc::new(build_local_test_app_state(database_url).await);
    let project = first_active_project(&app_state);
    let app = login_router(app_state.clone());

    for (password, message) in [
        ("", TOO_SHORT_MESSAGE),
        ("1234567", TOO_SHORT_MESSAGE),
        ("x".repeat(65).as_str(), TOO_LONG_MESSAGE),
    ] {
        let email = format!("password-policy-{}@example.com", Uuid::new_v4());

        let response = send(
            app.clone(),
            v2_request(
                "POST",
                "/register",
                Some(json!({
                    "email": email,
                    "password": password,
                    "client_id": project.client_id,
                })),
                None,
            ),
        )
        .await;
        assert_password_rejected(response, message).await;
        assert_user_absent(&app_state, &email, &project);
    }

    // The boundary itself stays open to exactly-the-minimum passwords.
    let email = format!("password-policy-boundary-{}@example.com", Uuid::new_v4());
    app_state
        .register_user(RegisterCredentials {
            name: None,
            email: Some(email.clone()),
            password: "12345678".to_string(),
            client_id: project.client_id,
        })
        .await
        .expect("an exactly-minimum password must still register");
    app_state
        .db
        .get_user_by_email(email, project.id)
        .expect("minimum-length registration should create the user");
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn db_change_password_rejects_short_new_password_without_touching_the_credential() {
    let Some(database_url) = test_database_url() else {
        panic!("requires a disposable migrated test database URL");
    };
    let app_state = Arc::new(build_local_test_app_state(database_url).await);
    let project = first_active_project(&app_state);
    let marker = Uuid::new_v4();
    let email = format!("password-policy-change-{marker}@example.com");
    let current_password = "current-password-8";
    let user = app_state
        .register_user(RegisterCredentials {
            name: None,
            email: Some(email.clone()),
            password: current_password.to_string(),
            client_id: project.client_id,
        })
        .await
        .expect("test password user should register");
    let authenticated = app_state
        .authenticate_user(
            Some(email.clone()),
            None,
            current_password.to_string(),
            project.id,
        )
        .await
        .expect("password should verify")
        .expect("password credential should open the active seed wrap");
    let token = NewToken::new_with_auth_context(
        &authenticated.user,
        TokenType::access_for_transport(true),
        &app_state,
        &authenticated.auth_context,
    )
    .expect("test v2 access token should issue")
    .token;

    let app = protected_router(app_state.clone())
        .route_layer(from_fn_with_state(app_state.clone(), validate_jwt));

    for (short, message) in [
        ("", TOO_SHORT_MESSAGE),
        ("1234567", TOO_SHORT_MESSAGE),
        ("x".repeat(65).as_str(), TOO_LONG_MESSAGE),
    ] {
        let response = send(
            app.clone(),
            v2_request(
                "POST",
                "/protected/change_password",
                Some(json!({
                    "current_password": current_password,
                    "new_password": short,
                })),
                Some(token.clone()),
            ),
        )
        .await;
        assert_password_rejected(response, message).await;
    }

    // The credential must still authenticate with the unchanged password.
    app_state
        .authenticate_user(Some(email), None, current_password.to_string(), project.id)
        .await
        .expect("password should still verify")
        .expect("a rejected change must not touch the password credential");

    app_state.db.delete_user(&user).unwrap();
}
