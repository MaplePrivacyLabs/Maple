//! System One through the SDK against a running OpenSecret.
//!
//! Validation errors are deterministic on any backend (they come before billing
//! and the provider); answers need a Continuum route for `glm-5-3-flash`, so
//! that case accepts the backend's typed 503/403 where none is configured.

mod common;

use maple_sdk::{OpenSecretClient, Result, SystemOneAnswer, SystemOneQuestion, SystemOneRequest};
use serde_json::json;
use std::env;
use uuid::Uuid;

static TEST_USER_AUTH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn api_url() -> String {
    let env_path = std::path::Path::new("../.env.local");
    if env_path.exists() {
        dotenvy::from_path(env_path).ok();
    } else {
        dotenvy::dotenv().ok();
    }
    env::var("VITE_OPEN_SECRET_API_URL").unwrap_or_else(|_| "http://localhost:3000".to_string())
}

async fn signed_in_client() -> Result<OpenSecretClient> {
    let client = common::new_test_client(api_url())?;
    client.perform_attestation_handshake().await?;

    let email = env::var("VITE_TEST_EMAIL").expect("VITE_TEST_EMAIL must be set");
    let password = env::var("VITE_TEST_PASSWORD").expect("VITE_TEST_PASSWORD must be set");
    let name = env::var("VITE_TEST_NAME").ok();
    let client_id = env::var("VITE_TEST_CLIENT_ID")
        .expect("VITE_TEST_CLIENT_ID must be set")
        .parse::<Uuid>()
        .expect("Invalid client_id format");

    // Parallel tests share one configured user; serialize the login/register bootstrap.
    let _auth_guard = TEST_USER_AUTH_LOCK.lock().await;
    if client
        .login(email.clone(), password.clone(), client_id)
        .await
        .is_err()
    {
        client.register(email, password, client_id, name).await?;
    }
    Ok(client)
}

/// A choice with one option: rejected with 422 `system_one_bad_option_count`.
fn one_option_request() -> SystemOneRequest {
    SystemOneRequest::new(json!("s")).with_question(
        "q",
        SystemOneQuestion::choice("x", [("only", serde_json::Value::Null)]),
    )
}

fn ticket_request() -> SystemOneRequest {
    SystemOneRequest::new(json!({
        "ticket": "Payment went through twice and I need one refunded today."
    }))
    .with_question(
        "is_urgent",
        SystemOneQuestion::noul("Does the customer need help today?"),
    )
    .with_question(
        "intent",
        SystemOneQuestion::choice(
            "What does the customer want?",
            [
                ("refund", "money back"),
                ("question", "information"),
                ("praise", "thanks"),
            ],
        ),
    )
    .with_question(
        "frustration",
        SystemOneQuestion::score("How frustrated is the customer?", ["Low", "Medium", "High"]),
    )
}

fn assert_bad_option_count(error: maple_sdk::Error) {
    assert_eq!(error.api_status(), Some(422), "{error}");
    assert_eq!(
        error.api_error_code().as_deref(),
        Some("system_one_bad_option_count"),
        "{error}"
    );
}

#[tokio::test]
async fn system_one_rejections_carry_status_and_code_with_jwt_and_api_key() -> Result<()> {
    let client = signed_in_client().await?;

    let error = client
        .system_one(one_option_request())
        .await
        .expect_err("one option is rejected");
    assert_bad_option_count(error);

    let created = client
        .create_api_key(format!("system-one-{}", Uuid::new_v4()))
        .await?;
    let per_request = client
        .system_one_with_api_key(one_option_request(), created.key.clone())
        .await
        .expect_err("one option is rejected with a per-request key");
    assert_bad_option_count(per_request);

    let api_key_client = common::new_test_client_with_api_key(api_url(), created.key.clone())?;
    api_key_client.perform_attestation_handshake().await?;
    let configured = api_key_client
        .system_one(one_option_request())
        .await
        .expect_err("one option is rejected with a configured key");
    assert_bad_option_count(configured);

    client.delete_api_key(&created.name).await?;
    Ok(())
}

#[tokio::test]
async fn system_one_answers_all_three_question_types_when_a_route_exists() -> Result<()> {
    let client = signed_in_client().await?;
    let response = match client.system_one(ticket_request()).await {
        Ok(response) => response,
        Err(error) if matches!(error.api_status(), Some(503) | Some(403)) => {
            // No Continuum route (CI) or no entitled plan: the typed error is the contract here.
            eprintln!(
                "System One answers skipped: {error} (code {:?})",
                error.api_error_code()
            );
            return Ok(());
        }
        Err(error) => return Err(error),
    };

    let names: Vec<&str> = response.answers.iter().map(|(name, _)| name).collect();
    assert_eq!(names, ["is_urgent", "intent", "frustration"]);
    assert!(matches!(
        response.answers.get("is_urgent"),
        Some(SystemOneAnswer::Noul { noul, .. }) if (0.0..=1.0).contains(noul)
    ));
    match response.answers.get("intent") {
        Some(SystemOneAnswer::Choice {
            choice,
            probabilities,
            confidence,
            ..
        }) => {
            let labels: Vec<&str> = probabilities.iter().map(|(label, _)| label).collect();
            assert_eq!(labels, ["refund", "question", "praise"]);
            assert!(probabilities.get(choice).is_some_and(|p| *p > 0.0));
            assert!((0.0..=1.0).contains(confidence));
        }
        other => panic!("unexpected intent answer {other:?}"),
    }
    match response.answers.get("frustration") {
        Some(SystemOneAnswer::Score { score, legend, .. }) => {
            let levels: Vec<&str> = legend.iter().map(|(level, _)| level).collect();
            assert_eq!(levels, ["0", "1", "2"]);
            assert!((0.0..=2.0).contains(score));
        }
        other => panic!("unexpected frustration answer {other:?}"),
    }
    assert!(response.usage.requests >= 3);
    Ok(())
}
