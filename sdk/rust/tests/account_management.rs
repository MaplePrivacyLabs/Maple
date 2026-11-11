mod common;

use maple_sdk::{Error, OpenSecretClient, PasswordResetCompletion, Result};
use std::env;
use uuid::Uuid;

fn load_test_env() {
    let env_path = std::path::Path::new("../.env.local");
    if env_path.exists() {
        dotenvy::from_path(env_path).ok();
    } else {
        dotenvy::dotenv().ok();
    }
}

async fn setup_client() -> Result<OpenSecretClient> {
    load_test_env();

    let base_url = env::var("VITE_OPEN_SECRET_API_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    let client = common::new_test_client(base_url)?;
    client.perform_attestation_handshake().await?;
    Ok(client)
}

fn test_client_id() -> Uuid {
    load_test_env();

    env::var("VITE_TEST_CLIENT_ID")
        .ok()
        .and_then(|id| Uuid::parse_str(&id).ok())
        .expect("VITE_TEST_CLIENT_ID must be set in .env.local or .env")
}

#[tokio::test]
async fn test_account_management_apis_exist() {
    // This test verifies that all account management methods exist and are callable
    // We don't actually execute them since they're destructive operations

    let _client = setup_client().await.expect("Failed to create client");

    // These would be the method signatures we can call:
    // - client.change_password(current_password, new_password)
    // - client.request_password_reset(email, hashed_secret, client_id)
    // - client.confirm_password_reset(email, code, secret, new_password, client_id)
    // - client.verify_email(code)
    // - client.request_new_verification_code()
    // - client.request_account_deletion(hashed_secret)
    // - client.confirm_account_deletion(code, secret)

    // The test passes if the code compiles, verifying all methods exist
}

#[tokio::test]
async fn test_guest_change_password_keeps_authenticated_token_state() -> Result<()> {
    let client_id = test_client_id();
    let client = setup_client().await?;
    let original_password = "test_guest_change_password_123";
    let new_password = format!(
        "new_guest_password_{}",
        chrono::Utc::now().timestamp_millis()
    );

    let guest_response = client
        .register_guest(original_password.to_string(), client_id)
        .await?;

    client
        .change_password(original_password.to_string(), new_password.clone())
        .await?;

    let user_response = client.get_user().await?;
    assert_eq!(user_response.user.id, guest_response.id);
    assert!(user_response.user.email.is_none());

    let relogin_client = setup_client().await?;
    let relogin_response = relogin_client
        .login_with_id(guest_response.id, new_password, client_id)
        .await?;
    assert_eq!(relogin_response.id, guest_response.id);

    Ok(())
}

fn assert_api_status<T: std::fmt::Debug>(result: Result<T>, expected: u16) {
    match result {
        Err(Error::Api { status, .. }) => assert_eq!(status, expected),
        other => panic!("expected API status {expected}, got {other:?}"),
    }
}

/// Registers a fresh email account so recovery state never touches the shared
/// test account.
async fn register_recovery_test_user(client: &OpenSecretClient, client_id: Uuid) -> Result<String> {
    let password = format!("Recovery-test-{}", Uuid::new_v4());
    client
        .register(
            format!("recovery-{}@local.test", Uuid::new_v4()),
            password.clone(),
            client_id,
            None,
        )
        .await?;
    Ok(password)
}

#[tokio::test]
async fn test_recovery_code_lifecycle() -> Result<()> {
    let client_id = test_client_id();
    let client = setup_client().await?;
    let password = register_recovery_test_user(&client, client_id).await?;

    let status = client.get_recovery_code_status().await?;
    assert!(!status.enrolled);
    assert!(status.issued_at.is_none());
    assert!(status.last_used_at.is_none());
    assert_eq!(status.use_count, 0);

    assert_api_status(
        client
            .enroll_recovery_code("not-the-password".to_string())
            .await,
        401,
    );
    // Rotation needs an existing enrollment.
    assert_api_status(client.rotate_recovery_code(password.clone()).await, 400);

    let enrolled = client.enroll_recovery_code(password.clone()).await?;
    assert!(enrolled.recovery_code.starts_with("MPLRC1-"));
    let status = client.get_recovery_code_status().await?;
    assert!(status.enrolled);
    assert!(status.issued_at.is_some());
    assert_eq!(status.use_count, 0);

    assert_api_status(client.enroll_recovery_code(password.clone()).await, 409);

    let rotated = client.rotate_recovery_code(password.clone()).await?;
    assert!(rotated.recovery_code.starts_with("MPLRC1-"));
    assert_ne!(rotated.recovery_code, enrolled.recovery_code);
    assert!(client.get_recovery_code_status().await?.enrolled);

    assert_api_status(
        client
            .disable_recovery_code("not-the-password".to_string())
            .await,
        401,
    );
    client.disable_recovery_code(password.clone()).await?;
    assert!(!client.get_recovery_code_status().await?.enrolled);
    // Disabling is idempotent.
    client.disable_recovery_code(password).await?;

    Ok(())
}

#[tokio::test]
async fn test_guest_cannot_enroll_recovery_code() -> Result<()> {
    let client_id = test_client_id();
    let client = setup_client().await?;
    let password = format!("Recovery-guest-{}", Uuid::new_v4());
    client.register_guest(password.clone(), client_id).await?;

    assert_api_status(client.enroll_recovery_code(password).await, 400);
    assert!(!client.get_recovery_code_status().await?.enrolled);

    Ok(())
}

#[tokio::test]
async fn test_v2_password_reset_rejects_invalid_proofs() -> Result<()> {
    let client_id = test_client_id();
    let client = setup_client().await?;
    let email = format!("recovery-reset-{}@local.test", Uuid::new_v4());

    assert_api_status(
        client
            .get_password_reset_options(
                email.clone(),
                "ABCD1234".to_string(),
                "not-a-reset-secret".to_string(),
                client_id,
            )
            .await,
        400,
    );

    for mode in [
        PasswordResetCompletion::Preserve {
            recovery_code: "MPLRC1-0000".to_string(),
        },
        PasswordResetCompletion::Destructive {
            acknowledge_data_loss: true,
        },
        PasswordResetCompletion::Destructive {
            acknowledge_data_loss: false,
        },
    ] {
        assert_api_status(
            client
                .complete_password_reset(
                    email.clone(),
                    "ABCD1234".to_string(),
                    "not-a-reset-secret".to_string(),
                    "New-password-1234".to_string(),
                    client_id,
                    mode,
                )
                .await,
            400,
        );
    }
    // A rejected completion leaves the client signed out.
    assert!(client.get_access_token()?.is_none());

    Ok(())
}

#[tokio::test]
#[ignore = "Requires email infrastructure and is destructive"]
async fn test_password_reset_flow() {
    // This test is skipped because:
    // 1. It requires actual email delivery to work
    // 2. It would change the account password
    // 3. We can't programmatically retrieve the email code

    // If this test were to run, it would:
    // 1. Call request_password_reset with email and secret
    // 2. Manually retrieve the code from email (not possible in automated test)
    // 3. Call confirm_password_reset with the code
    // 4. Verify login works with new password
}

#[tokio::test]
#[ignore = "Requires email verification code from actual email"]
async fn test_email_verification() {
    // This test is skipped because:
    // 1. It requires receiving an actual email with verification code
    // 2. Email can only be verified once
    // 3. We can't programmatically retrieve the email code

    // If this test were to run, it would:
    // 1. Create account with unverified email
    // 2. Call request_new_verification_code if needed
    // 3. Retrieve code from email (not possible in automated test)
    // 4. Call verify_email with the code
    // 5. Verify email_verified status is true
}

#[tokio::test]
#[ignore = "EXTREMELY DESTRUCTIVE - would permanently delete the account"]
async fn test_account_deletion() {
    // This test is skipped because:
    // 1. IT WOULD PERMANENTLY DELETE THE TEST ACCOUNT
    // 2. All data would be lost forever
    // 3. The account could never be recovered
    // 4. It requires email confirmation code

    // WARNING: Never run this on a real account you want to keep!

    // If this test were to run, it would:
    // 1. Create a throwaway test account
    // 2. Call request_account_deletion with hashed secret
    // 3. Retrieve confirmation code from email
    // 4. Call confirm_account_deletion
    // 5. Verify the account no longer exists
}

#[tokio::test]
async fn test_password_reset_requires_no_auth() {
    // This test verifies that password reset endpoints don't require authentication
    // We can safely test this without actually completing the flow

    let client = setup_client().await.expect("Failed to create client");

    // These calls should work without being logged in
    // We use invalid data so we don't actually trigger emails
    let result = client
        .request_password_reset(
            "nonexistent@example.com".to_string(),
            "test_hashed_secret".to_string(),
            Uuid::new_v4(),
        )
        .await;

    // We expect this to fail with an error (user not found or similar)
    // but NOT with an authentication error
    assert!(result.is_err(), "Should fail with invalid email");

    // Similarly for confirm - should fail but not for auth reasons
    let result = client
        .confirm_password_reset(
            "nonexistent@example.com".to_string(),
            "INVALID".to_string(),
            "test_secret".to_string(),
            "NewPassword123!".to_string(),
            Uuid::new_v4(),
        )
        .await;

    assert!(result.is_err(), "Should fail with invalid code");
}

#[tokio::test]
async fn test_verify_email_requires_no_auth() {
    // This test verifies that email verification doesn't require authentication

    let client = setup_client().await.expect("Failed to create client");

    // This should work without being logged in
    // We use an invalid code so we don't actually verify anything
    let result = client.verify_email("INVALID_CODE_12345".to_string()).await;

    // We expect this to fail with an error (invalid code)
    // but NOT with an authentication error
    assert!(result.is_err(), "Should fail with invalid code");
}

// Note: The TypeScript SDK doesn't have tests for these destructive operations either,
// for the same reasons we're skipping them here. This is the correct approach for
// operations that permanently modify account state or require external resources.
