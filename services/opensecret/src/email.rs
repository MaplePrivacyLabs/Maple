mod maple_templates;

use crate::AppMode;
use crate::DBError;
use crate::PROJECT_RESEND_API_KEY;
use chrono::{Duration, Utc};
use resend_rs::types::CreateEmailBaseOptions;
use resend_rs::{Resend, Result};
use tracing::error;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum EmailError {
    #[error("Unknown Email error")]
    UnknownError,
    #[error("Resend API key not found")]
    ApiKeyNotFound,
    #[error("Project email settings not found")]
    ProjectSettingsNotFound,
    #[error("Project email settings incomplete")]
    IncompleteSettings,
    #[error("Database error: {0}")]
    DatabaseError(#[from] DBError),
}

async fn get_project_email_settings(
    app_state: &crate::AppState,
    project_id: i32,
) -> Result<(String, String), EmailError> {
    // Get project email settings
    let email_settings = app_state
        .db
        .get_project_email_settings(project_id)?
        .ok_or(EmailError::ProjectSettingsNotFound)?;

    // Verify provider is resend
    if email_settings.provider != "resend" {
        error!("Unsupported email provider: {}", email_settings.provider);
        return Err(EmailError::IncompleteSettings);
    }

    // Verify send_from is set
    if email_settings.send_from.is_empty() {
        error!("Project send_from email not configured");
        return Err(EmailError::IncompleteSettings);
    }

    // Get project's Resend API key
    let secret = app_state
        .db
        .get_org_project_secret_by_key_name_and_project(PROJECT_RESEND_API_KEY, project_id)?
        .ok_or(EmailError::ApiKeyNotFound)?;

    // Decrypt the API key
    let secret_key = secp256k1::SecretKey::from_slice(&app_state.enclave_key)
        .map_err(|_| EmailError::UnknownError)?;
    let api_key = String::from_utf8(
        crate::encrypt::decrypt_with_key(&secret_key, &secret.secret_enc)
            .map_err(|_| EmailError::UnknownError)?,
    )
    .map_err(|_| EmailError::UnknownError)?;

    Ok((api_key, email_settings.send_from))
}

// TODO remove the send email and do it outside of the enclave
pub async fn send_hello_email(
    app_state: &crate::AppState,
    project_id: i32,
    to_email: String,
) -> Result<(), EmailError> {
    // Get project name
    let project = app_state
        .db
        .get_org_project_by_id(project_id)
        .map_err(|e| {
            error!("Failed to get project: {}", e);
            EmailError::UnknownError
        })?;

    // Only send welcome email for Maple project for now. It has no team
    // sign-off, so it needs no organization name.
    let Some(message) = account_message(AccountEmail::Welcome, &project.name, "")? else {
        tracing::debug!("Skipping welcome email for non-Maple project");
        return Ok(());
    };

    tracing::debug!("Sending maple hello email");

    let (api_key, from_email) = get_project_email_settings(app_state, project_id).await?;
    let resend = Resend::new(&api_key);

    let to = [to_email];

    // Schedule the email to be sent 5 minutes from now
    let scheduled_time = Utc::now() + Duration::minutes(5);
    let scheduled_at = scheduled_time.to_rfc3339();

    let email =
        CreateEmailBaseOptions::new(sender(&project.name, &from_email), to, message.subject)
            .with_html(&message.html)
            .with_scheduled_at(&scheduled_at);

    let email = with_account_reply_to(email, &project.name);
    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

/// Pair the project name with its sending address so inboxes show "Maple"
/// rather than the address's local part ("hello"). The project settings only
/// store a bare address, so the display name is added here.
fn sender(project_name: &str, address: &str) -> String {
    // Keep only characters that are safe unquoted in a From display name.
    let name: String = project_name
        .chars()
        .filter(|c| c.is_alphanumeric() || " !#$%&'*+-/=?^_`{|}~".contains(*c))
        .collect();
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        address.to_string()
    } else {
        format!("{name} <{address}>")
    }
}

fn is_maple_project(project_name: &str) -> bool {
    project_name == "Maple"
}

/// Maple's project lives in the OpenSecret org, so the org name is the wrong
/// sign-off for Maple account mail. Other projects keep the organization name.
fn account_team_name(project_name: &str, org_name: &str) -> String {
    if is_maple_project(project_name) {
        project_name.to_string()
    } else {
        org_name.to_string()
    }
}

fn account_support_email(project_name: &str) -> &'static str {
    if is_maple_project(project_name) {
        "support@trymaple.ai"
    } else {
        "support@opensecret.cloud"
    }
}

/// Maple sends from email.trymaple.ai, which has no inbox, so a reply to that
/// address bounces. Point replies at support instead. Other projects keep
/// whatever their own sending address does with replies.
fn account_reply_to(project_name: &str) -> Option<&'static str> {
    is_maple_project(project_name).then_some(account_support_email(project_name))
}

fn with_account_reply_to(
    email: CreateEmailBaseOptions,
    project_name: &str,
) -> CreateEmailBaseOptions {
    match account_reply_to(project_name) {
        Some(address) => email.with_reply(address),
        None => email,
    }
}

