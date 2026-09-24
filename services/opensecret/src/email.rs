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

const WELCOME_EMAIL_HTML: &str = r#"
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Welcome to Maple</title>
    <style>
        body {
            font-family: ui-sans-serif, system-ui, -apple-system, sans-serif;
            margin: 0;
            padding: 0;
            background-color: #f9fafb;
            color: #1a1a1a;
            -webkit-font-smoothing: antialiased;
        }
        .wrapper {
            max-width: 600px;
            margin: 0 auto;
            background-color: #ffffff;
        }
        .header {
            padding: 40px 32px 24px;
            text-align: center;
        }
        .header img.logo {
            height: 48px;
            width: 48px;
            border-radius: 12px;
            margin-bottom: 16px;
        }
        .header h1 {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
            font-size: 32px;
            font-weight: 350;
            letter-spacing: -0.02em;
            margin: 0;
            color: #111;
        }
        .header p {
            font-size: 16px;
            color: #555;
            margin: 12px 0 0;
            line-height: 1.5;
        }
        .hero-image {
            text-align: center;
            padding: 8px 32px 24px;
        }
        .hero-image img {
            max-width: 100%;
            height: auto;
            border-radius: 8px;
        }
        .section {
            padding: 0 32px 24px;
        }
        .section h2 {
            font-size: 18px;
            font-weight: 600;
            margin: 0 0 8px;
            color: #111;
        }
        .section p {
            font-size: 15px;
            color: #444;
            line-height: 1.6;
            margin: 0 0 12px;
        }
        .download-buttons {
            text-align: center;
            padding: 0 32px 32px;
        }
        .download-buttons a {
            display: inline-block;
            margin: 4px 6px;
        }
        .download-buttons img {
            height: 44px;
        }
        .divider {
            border: none;
            border-top: 1px solid #eee;
            margin: 0 32px;
        }
        .pro-section {
            padding: 24px 32px;
        }
        .pro-section h2 {
            font-size: 18px;
            font-weight: 600;
            margin: 0 0 12px;
            color: #111;
        }
        .pro-section p {
            font-size: 15px;
            color: #444;
            line-height: 1.6;
            margin: 0 0 16px;
        }
        .feature-grid {
            border-collapse: collapse;
            mso-table-lspace: 0pt;
            mso-table-rspace: 0pt;
            margin: 0 0 16px;
        }
        .feature-grid td {
            vertical-align: top;
            padding: 8px 0;
            width: 50%;
        }
        .feature-grid .label {
            font-size: 14px;
            font-weight: 600;
            color: #111;
        }
        .feature-grid .desc {
            font-size: 13px;
            color: #666;
            margin-top: 2px;
        }
        .cta-button {
            display: inline-block;
            padding: 12px 28px;
            background-color: #111;
            color: #ffffff !important;
            text-decoration: none;
            border-radius: 6px;
            font-size: 15px;
            font-weight: 500;
        }
        .proxy-section {
            padding: 24px 32px;
            background-color: #f4f4f5;
        }
        .proxy-section h2 {
            font-size: 16px;
            font-weight: 600;
            margin: 0 0 8px;
            color: #111;
        }
        .proxy-section p {
            font-size: 14px;
            color: #555;
            line-height: 1.6;
            margin: 0 0 12px;
        }
        .proxy-section a {
            color: #f67d57;
            font-weight: 600;
        }
        .footer {
            padding: 24px 32px;
            text-align: center;
        }
        .footer p {
            font-size: 13px;
            color: #999;
            margin: 0 0 6px;
            line-height: 1.5;
        }
        a {
            color: #f67d57;
        }
        .footer a {
            color: #f67d57;
            text-decoration: none;
        }
    </style>
