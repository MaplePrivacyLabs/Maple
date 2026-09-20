use crate::models::org_project_secrets::NewOrgProjectSecret;
use crate::models::project_settings::{EmailSettings, OAuthProviderSettings, OAuthSettings};
use crate::web::platform::{
    PROJECT_GITHUB_OAUTH_SECRET, PROJECT_GOOGLE_OAUTH_SECRET, PROJECT_RESEND_API_KEY,
};
use crate::{AppMode, AppState, Error};
use secp256k1::SecretKey;
use std::sync::Arc;
use tracing::{debug, error, info};

pub async fn run_migrations(
    app_state: &Arc<AppState>,
    github_client_secret: Option<String>,
    google_client_secret: Option<String>,
    github_client_id: Option<String>,
    google_client_id: Option<String>,
) -> Result<(), Error> {
    debug!("Starting migrations");
    migrate_maple_project_settings(
        app_state,
        github_client_secret,
        google_client_secret,
        github_client_id,
        google_client_id,
    )
    .await?;
    debug!("Migrations completed successfully");
    Ok(())
}

async fn migrate_maple_project_settings(
    app_state: &Arc<AppState>,
    github_client_secret: Option<String>,
    google_client_secret: Option<String>,
    github_client_id: Option<String>,
    google_client_id: Option<String>,
) -> Result<(), Error> {
    info!("Checking Maple project settings migration");

    let org = app_state
        .db
        .get_org_by_name("OpenSecret")?
        .expect("OpenSecret organization must exist");

    let maple = app_state
        .db
        .get_org_project_by_name_and_org("Maple", org.id)?
        .expect("Maple project must exist");

    let needs_email_migration = app_state.db.get_project_email_settings(maple.id)?.is_none();
    let needs_oauth_migration = app_state.db.get_project_oauth_settings(maple.id)?.is_none();

    if needs_email_migration || needs_oauth_migration {
        info!("Starting Maple project settings migration");
        perform_maple_settings_migration(
            app_state,
            maple.id,
            github_client_secret,
            google_client_secret,
            github_client_id,
            google_client_id,
        )
        .await?;
    } else {
        debug!("Maple project settings already exist, skipping migration");
    }

    Ok(())
}

async fn perform_maple_settings_migration(
    app_state: &Arc<AppState>,
    maple_id: i32,
    github_client_secret: Option<String>,
    google_client_secret: Option<String>,
    github_client_id: Option<String>,
    google_client_id: Option<String>,
) -> Result<(), Error> {
    let (verification_base_url, oauth_base_url) = match app_state.app_mode {
        AppMode::Local => ("http://127.0.0.1:5173/verify", "http://127.0.0.1:5173"),
        AppMode::Dev => (
            "https://dev.secretgpt.ai/verify",
            "https://dev.secretgpt.ai",
        ),
        AppMode::Preview => (
            "https://opensecret.cloud/verify",
            "https://preview.opensecret.cloud",
        ),
        AppMode::Prod => ("https://trymaple.ai/verify", "https://trymaple.ai"),
        AppMode::Custom(_) => (
            "https://preview.opensecret.cloud/verify",
            "https://preview.opensecret.cloud",
        ),
    };

    let send_from = match app_state.app_mode {
        AppMode::Local => "local@email.trymaple.ai",
        AppMode::Dev => "dev@email.trymaple.ai",
        AppMode::Preview => "preview@email.trymaple.ai",
        AppMode::Prod => "hello@email.trymaple.ai",
        AppMode::Custom(_) => "preview@email.trymaple.ai",
    }
    .to_string();

    if app_state.db.get_project_email_settings(maple_id)?.is_none() {
        let email_settings = EmailSettings {
            provider: "resend".to_string(),
            send_from,
            email_verification_url: verification_base_url.to_string(),
        };

        app_state
            .db
            .update_project_email_settings(maple_id, email_settings)?;

        if let Some(resend_api_key) = &app_state.resend_api_key {
            migrate_project_secret(app_state, maple_id, PROJECT_RESEND_API_KEY, resend_api_key)
                .await?;
            info!("Successfully migrated Resend API key to Maple project secrets");
        } else {
            error!("No Resend API key found during migration");
        }
    }

    if app_state.db.get_project_oauth_settings(maple_id)?.is_none() {
        let oauth_settings = OAuthSettings {
            google_oauth_enabled: google_client_id.is_some() && google_client_secret.is_some(),
            github_oauth_enabled: github_client_id.is_some() && github_client_secret.is_some(),
            apple_oauth_enabled: false,
            google_oauth_settings: google_client_id.map(|client_id| OAuthProviderSettings {
                client_id,
                redirect_url: format!("{}/auth/google/callback", oauth_base_url),
                additional_redirect_urls: None,
            }),
            github_oauth_settings: github_client_id.map(|client_id| OAuthProviderSettings {
                client_id,
                redirect_url: format!("{}/auth/github/callback", oauth_base_url),
                additional_redirect_urls: None,
            }),
            apple_oauth_settings: None,
        };

        app_state
            .db
            .update_project_oauth_settings(maple_id, oauth_settings)?;

        if let Some(secret) = github_client_secret {
            migrate_project_secret(app_state, maple_id, PROJECT_GITHUB_OAUTH_SECRET, &secret)
                .await?;
            info!("Successfully migrated GitHub OAuth secret to Maple project secrets");
        }

        if let Some(secret) = google_client_secret {
            migrate_project_secret(app_state, maple_id, PROJECT_GOOGLE_OAUTH_SECRET, &secret)
                .await?;
            info!("Successfully migrated Google OAuth secret to Maple project secrets");
        }
    }

    info!("Successfully completed Maple project settings migration");
    Ok(())
}

async fn migrate_project_secret(
    app_state: &Arc<AppState>,
    project_id: i32,
    key_name: &str,
    secret_value: &str,
) -> Result<(), Error> {
    let secret_key = SecretKey::from_slice(&app_state.enclave_key)
        .map_err(|e| Error::EncryptionError(e.to_string()))?;
    let encrypted_value =
        crate::encrypt::encrypt_with_key(&secret_key, secret_value.as_bytes()).await;

    let new_secret = NewOrgProjectSecret::new(project_id, key_name.to_string(), encrypted_value);
    app_state.db.create_org_project_secret(new_secret)?;

    Ok(())
}