/// An account email for one of a project's users, with the values it shows.
#[derive(Clone, Copy)]
enum AccountEmail<'a> {
    Welcome,
    Verification { url: &'a str, code: &'a str },
    PasswordReset { code: &'a str },
    PasswordResetConfirmation,
    AccountDeletion { code: &'a str },
    AccountDeletionConfirmation,
}

struct AccountMessage {
    subject: String,
    html: String,
}

/// Build a project's account email, or `None` when the project doesn't send
/// it. Maple gets the redesigned templates. Every other project keeps its
/// original subject and HTML, and gets no welcome email. Only the email that
/// will be sent is rendered. Kept free of I/O so tests cover this switch.
fn account_message(
    email: AccountEmail<'_>,
    project_name: &str,
    org_name: &str,
) -> Result<Option<AccountMessage>, EmailError> {
    let team_name = account_team_name(project_name, org_name);
    let support_email = account_support_email(project_name);

    if is_maple_project(project_name) {
        return maple_templates::render(email, project_name, &team_name, support_email)
            .map(Some)
            .map_err(|e| {
                error!("Failed to render Maple account email: {:?}", e);
                EmailError::UnknownError
            });
    }

    let (subject, html) = match email {
        AccountEmail::Welcome => return Ok(None),
        AccountEmail::Verification { url, code } => (
            format!("Verify Your {} Account", project_name),
            legacy_verification_html(project_name, &team_name, url, code),
        ),
        AccountEmail::PasswordReset { code } => (
            format!("Reset Your {} Password", project_name),
            legacy_password_reset_html(project_name, &team_name, code),
        ),
        AccountEmail::PasswordResetConfirmation => (
            format!("Your {} Password Has Been Reset", project_name),
            legacy_password_reset_confirmation_html(project_name, &team_name, support_email),
        ),
        AccountEmail::AccountDeletion { code } => (
            format!("Account Deletion Request for Your {} Account", project_name),
            legacy_account_deletion_html(project_name, &team_name, code),
        ),
        AccountEmail::AccountDeletionConfirmation => (
            format!("Your {} Account Has Been Deleted", project_name),
            legacy_account_deletion_confirmation_html(project_name, &team_name, support_email),
        ),
    };
    Ok(Some(AccountMessage { subject, html }))
}

// The legacy templates below are the original account emails, still sent by
// every project except Maple. Each keeps a blank line where the Maple logo
// used to go, so their HTML stays byte-identical to before.

fn legacy_verification_html(
    project_name: &str,
    team_name: &str,
    verification_url: &str,
    verification_code: &str,
) -> String {
    format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Verify Your {} Account</title>
            <style>
                body {{ font-family: ui-sans-serif,system-ui,sans-serif; }}
                .container {{ max-width: 600px; margin: 0 auto; padding: 20px; }}
                h1, h2, h3 {{ font-weight: 300; }}
                .button {{ display: inline-block; padding: 10px 20px; background-color: black; color: #ffffff; text-decoration: none; border-radius: 5px; }}
                .code {{ background-color: rgba(1,1,1,0.05); padding: 10px; border-radius: 5px; font-family: monospace; font-size: 16px; }}
            </style>
        </head>
        <body>
            <div class="container">
                
                <h1>Welcome to {}!</h1>
                <p>Thank you for registering. To complete your account setup, please verify your email address by clicking the button below:</p>
                <p>
                    <a href="{}" class="button">Verify Your Email</a>
                </p>
                <p>If the button doesn't work, you can copy and paste the following link into your browser:</p>
                <p>{}</p>
                <p>Alternatively, you can use the following verification code:</p>
                <p class="code">{}</p>
                <p>This verification link and code will expire in 24 hours.</p>
                <p>If you didn't create an account with {}, please ignore this email.</p>
                <p>Best regards,<br>The {} Team</p>
            </div>
        </body>
        </html>
        "#,
        project_name,
        project_name,
        verification_url,
        verification_url,
        verification_code,
        project_name,
        team_name
    )
}

fn legacy_password_reset_html(
    project_name: &str,
    team_name: &str,
    alphanumeric_code: &str,
) -> String {
    format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Reset Your {} Password</title>
            <style>
                body {{ font-family: ui-sans-serif,system-ui,sans-serif; }}
                .container {{ max-width: 600px; margin: 0 auto; padding: 20px; }}
                h1, h2, h3 {{ font-weight: 300; }}
                .code {{ background-color: rgba(1,1,1,0.05); padding: 10px; border-radius: 5px; font-family: monospace; font-size: 16px; }}
            </style>
        </head>
        <body>
            <div class="container">
                
                <h1>Reset Your {} Password</h1>
                <p>We received a request to reset your {} account password. If you didn't make this request, you can ignore this email.</p>
                <p>To reset your password, use the following code:</p>
                <p class="code">{}</p>
                <p>This code will expire in 24 hours.</p>
                <p>If you have any issues, please contact our support team.</p>
                <p>Best regards,<br>The {} Team</p>
            </div>
        </body>
        </html>
        "#,
        project_name, project_name, project_name, alphanumeric_code, team_name
    )
}

fn legacy_password_reset_confirmation_html(
    project_name: &str,
    team_name: &str,
    support_email: &str,
) -> String {
    format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Password Reset Confirmation</title>
            <style>
                body {{ font-family: ui-sans-serif,system-ui,sans-serif; }}
                .container {{ max-width: 600px; margin: 0 auto; padding: 20px; }}
                h1, h2, h3 {{ font-weight: 300; }}
            </style>
        </head>
        <body>
            <div class="container">
                
                <h1>Password Reset Confirmation</h1>
                <p>Your {} account password has been successfully reset.</p>
                <p>If you did not initiate this password reset, please contact us immediately at <a href="mailto:{}">{}</a>.</p>
                <p>For security reasons, we recommend that you:</p>
                <ul>
                    <li>Change your password again if you suspect any unauthorized access.</li>
                    <li>Review your account activity for any suspicious actions.</li>
                </ul>
                <p>If you have any questions or concerns, please don't hesitate to reach out to our support team.</p>
                <p>Best regards,<br>The {} Team</p>
            </div>
        </body>
        </html>
        "#,
        project_name, support_email, support_email, team_name
    )
}

