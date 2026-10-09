//! Maple account mail, ported from email-previews/build.py. The HTML files keep
//! the approved shared layout separate from the six message bodies.

use chrono::{Datelike, Local};

pub(super) const ASSET_BASE: &str = "https://img.maple.ai/";

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Welcome,
    Verification,
    PasswordReset,
    PasswordResetConfirmation,
    AccountDeletion,
    AccountDeletionConfirmation,
}

pub(super) struct Message {
    pub subject: String,
    pub html: String,
}

fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

// Resolve each placeholder once. Values are never parsed as templates, even if
// user input contains braces or another placeholder's name.
fn fill(template: &str, fields: &[(&str, &str)]) -> String {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let end = tail.find("}}").expect("complete template placeholder");
        let key = &tail[..end];
        let value = fields
            .iter()
            .find(|(name, _)| *name == key)
            .unwrap_or_else(|| panic!("unknown email template placeholder: {key}"))
            .1;
        result.push_str(value);
        rest = &tail[end + 2..];
    }
    result.push_str(rest);
    result
}

pub(super) fn render(
    kind: Kind,
    project: &str,
    team: &str,
    support: &str,
    code: &str,
    verification_url: &str,
) -> Message {
    let (subject, title, preheader, body) = match kind {
        Kind::Welcome => (
            "Welcome to Maple".to_string(),
            "Welcome to Maple".to_string(),
            "Your private space for real conversations is ready.".to_string(),
            include_str!("maple_templates/welcome.html"),
        ),
        Kind::Verification => (
            format!("Verify your {project} account"),
            format!("Verify your {project} account"),
            "Confirm your email to finish setting up. The link expires in 24 hours.".to_string(),
            include_str!("maple_templates/verification.html"),
        ),
        Kind::PasswordReset => (
            format!("Reset your {project} password"),
            format!("Reset your {project} password"),
            format!("Your code is {code}. It expires in 24 hours."),
            include_str!("maple_templates/password-reset.html"),
        ),
        Kind::PasswordResetConfirmation => (
            format!("Your {project} password was changed"),
            "Password updated".to_string(),
            "If this was you, you're all set. If not, contact us right away.".to_string(),
            include_str!("maple_templates/password-reset-confirmation.html"),
        ),
        Kind::AccountDeletion => (
            format!("Confirm your {project} account deletion"),
            "Confirm account deletion".to_string(),
            "Enter the code in the app to confirm. This can't be undone.".to_string(),
            include_str!("maple_templates/account-deletion.html"),
        ),
        Kind::AccountDeletionConfirmation => (
            format!("Your {project} account has been deleted"),
            "Account deleted".to_string(),
            "Your account and its data have been permanently removed.".to_string(),
            include_str!("maple_templates/account-deletion-confirmation.html"),
        ),
    };
    let campaign = match kind {
        Kind::Welcome => "welcome",
        Kind::Verification => "verify-email",
        Kind::PasswordReset => "password-reset",
        Kind::PasswordResetConfirmation => "password-changed",
        Kind::AccountDeletion => "delete-account",
        Kind::AccountDeletionConfirmation => "account-deleted",
    };

    let project = escape_html(project);
    let team = escape_html(team);
    let support = escape_html(support);
    let code = escape_html(code);
    let verification_url = escape_html(verification_url);
    let fields = [
        ("PROJECT", project.as_str()),
        ("TEAM", team.as_str()),
        ("SUPPORT", support.as_str()),
        ("VERIFICATION_CODE", code.as_str()),
        ("VERIFICATION_URL", verification_url.as_str()),
        ("RESET_CODE", code.as_str()),
        ("DELETION_CODE", code.as_str()),
        ("ASSET_BASE", ASSET_BASE),
    ];
    let blocks = fill(body, &fields);
    let title = escape_html(&title);
    let preheader = escape_html(&preheader);
    let year = Local::now().year().to_string();
    let html = fill(
        include_str!("maple_templates/layout.html"),
        &[
            ("TITLE", &title),
            ("PREHEADER", &preheader),
            ("BLOCKS", &blocks),
            ("SUPPORT", &support),
            ("YEAR", &year),
            ("CAMPAIGN", campaign),
            ("ASSET_BASE", ASSET_BASE),
        ],
    );
    Message { subject, html }
}