</head>
<body>
    <div class="wrapper">

        <!-- Header -->
        <div class="header">
            <img class="logo" src="https://www.trymaple.ai/apple-touch-icon.png" alt="Maple" width="48" height="48">
            <h1>Welcome to Maple</h1>
            <p>Powerful AI that respects your privacy</p>
        </div>

        <!-- Intro message -->
        <div class="section">
            <h2>Your private room is ready</h2>
            <p>This is a personal space for you and AI to talk, without anyone listening. When your conversations are confidential, you're free to think openly, ask questions, and build the future. Say what's on your mind.</p>
        </div>
        <div class="download-buttons">
            <a href="https://trymaple.ai" style="background-color: #f67d57; color: #fff; padding: 12px 28px; border-radius: 6px; text-decoration: none; font-size: 15px; font-weight: 500;">Start your first chat</a>
        </div>

        <!-- Hero Image -->
        <div class="hero-image">
            <img src="https://blog.trymaple.ai/content/images/size/w1600/2026/02/maple-mobile-and-desktop.jpg" alt="Maple on desktop and mobile" style="max-width: 100%; height: auto; border-radius: 8px;">
        </div>

        <!-- Download Section -->
        <div class="section">
            <h2>Get Maple on Every Device</h2>
            <p>Your conversations sync securely across all your devices. Pick up right where you left off. iPhone, Android, Mac, Windows, Linux, and Web.</p>
        </div>
        <div class="download-buttons">
            <a href="https://www.trymaple.ai/research#download" style="background-color: #111; color: #fff; padding: 12px 28px; border-radius: 6px; text-decoration: none; font-size: 15px; font-weight: 500;">Download Apps</a>
        </div>

        <hr class="divider">

        <!-- Pro Upsell -->
        <div class="pro-section">
            <h2>Do More with Pro</h2>
            <p>Unlock the full power of private AI. Everything is end-to-end encrypted and your data stays yours.</p>

            <table class="feature-grid" width="100%" cellpadding="0" cellspacing="0" border="0">
                <tr>
                    <td>
                        <div class="label">Most Powerful Models</div>
                        <div class="desc">Top open models, private by default</div>
                    </td>
                    <td>
                        <div class="label">Web Search</div>
                        <div class="desc">Real-time answers from the web</div>
                    </td>
                </tr>
                <tr>
                    <td>
                        <div class="label">Image Analysis</div>
                        <div class="desc">Understand photos and screenshots</div>
                    </td>
                    <td>
                        <div class="label">Document Upload</div>
                        <div class="desc">Analyze PDFs, text docs, and more</div>
                    </td>
                </tr>
                <tr>
                    <td>
                        <div class="label">Voice</div>
                        <div class="desc">Talk to Maple on mobile and desktop</div>
                    </td>
                    <td>
                        <div class="label">Developer API</div>
                        <div class="desc">OpenAI-compatible API access</div>
                    </td>
                </tr>
            </table>

            <div style="text-align: center;">
                <a href="https://www.trymaple.ai/research#pricing" class="cta-button">See Plans</a>
            </div>
        </div>

        <hr class="divider">

        <!-- Maple Proxy -->
        <div class="proxy-section">
            <table width="100%" cellpadding="0" cellspacing="0" border="0" style="border-collapse: collapse; mso-table-lspace: 0pt; mso-table-rspace: 0pt;">
                <tr>
                    <td width="48" valign="top" style="width: 48px; padding: 0;">
                        <img src="https://www.trymaple.ai/apple-touch-icon.png" alt="Maple" width="48" height="48" style="display: block; width: 48px; height: 48px; border-radius: 12px;">
                    </td>
                    <td width="16" style="width: 16px; font-size: 0; line-height: 0;">&nbsp;</td>
                    <td valign="top" style="vertical-align: top;">
                        <h2>Build with Maple Proxy</h2>
                        <p>Bring encrypted AI into your own tools. Maple Proxy is an OpenAI-compatible API that works with 1,000s of tools. Use it with coding assistants, automation pipelines, or your own apps.</p>
                        <p><a href="https://blog.trymaple.ai/maple-proxy-documentation/">Read the docs &rarr;</a></p>
                    </td>
                </tr>
            </table>
        </div>

        <!-- Footer -->
        <div class="footer">
            <img src="https://www.trymaple.ai/apple-touch-icon.png" alt="Maple" width="48" height="48" style="width: 48px; height: 48px; border-radius: 12px; margin-bottom: 12px;">
            <p>Questions? Reach us at <a href="mailto:support@trymaple.ai">support@trymaple.ai</a></p>
            <p><a href="https://trymaple.ai">trymaple.ai</a></p>
        </div>

    </div>
</body>
</html>
"#;

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

    // Only send welcome email for Maple project for now
    if project.name != "Maple" {
        tracing::debug!("Skipping welcome email for non-Maple project");
        return Ok(());
    }

    tracing::debug!("Sending maple hello email");

    let (api_key, from_email) = get_project_email_settings(app_state, project_id).await?;
    let resend = Resend::new(&api_key);

    let to = [to_email];
    let subject = format!("Welcome to {}!", project.name);

    // Schedule the email to be sent 5 minutes from now
    let scheduled_time = Utc::now() + Duration::minutes(5);
    let scheduled_at = scheduled_time.to_rfc3339();

    let email = CreateEmailBaseOptions::new(from_email, to, subject)
        .with_html(WELCOME_EMAIL_HTML)
        .with_scheduled_at(&scheduled_at);

    let email = with_account_reply_to(email, &project.name);
    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

const MAPLE_MARK_HTML: &str = r#"<img src="https://www.trymaple.ai/apple-touch-icon.png" alt="Maple" width="48" height="48" style="display:block;width:48px;height:48px;border-radius:12px;margin:0 0 16px;">"#;

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