fn legacy_account_deletion_html(
    project_name: &str,
    team_name: &str,
    confirmation_code: &str,
) -> String {
    format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Account Deletion Request</title>
            <style>
                body {{ font-family: ui-sans-serif,system-ui,sans-serif; }}
                .container {{ max-width: 600px; margin: 0 auto; padding: 20px; }}
                h1, h2, h3 {{ font-weight: 300; }}
                .code {{ background-color: rgba(1,1,1,0.05); padding: 10px; border-radius: 5px; font-family: monospace; font-size: 16px; }}
                .warning {{ color: #e74c3c; }}
            </style>
        </head>
        <body>
            <div class="container">
                
                <h1>Account Deletion Request</h1>
                <p>We received a request to delete your {} account. <span class="warning">This action is permanent and cannot be undone.</span></p>
                <p>To confirm your account deletion, use the following confirmation code:</p>
                <p class="code">{}</p>
                <p>This confirmation code will expire in 24 hours.</p>
                <p>If you did not request this account deletion, please ignore this email, and your account will remain active. If you have any concerns about account security, please contact our support team.</p>
                <p>Best regards,<br>The {} Team</p>
            </div>
        </body>
        </html>
        "#,
        project_name, confirmation_code, team_name
    )
}

fn legacy_account_deletion_confirmation_html(
    project_name: &str,
    team_name: &str,
    support_email: &str,
) -> String {
    format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Account Deletion Confirmation</title>
            <style>
                body {{ font-family: ui-sans-serif,system-ui,sans-serif; }}
                .container {{ max-width: 600px; margin: 0 auto; padding: 20px; }}
                h1, h2, h3 {{ font-weight: 300; }}
            </style>
        </head>
        <body>
            <div class="container">
                
                <h1>Account Deletion Confirmation</h1>
                <p>Your {} account has been successfully deleted along with all associated data.</p>
                <p>If you did not request this account deletion, please contact us immediately at <a href="mailto:{}">{}</a>.</p>
                <p>Thank you for your time with us. We hope to see you again in the future.</p>
                <p>Best regards,<br>The {} Team</p>
            </div>
        </body>
        </html>
        "#,
        project_name, support_email, support_email, team_name
    )
}