fn account_mark_html(project_name: &str) -> &'static str {
    if is_maple_project(project_name) {
        MAPLE_MARK_HTML
    } else {
        ""
    }
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
    let subject = format!("Verify Your {} Account", project.name);
    let team_name = account_team_name(&project.name, &org.name);
    let mark = account_mark_html(&project.name);

    // Ensure base URL has exactly one trailing slash
    let base_url = email_settings.email_verification_url.trim_end_matches('/');
    let verification_url = format!("{}/{}", base_url, verification_code);

    let html_content = format!(
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
                {}
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
        project.name,
        mark,
        project.name,
        verification_url,
        verification_url,
        verification_code,
        project.name,
        team_name
    );

    let email = CreateEmailBaseOptions::new(from_email, to, subject).with_html(&html_content);

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
    let subject = format!("Reset Your {} Password", project.name);
    let team_name = account_team_name(&project.name, &org.name);
    let mark = account_mark_html(&project.name);

    let html_content = format!(
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
                {}
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
        project.name, mark, project.name, project.name, alphanumeric_code, team_name
    );

    let email = CreateEmailBaseOptions::new(from_email, to, subject).with_html(&html_content);

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
    let subject = format!("Your {} Password Has Been Reset", project.name);
    let team_name = account_team_name(&project.name, &org.name);
    let support_email = account_support_email(&project.name);
    let mark = account_mark_html(&project.name);

    let html_content = format!(
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
                {}
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
        mark, project.name, support_email, support_email, team_name
    );

    let email = CreateEmailBaseOptions::new(from_email, to, subject).with_html(&html_content);

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
    let subject = format!("Account Deletion Request for Your {} Account", project.name);
    let team_name = account_team_name(&project.name, &org.name);
    let mark = account_mark_html(&project.name);

    let html_content = format!(
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
                {}
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
        mark, project.name, confirmation_code, team_name
    );

    let email = CreateEmailBaseOptions::new(from_email, to, subject).with_html(&html_content);

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
    let subject = format!("Your {} Account Has Been Deleted", project.name);
    let team_name = account_team_name(&project.name, &org.name);
    let support_email = account_support_email(&project.name);
    let mark = account_mark_html(&project.name);

    let html_content = format!(
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
                {}
                <h1>Account Deletion Confirmation</h1>
                <p>Your {} account has been successfully deleted along with all associated data.</p>
                <p>If you did not request this account deletion, please contact us immediately at <a href="mailto:{}">{}</a>.</p>
                <p>Thank you for your time with us. We hope to see you again in the future.</p>
                <p>Best regards,<br>The {} Team</p>
            </div>
        </body>
        </html>
        "#,
        mark, project.name, support_email, support_email, team_name
    );

    let email = CreateEmailBaseOptions::new(from_email, to, subject).with_html(&html_content);

    let email = with_account_reply_to(email, &project.name);
    let _email = resend.emails.send(email).await.map_err(|e| {
        tracing::error!("Failed to send email: {}", e);
        EmailError::UnknownError
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{account_mark_html, account_support_email, account_team_name, WELCOME_EMAIL_HTML};

    #[test]
    fn maple_account_mail_uses_the_maple_mark_and_signoff() {
        assert_eq!(account_team_name("Maple", "OpenSecret"), "Maple");
        assert_eq!(account_support_email("Maple"), "support@trymaple.ai");
        assert!(account_mark_html("Maple").contains("www.trymaple.ai/apple-touch-icon.png"));
        assert!(WELCOME_EMAIL_HTML.contains("https://www.trymaple.ai/research#download"));
        assert!(WELCOME_EMAIL_HTML.contains("https://www.trymaple.ai/research#pricing"));
        assert!(WELCOME_EMAIL_HTML.contains("https://www.trymaple.ai/apple-touch-icon.png"));
        assert!(WELCOME_EMAIL_HTML.contains("Welcome to Maple</h1>"));
        assert!(WELCOME_EMAIL_HTML.contains("Windows"));
        assert!(!WELCOME_EMAIL_HTML.contains("maple-app-icon-rounded"));
        assert!(!WELCOME_EMAIL_HTML.contains("#a855f7"));
        assert!(!WELCOME_EMAIL_HTML.contains("Maple AI"));
    }

    #[test]
    fn other_project_mail_keeps_the_organization_signoff() {
        assert_eq!(account_team_name("Other", "OpenSecret"), "OpenSecret");
        assert_eq!(account_support_email("Other"), "support@opensecret.cloud");
        assert_eq!(account_mark_html("Other"), "");
    }

    #[test]
    fn maple_replies_go_to_support() {
        use super::account_reply_to;

        assert_eq!(account_reply_to("Maple"), Some("support@trymaple.ai"));
        assert_eq!(account_reply_to("Other"), None);
    }
}