pub async fn send_verification_email(
    app_state: &crate::AppState,
    project_id: i32,
    to_email: String,
    verification_code: uuid::Uuid,
) -> Result<(), EmailError> {
    let (api_key, from_email) = get_project_email_settings(app_state, project_id).await?;
    let resend = Resend::new(&api_key);

    // Get project name and email settings
    let project = app_state
        .db
        .get_org_project_by_id(project_id)
        .map_err(|e| {
            error!("Failed to get project: {}", e);
            EmailError::UnknownError
        })?;

    // Get organization name for the team signature
    let org = app_state.db.get_org_by_id(project.org_id).map_err(|e| {
        error!("Failed to get organization: {}", e);
        EmailError::UnknownError
    })?;

    let email_settings = app_state
        .db
        .get_project_email_settings(project_id)?
        .ok_or(EmailError::ProjectSettingsNotFound)?;

    let to = [to_email];

    // Ensure base URL has exactly one trailing slash
    let base_url = email_settings.email_verification_url.trim_end_matches('/');
    let verification_url = format!("{}/{}", base_url, verification_code);
    let verification_code = verification_code.to_string();

    let Some(message) = account_message(
        AccountEmail::Verification {
            url: &verification_url,
            code: &verification_code,
        },
        &project.name,
        &org.name,
    )?
    else {
        return Ok(());
    };

    let email =
        CreateEmailBaseOptions::new(sender(&project.name, &from_email), to, message.subject)
            .with_html(&message.html);

    let email = with_account_reply_to(email, &project.name);
    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

pub async fn send_password_reset_email(
    app_state: &crate::AppState,
    project_id: i32,
    to_email: String,
    alphanumeric_code: String,
) -> Result<(), EmailError> {
    let (api_key, from_email) = get_project_email_settings(app_state, project_id).await?;
    let resend = Resend::new(&api_key);

    // Get project name
    let project = app_state
        .db
        .get_org_project_by_id(project_id)
        .map_err(|e| {
            error!("Failed to get project: {}", e);
            EmailError::UnknownError
        })?;

    // Get organization name for the team signature
    let org = app_state.db.get_org_by_id(project.org_id).map_err(|e| {
        error!("Failed to get organization: {}", e);
        EmailError::UnknownError
    })?;

    let to = [to_email];

    let Some(message) = account_message(
        AccountEmail::PasswordReset {
            code: &alphanumeric_code,
        },
        &project.name,
        &org.name,
    )?
    else {
        return Ok(());
    };

    let email =
        CreateEmailBaseOptions::new(sender(&project.name, &from_email), to, message.subject)
            .with_html(&message.html);

    let email = with_account_reply_to(email, &project.name);
    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

pub async fn send_password_reset_confirmation_email(
    app_state: &crate::AppState,
    project_id: i32,
    to_email: String,
) -> Result<(), EmailError> {
    let (api_key, from_email) = get_project_email_settings(app_state, project_id).await?;
    let resend = Resend::new(&api_key);

    // Get project name
    let project = app_state
        .db
        .get_org_project_by_id(project_id)
        .map_err(|e| {
            error!("Failed to get project: {}", e);
            EmailError::UnknownError
        })?;

    // Get organization name for the team signature
    let org = app_state.db.get_org_by_id(project.org_id).map_err(|e| {
        error!("Failed to get organization: {}", e);
        EmailError::UnknownError
    })?;

    let to = [to_email];

    let Some(message) = account_message(
        AccountEmail::PasswordResetConfirmation,
        &project.name,
        &org.name,
    )?
    else {
        return Ok(());
    };

    let email =
        CreateEmailBaseOptions::new(sender(&project.name, &from_email), to, message.subject)
            .with_html(&message.html);

    let email = with_account_reply_to(email, &project.name);
    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

pub async fn send_platform_verification_email(
    app_state: &crate::AppState,
    resend_api_key: Option<String>,
    to_email: String,
    verification_code: uuid::Uuid,
) -> Result<(), EmailError> {
    if resend_api_key.is_none() {
        return Err(EmailError::ApiKeyNotFound);
    }
    let api_key = resend_api_key.expect("just checked");

    let resend = Resend::new(&api_key);

    let to = [to_email];
    let from_email = from_opensecret_email(app_state.app_mode.clone());
    let subject = "Verify Your OpenSecret Account";

    let base_url = match app_state.app_mode {
        AppMode::Local => "http://localhost:5173",
        AppMode::Dev => "https://dev.opensecret.cloud",
        AppMode::Preview => "https://preview.opensecret.cloud",
        AppMode::Prod => "https://app.opensecret.cloud",
        AppMode::Custom(_) => "https://preview.opensecret.cloud",
    };

    let verification_url = format!("{}/verify/{}", base_url, verification_code);

    let html_content = format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Verify Your OpenSecret Account</title>
            <style>
                body {{ font-family: ui-sans-serif,system-ui,sans-serif; }}
                .container {{ max-width: 600px; margin: 0 auto; padding: 20px; }}
                h1, h2, h3 {{ font-weight: 300; }}
                .button {{ display: inline-block; padding: 10px 20px; background-color: black; color: #ffffff; text-decoration: none; border-radius: 5px; }}
                .code {{ background-color: rgba(1,1,1,0.05); padding: 10px; border-radius: 5px; font-family: monospace; font-size: 16px; }}
            </style>
        </head>
        <body>
            <div class="container">
                <h1>Welcome to OpenSecret!</h1>
                <p>Thank you for registering. To complete your account setup, please verify your email address by clicking the button below:</p>
                <p>
                    <a href="{}" class="button">Verify Your Email</a>
                </p>
                <p>If the button doesn't work, you can copy and paste the following link into your browser:</p>
                <p>{}</p>
                <p>Alternatively, you can use the following verification code:</p>
                <p class="code">{}</p>
                <p>This verification link and code will expire in 24 hours.</p>
                <p>If you didn't create an account with OpenSecret, please ignore this email.</p>
                <p>Best regards,<br>The OpenSecret Team</p>
            </div>
        </body>
        </html>
        "#,
        verification_url, verification_url, verification_code
    );

    let email = CreateEmailBaseOptions::new(from_email, to, subject).with_html(&html_content);

    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

pub async fn send_platform_invite_email(
    app_mode: AppMode,
    resend_api_key: Option<String>,
    to_email: String,
    organization_name: String,
    invite_code: Uuid,
    org_id: Uuid,
) -> Result<(), EmailError> {
    if resend_api_key.is_none() {
        return Err(EmailError::ApiKeyNotFound);
    }
    let api_key = resend_api_key.expect("just checked");

    let resend = Resend::new(&api_key);

    let from = from_opensecret_email(app_mode.clone());
    let to = [to_email];
    let subject = "You've Been Invited to Join an Organization on OpenSecret";

    let base_url = match app_mode {
        AppMode::Local => "http://localhost:5173",
        AppMode::Dev => "https://dev.opensecret.cloud",
        AppMode::Preview => "https://preview.opensecret.cloud",
        AppMode::Prod => "https://app.opensecret.cloud",
        AppMode::Custom(_) => "https://preview.opensecret.cloud",
    };

    let invite_url = format!("{}/invite/orgs/{}/code/{}", base_url, org_id, invite_code);

    let html_content = format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Organization Invitation - OpenSecret</title>
            <style>
                body {{ font-family: ui-sans-serif,system-ui,sans-serif; }}
                .container {{ max-width: 600px; margin: 0 auto; padding: 20px; }}
                h1, h2, h3 {{ font-weight: 300; }}
                .button {{ display: inline-block; padding: 10px 20px; background-color: black; color: #ffffff; text-decoration: none; border-radius: 5px; }}
                .code {{ background-color: rgba(1,1,1,0.05); padding: 10px; border-radius: 5px; font-family: monospace; font-size: 16px; }}
            </style>
        </head>
        <body>
            <div class="container">
                <h1>You've Been Invited!</h1>
                <p>You've been invited to join the {} organization on OpenSecret. To accept this invitation, please click the button below:</p>
                <p>
                    <a href="{}" class="button">Accept Invitation</a>
                </p>
                <p>If the button doesn't work, you can copy and paste the following link into your browser:</p>
                <p>{}</p>
                <p>Alternatively, you can use the following invitation code:</p>
                <p class="code">{}</p>
                <p>This invitation link and code will expire in 24 hours.</p>
                <p>If you weren't expecting this invitation, you can safely ignore this email.</p>
                <p>Best regards,<br>The OpenSecret Team</p>
            </div>
        </body>
        </html>
        "#,
        organization_name, invite_url, invite_url, invite_code
    );

    let email = CreateEmailBaseOptions::new(from, to, subject).with_html(&html_content);

    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

fn from_opensecret_email(app_mode: AppMode) -> String {
    match app_mode {
        AppMode::Local => "local@email.opensecret.cloud".to_string(),
        AppMode::Dev => "dev@email.opensecret.cloud".to_string(),
        AppMode::Preview => "preview@email.opensecret.cloud".to_string(),
        AppMode::Prod => "hello@email.opensecret.cloud".to_string(),
        AppMode::Custom(_) => "preview@email.opensecret.cloud".to_string(),
    }
}

pub async fn send_platform_password_reset_email(
    app_state: &crate::AppState,
    resend_api_key: Option<String>,
    to_email: String,
    alphanumeric_code: String,
) -> Result<(), EmailError> {
    if resend_api_key.is_none() {
        return Err(EmailError::ApiKeyNotFound);
    }
    let api_key = resend_api_key.expect("just checked");

    let resend = Resend::new(&api_key);

    let to = [to_email];
    let from_email = from_opensecret_email(app_state.app_mode.clone());
    let subject = "Reset Your OpenSecret Platform Password";

    let base_url = match app_state.app_mode {
        AppMode::Local => "http://localhost:5173",
        AppMode::Dev => "https://dev.opensecret.cloud",
        AppMode::Preview => "https://preview.opensecret.cloud",
        AppMode::Prod => "https://app.opensecret.cloud",
        AppMode::Custom(_) => "https://preview.opensecret.cloud",
    };

    let reset_url = format!(
        "{}/platform/reset-password?code={}",
        base_url, alphanumeric_code
    );

    let html_content = format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Reset Your OpenSecret Platform Password</title>
            <style>
                body {{ font-family: ui-sans-serif,system-ui,sans-serif; }}
                .container {{ max-width: 600px; margin: 0 auto; padding: 20px; }}
                h1, h2, h3 {{ font-weight: 300; }}
                .button {{ display: inline-block; padding: 10px 20px; background-color: black; color: #ffffff; text-decoration: none; border-radius: 5px; }}
                .code {{ background-color: rgba(1,1,1,0.05); padding: 10px; border-radius: 5px; font-family: monospace; font-size: 16px; }}
            </style>
        </head>
        <body>
            <div class="container">
                <h1>Password Reset Request</h1>
                <p>You recently requested to reset your password for your OpenSecret Platform account. Use the code below to complete the process:</p>
                <p class="code">{}</p>
                <p>Alternatively, you can click the button below to continue:</p>
                <p>
                    <a href="{}" class="button">Reset Password</a>
                </p>
                <p>If you did not request a password reset, please ignore this email or contact support if you have questions.</p>
                <p>This password reset link and code will expire in 24 hours.</p>
                <p>Best regards,<br>The OpenSecret Team</p>
            </div>
        </body>
        </html>
        "#,
        alphanumeric_code, reset_url
    );

    let email = CreateEmailBaseOptions::new(from_email, to, subject).with_html(&html_content);

    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

pub async fn send_platform_password_reset_confirmation_email(
    app_state: &crate::AppState,
    resend_api_key: Option<String>,
    to_email: String,
) -> Result<(), EmailError> {
    if resend_api_key.is_none() {
        return Err(EmailError::ApiKeyNotFound);
    }
    let api_key = resend_api_key.expect("just checked");

    let resend = Resend::new(&api_key);

    let to = [to_email];
    let from_email = from_opensecret_email(app_state.app_mode.clone());
    let subject = "Your OpenSecret Platform Password Has Been Reset";

    let html_content = r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Password Reset Confirmation</title>
            <style>
                body { font-family: ui-sans-serif,system-ui,sans-serif; }
                .container { max-width: 600px; margin: 0 auto; padding: 20px; }
                h1, h2, h3 { font-weight: 300; }
            </style>
        </head>
        <body>
            <div class="container">
                <h1>Password Reset Confirmation</h1>
                <p>Your OpenSecret Platform account password has been successfully reset.</p>
                <p>If you did not initiate this password reset, please contact us immediately at <a href="mailto:support@opensecret.cloud">support@opensecret.cloud</a>.</p>
                <p>For security reasons, we recommend that you:</p>
                <ul>
                    <li>Change your password again if you suspect any unauthorized access.</li>
                    <li>Review your account activity for any suspicious actions.</li>
                </ul>
                <p>If you have any questions or concerns, please don't hesitate to reach out to our support team.</p>
                <p>Best regards,<br>The OpenSecret Team</p>
            </div>
        </body>
        </html>
        "#.to_string();

    let email = CreateEmailBaseOptions::new(from_email, to, subject).with_html(&html_content);

    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

pub async fn send_account_deletion_email(
    app_state: &crate::AppState,
    project_id: i32,
    to_email: String,
    confirmation_code: String,
) -> Result<(), EmailError> {
    let (api_key, from_email) = get_project_email_settings(app_state, project_id).await?;
    let resend = Resend::new(&api_key);

    // Get project name
    let project = app_state
        .db
        .get_org_project_by_id(project_id)
        .map_err(|e| {
            error!("Failed to get project: {}", e);
            EmailError::UnknownError
        })?;

    // Get organization name for the team signature
    let org = app_state.db.get_org_by_id(project.org_id).map_err(|e| {
        error!("Failed to get organization: {}", e);
        EmailError::UnknownError
    })?;

    let to = [to_email];

    let Some(message) = account_message(
        AccountEmail::AccountDeletion {
            code: &confirmation_code,
        },
        &project.name,
        &org.name,
    )?
    else {
        return Ok(());
    };

    let email =
        CreateEmailBaseOptions::new(sender(&project.name, &from_email), to, message.subject)
            .with_html(&message.html);

    let email = with_account_reply_to(email, &project.name);
    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

pub async fn send_account_deletion_confirmation_email(
    app_state: &crate::AppState,
    project_id: i32,
    to_email: String,
) -> Result<(), EmailError> {
    let (api_key, from_email) = get_project_email_settings(app_state, project_id).await?;
    let resend = Resend::new(&api_key);

    // Get project name
    let project = app_state
        .db
        .get_org_project_by_id(project_id)
        .map_err(|e| {
            error!("Failed to get project: {}", e);
            EmailError::UnknownError
        })?;

    // Get organization name for the team signature
    let org = app_state.db.get_org_by_id(project.org_id).map_err(|e| {
        error!("Failed to get organization: {}", e);
        EmailError::UnknownError
    })?;

    let to = [to_email];

    let Some(message) = account_message(
        AccountEmail::AccountDeletionConfirmation,
        &project.name,
        &org.name,
    )?
    else {
        return Ok(());
    };

    let email =
        CreateEmailBaseOptions::new(sender(&project.name, &from_email), to, message.subject)
            .with_html(&message.html);

    let email = with_account_reply_to(email, &project.name);
    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        account_message, account_reply_to, maple_templates, sender, with_account_reply_to,
        AccountEmail, AccountMessage,
    };
    use resend_rs::types::CreateEmailBaseOptions;

    const VERIFY_CODE: &str = "3f6c2d1e-8b4a-4c7e-9f21-5a0d7b6e4c19";
    const VERIFY_URL: &str =
        "https://trymaple.ai/verify-email/3f6c2d1e-8b4a-4c7e-9f21-5a0d7b6e4c19";
    const RESET_CODE: &str = "7KQ2M9XA";
    const DELETE_CODE: &str = "b81e4f0a-2c6d-4a93-8e57-d1f3a9c20b64";

    /// What an account email should be for Maple and for any other project.
    struct Expected {
        maple_subject: &'static str,
        maple_campaign: &'static str,
        /// The subject and pre-redesign HTML other projects get, or `None`
        /// when they don't send this email.
        other: Option<(&'static str, &'static str)>,
    }

    /// Every account email, with the values the snapshots were taken with.
    /// The match is exhaustive, so a new `AccountEmail` variant fails to
    /// compile here until it has a sample and expectations.
    fn every_account_email() -> Vec<(AccountEmail<'static>, Expected)> {
        [
            AccountEmail::Welcome,
            AccountEmail::Verification {
                url: VERIFY_URL,
                code: VERIFY_CODE,
            },
            AccountEmail::PasswordReset { code: RESET_CODE },
            AccountEmail::PasswordResetConfirmation,
            AccountEmail::AccountDeletion { code: DELETE_CODE },
            AccountEmail::AccountDeletionConfirmation,
        ]
        .into_iter()
        .map(|email| {
            let expected = match email {
                AccountEmail::Welcome => Expected {
                    maple_subject: "Welcome to Maple",
                    maple_campaign: "welcome",
                    other: None,
                },
                AccountEmail::Verification { .. } => Expected {
                    maple_subject: "Verify your Maple account",
                    maple_campaign: "verify-email",
                    other: Some((
                        "Verify Your Other Account",
                        include_str!("email/non_maple_snapshots/verification.json"),
                    )),
                },
                AccountEmail::PasswordReset { .. } => Expected {
                    maple_subject: "Reset your Maple password",
                    maple_campaign: "password-reset",
                    other: Some((
                        "Reset Your Other Password",
                        include_str!("email/non_maple_snapshots/password_reset.json"),
                    )),
                },
                AccountEmail::PasswordResetConfirmation => Expected {
                    maple_subject: "Your Maple password was changed",
                    maple_campaign: "password-changed",
                    other: Some((
                        "Your Other Password Has Been Reset",
                        include_str!("email/non_maple_snapshots/password_reset_confirmation.json"),
                    )),
                },
                AccountEmail::AccountDeletion { .. } => Expected {
                    maple_subject: "Confirm your Maple account deletion",
                    maple_campaign: "delete-account",
                    other: Some((
                        "Account Deletion Request for Your Other Account",
                        include_str!("email/non_maple_snapshots/account_deletion.json"),
                    )),
                },
                AccountEmail::AccountDeletionConfirmation => Expected {
                    maple_subject: "Your Maple account has been deleted",
                    maple_campaign: "account-deleted",
                    other: Some((
                        "Your Other Account Has Been Deleted",
                        include_str!(
                            "email/non_maple_snapshots/account_deletion_confirmation.json"
                        ),
                    )),
                },
            };
            (email, expected)
        })
        .collect()
    }

    fn maple_message(email: AccountEmail<'_>) -> AccountMessage {
        account_message(email, "Maple", "OpenSecret")
            .expect("Maple account email renders")
            .expect("Maple sends every account email")
    }

    #[test]
    fn maple_projects_get_the_redesigned_emails() {
        assert_eq!(account_reply_to("Maple"), Some("support@trymaple.ai"));
        for (email, expected) in every_account_email() {
            let message = maple_message(email);
            assert_eq!(message.subject, expected.maple_subject);
            let html = &message.html;
            assert!(html.starts_with("<!DOCTYPE html>"));
            assert!(html.contains(&format!("{}wordmark.png", maple_templates::ASSET_BASE)));
            assert!(html.contains("support@trymaple.ai"));
            assert!(!html.contains("OpenSecret") && !html.contains("opensecret.cloud"));
            assert!(!html.contains("{{") && !html.contains("}}"));
            if !matches!(email, AccountEmail::Welcome) {
                assert!(html.contains("The Maple team"), "{}", message.subject);
            }
        }
    }

    #[test]
    fn other_projects_keep_their_original_emails() {
        assert_eq!(sender("Other", "hi@acme.test"), "Other <hi@acme.test>");
        assert_eq!(account_reply_to("Other"), None);
        for (email, expected) in every_account_email() {
            let message =
                account_message(email, "Other", "Acme Org").expect("original email renders");
            match expected.other {
                None => assert!(message.is_none()),
                Some((subject, snapshot)) => {
                    let message = message.expect("other projects send this email");
                    assert_eq!(message.subject, subject);
                    assert_eq!(
                        message.html,
                        serde_json::from_str::<String>(snapshot).unwrap()
                    );
                }
            }
        }
    }

    #[test]
    fn maple_email_links_images_and_font() {
        let welcome = maple_message(AccountEmail::Welcome).html;
        assert!(welcome.contains("href=\"https://app.maple.ai\""));
        assert!(welcome.contains("<a href=\"https://maple.ai\"><img"));
        for href in [
            "https://maple.ai/pricing?utm_source=email&amp;utm_medium=email&amp;utm_campaign=welcome&amp;utm_content=see-plans",
            "https://maple.ai/downloads?utm_source=email&amp;utm_medium=email&amp;utm_campaign=welcome&amp;utm_content=download-app",
            "https://maple.ai/research?utm_source=email&amp;utm_medium=email&amp;utm_campaign=welcome&amp;utm_content=details",
            "https://maple.ai/docs/proxy?utm_source=email&amp;utm_medium=email&amp;utm_campaign=welcome&amp;utm_content=proxy-docs",
        ] {
            assert!(welcome.contains(&format!("href=\"{href}\"")), "{href}");
        }
        assert!(welcome.contains("href=\"https://github.com/MaplePrivacyLabs/Maple\""));
        assert!(!welcome.contains("research#download"));
        assert!(!welcome.contains("research#pricing"));
        assert!(!welcome.contains("OpenSecretCloud/Maple"));
        assert!(welcome.contains("class=\"ink\" src=\"https://img.maple.ai/email/research-laptop.jpg\" width=\"536\" alt=\"Maple Research open on a laptop\""));
        assert!(welcome.contains("app-icon.png\" width=\"48\" height=\"48\" alt=\"\""));
        assert!(welcome.contains("footer-watermark-light.png\" width=\"600\" alt=\"\""));
        assert!(welcome.contains("footer-watermark-dark.png\" width=\"600\" alt=\"\""));
        assert!(welcome.contains("601 Congress Ave, Suite 250, Austin, TX 78701"));
        assert!(welcome.contains("https://img.maple.ai/email/tile-welcome.png"));
        assert!(!welcome.contains("tracking"));

        for (email, expected) in every_account_email() {
            let html = maple_message(email).html;
            let campaign = expected.maple_campaign;
            for (url, content) in [
                ("https://maple.ai", "footer-home"),
                ("https://app.maple.ai/privacy", "footer-privacy"),
            ] {
                let href = format!(
                    "href=\"{url}?utm_source=email&amp;utm_medium=email&amp;utm_campaign={campaign}&amp;utm_content={content}\""
                );
                assert!(html.contains(&href), "{href}");
            }
            let tagged = if matches!(email, AccountEmail::Welcome) {
                6
            } else {
                2
            };
            assert_eq!(html.matches("utm_campaign=").count(), tagged);
            assert!(!html.contains("mailto:support@trymaple.ai?utm_"));
            assert!(!html.contains("x.com/trymapleai?utm_"));
            assert!(!html.contains("MaplePrivacyLabs/Maple?utm_"));
            assert!(!html.contains(&format!("{VERIFY_URL}?utm_")));

            // Images and the font load only from Maple's asset host.
            assert!(!html.contains("googleapis") && !html.contains("gstatic"));
            assert!(html.contains(&format!(
                "src: url('{}manrope-variable.woff2') format('woff2')",
                maple_templates::ASSET_BASE
            )));
            for src in html.split("src=\"").skip(1) {
                assert!(src.starts_with(maple_templates::ASSET_BASE), "{src}");
            }
        }
    }

    #[test]
    fn codes_stay_out_of_subjects_and_preview_text() {
        // Inboxes, lock screens and watches show the subject and the hidden
        // preview text, so a code may only appear in the message itself.
        for (email, code) in [
            (AccountEmail::PasswordReset { code: RESET_CODE }, RESET_CODE),
            (
                AccountEmail::AccountDeletion { code: DELETE_CODE },
                DELETE_CODE,
            ),
        ] {
            let message = maple_message(email);
            assert!(!message.subject.contains(code));
            assert_eq!(message.html.matches(code).count(), 1, "{code}");
        }
        // The verification code is also part of the verification link.
        let message = maple_message(AccountEmail::Verification {
            url: VERIFY_URL,
            code: VERIFY_CODE,
        });
        assert!(!message.subject.contains(VERIFY_CODE));
        assert_eq!(
            message.html.matches(VERIFY_CODE).count(),
            1 + message.html.matches(VERIFY_URL).count()
        );
    }

    #[test]
    fn account_mail_keeps_sender_reply_to_and_html_payload() {
        let maple = maple_message(AccountEmail::Welcome);
        let email = CreateEmailBaseOptions::new(
            sender("Maple", "hello@email.trymaple.ai"),
            ["you@example.com"],
            maple.subject,
        )
        .with_html(&maple.html)
        .with_scheduled_at("2026-10-08T19:15:00Z");
        let payload = serde_json::to_value(with_account_reply_to(email, "Maple")).unwrap();
        assert_eq!(payload["from"], "Maple <hello@email.trymaple.ai>");
        assert_eq!(payload["to"], serde_json::json!(["you@example.com"]));
        assert_eq!(
            payload["reply_to"],
            serde_json::json!(["support@trymaple.ai"])
        );
        assert!(payload["html"]
            .as_str()
            .unwrap()
            .starts_with("<!DOCTYPE html>"));
        assert_eq!(payload["scheduled_at"], "2026-10-08T19:15:00Z");
        assert!(payload.get("text").is_none());
    }

    #[test]
    fn maple_html_escapes_dynamic_values_in_text_and_attributes() {
        let hostile = "<Org \"x\" & '{{TEAM}}'>";
        let url = "https://example.test/path?x=\"<>&";
        let msg = maple_templates::render(
            AccountEmail::Verification { url, code: hostile },
            hostile,
            hostile,
            "help@example.test",
        )
        .unwrap();
        assert!(msg
            .html
            .contains("&lt;Org &quot;x&quot; &amp; &#x27;{{TEAM}}&#x27;&gt;"));
        assert!(msg
            .html
            .contains("href=\"https://example.test/path?x=&quot;&lt;&gt;&amp;\""));
        assert!(!msg.html.contains(hostile));
        assert!(!msg.html.contains(url));
        assert!(!msg.html.contains("<Org"));
    }

    #[test]
    fn project_mail_is_sent_under_the_project_name() {
        assert_eq!(
            sender("Maple", "hello@email.trymaple.ai"),
            "Maple <hello@email.trymaple.ai>"
        );
        assert_eq!(
            sender("Acme, Inc.", "hi@acme.test"),
            "Acme Inc <hi@acme.test>"
        );
        assert_eq!(
            sender("Evil\r\nBcc: x@y.z <a>", "hi@acme.test"),
            "EvilBcc xyz a <hi@acme.test>"
        );
        assert_eq!(sender("<>", "hi@acme.test"), "hi@acme.test");
    }
}
