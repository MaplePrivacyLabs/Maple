//! Maple account mail. The HTML files keep the shared layout separate from the
//! message bodies and are compiled into the binary.
//!
//! Rendering cannot panic. Every `{{NAME}}` in the templates is checked while
//! the crate compiles, so a misspelled or unclosed placeholder fails the build,
//! and `fill` reports anything else as an error instead of panicking.

use super::{AccountEmail, AccountMessage};
use chrono::{Datelike, Utc};

pub(super) const ASSET_BASE: &str = "https://img.maple.ai/email/";

const LAYOUT: &str = include_str!("maple_templates/layout.html");
const WELCOME: &str = include_str!("maple_templates/welcome.html");
const VERIFICATION: &str = include_str!("maple_templates/verification.html");
const PASSWORD_RESET: &str = include_str!("maple_templates/password-reset.html");
const PASSWORD_RESET_CONFIRMATION: &str =
    include_str!("maple_templates/password-reset-confirmation.html");
const RECOVERY_CODE_CHANGE: &str = include_str!("maple_templates/recovery-code-change.html");
const ACCOUNT_DELETION: &str = include_str!("maple_templates/account-deletion.html");
const ACCOUNT_DELETION_CONFIRMATION: &str =
    include_str!("maple_templates/account-deletion-confirmation.html");

/// Values `render` gives the layout.
const LAYOUT_FIELDS: &[&str] = &[
    "TITLE",
    "PREHEADER",
    "BLOCKS",
    "SUPPORT",
    "YEAR",
    "CAMPAIGN",
    "ASSET_BASE",
];
/// Values `render` gives every message body. Bodies that show a code or link
/// also get the names listed next to them below.
const BODY_FIELDS: &[&str] = &["PROJECT", "TEAM", "SUPPORT", "ASSET_BASE"];

// Evaluated while compiling, never at runtime: each template may only use the
// values `render` supplies to it.
const _: () = {
    assert!(
        placeholders_are_known(LAYOUT, &[LAYOUT_FIELDS]),
        "layout.html has an unknown or unclosed placeholder"
    );
    assert!(
        placeholders_are_known(WELCOME, &[BODY_FIELDS]),
        "welcome.html has an unknown or unclosed placeholder"
    );
    assert!(
        placeholders_are_known(
            VERIFICATION,
            &[BODY_FIELDS, &["VERIFICATION_CODE", "VERIFICATION_URL"]]
        ),
        "verification.html has an unknown or unclosed placeholder"
    );
    assert!(
        placeholders_are_known(PASSWORD_RESET, &[BODY_FIELDS, &["RESET_CODE"]]),
        "password-reset.html has an unknown or unclosed placeholder"
    );
    assert!(
        placeholders_are_known(
            PASSWORD_RESET_CONFIRMATION,
            &[BODY_FIELDS, &["RECOVERY_DETAILS"]]
        ),
        "password-reset-confirmation.html has an unknown or unclosed placeholder"
    );
    assert!(
        placeholders_are_known(
            RECOVERY_CODE_CHANGE,
            &[BODY_FIELDS, &["RECOVERY_TITLE", "RECOVERY_DETAILS"]]
        ),
        "recovery-code-change.html has an unknown or unclosed placeholder"
    );
    assert!(
        placeholders_are_known(ACCOUNT_DELETION, &[BODY_FIELDS, &["DELETION_CODE"]]),
        "account-deletion.html has an unknown or unclosed placeholder"
    );
    assert!(
        placeholders_are_known(ACCOUNT_DELETION_CONFIRMATION, &[BODY_FIELDS]),
        "account-deletion-confirmation.html has an unknown or unclosed placeholder"
    );
};

/// Why a template could not be filled.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TemplateError {
    /// A `{{` with no `}}` after it.
    Unclosed,
    /// A placeholder with no value supplied.
    Unknown(String),
}

pub(super) fn escape_html(value: &str) -> String {
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
fn fill(template: &str, fields: &[(&str, &str)]) -> Result<String, TemplateError> {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some((text, after_open)) = rest.split_once("{{") {
        let (key, after_close) = after_open.split_once("}}").ok_or(TemplateError::Unclosed)?;
        let value = fields
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| *value)
            .ok_or_else(|| TemplateError::Unknown(key.to_string()))?;
        result.push_str(text);
        result.push_str(value);
        rest = after_close;
    }
    result.push_str(rest);
    Ok(result)
}

/// Whether every `{{...}}` in `template` is closed and names one of `fields`,
/// by the same rules as `fill`. A `const fn` so the checks above run at
/// compile time.
const fn placeholders_are_known(template: &str, fields: &[&[&str]]) -> bool {
    let bytes = template.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut end = start;
            while end + 1 < bytes.len() && !(bytes[end] == b'}' && bytes[end + 1] == b'}') {
                end += 1;
            }
            if end + 1 >= bytes.len() || !is_field(bytes, start, end, fields) {
                return false;
            }
            i = end + 2;
        } else {
            i += 1;
        }
    }
    true
}

/// Whether `bytes[start..end]` equals one of `fields`.
const fn is_field(bytes: &[u8], start: usize, end: usize, fields: &[&[&str]]) -> bool {
    let mut group = 0;
    while group < fields.len() {
        let mut index = 0;
        while index < fields[group].len() {
            let name = fields[group][index].as_bytes();
            if name.len() == end - start {
                let mut j = 0;
                while j < name.len() && name[j] == bytes[start + j] {
                    j += 1;
                }
                if j == name.len() {
                    return true;
                }
            }
            index += 1;
        }
        group += 1;
    }
    false
}

pub(super) fn render(
    email: AccountEmail<'_>,
    project: &str,
    team: &str,
    support: &str,
) -> Result<AccountMessage, TemplateError> {
    let (subject, title, preheader, body, campaign) = match email {
        AccountEmail::Welcome => (
            "Welcome to Maple".to_string(),
            "Welcome to Maple".to_string(),
            "Your private space for real conversations is ready.",
            WELCOME,
            "welcome",
        ),
        AccountEmail::Verification { .. } => (
            format!("Verify your {project} account"),
            format!("Verify your {project} account"),
            "Confirm your email to finish setting up. The link expires in 24 hours.",
            VERIFICATION,
            "verify-email",
        ),
        AccountEmail::PasswordReset { .. } => (
            format!("Reset your {project} password"),
            format!("Reset your {project} password"),
            "Enter the code in the app to choose a new password. It expires in 24 hours.",
            PASSWORD_RESET,
            "password-reset",
        ),
        AccountEmail::PasswordResetConfirmation { .. } => (
            format!("Your {project} password was changed"),
            "Password updated".to_string(),
            "If this was you, you're all set. If not, contact us right away.",
            PASSWORD_RESET_CONFIRMATION,
            "password-changed",
        ),
        AccountEmail::RecoveryCodeChange { event } => (
            format!("{} for your {project} account", event.title()),
            event.title().to_string(),
            "A recovery setting changed on your account. If this wasn't you, contact us right away.",
            RECOVERY_CODE_CHANGE,
            "recovery-code-changed",
        ),
        AccountEmail::AccountDeletion { .. } => (
            format!("Confirm your {project} account deletion"),
            "Confirm account deletion".to_string(),
            "Enter the code in the app to confirm. This can't be undone.",
            ACCOUNT_DELETION,
            "delete-account",
        ),
        AccountEmail::AccountDeletionConfirmation => (
            format!("Your {project} account has been deleted"),
            "Account deleted".to_string(),
            "Your account and its data have been permanently removed.",
            ACCOUNT_DELETION_CONFIRMATION,
            "account-deleted",
        ),
    };
    // The code or link a body shows, beyond the values every body gets.
    let extra = match email {
        AccountEmail::Verification { url, code } => vec![
            ("VERIFICATION_CODE", escape_html(code)),
            ("VERIFICATION_URL", escape_html(url)),
        ],
        AccountEmail::PasswordReset { code } => vec![("RESET_CODE", escape_html(code))],
        AccountEmail::AccountDeletion { code } => vec![("DELETION_CODE", escape_html(code))],
        AccountEmail::PasswordResetConfirmation { recovery } => {
            vec![("RECOVERY_DETAILS", escape_html(recovery.details()))]
        }
        AccountEmail::RecoveryCodeChange { event } => vec![
            ("RECOVERY_TITLE", escape_html(event.title())),
            ("RECOVERY_DETAILS", escape_html(event.details())),
        ],
        AccountEmail::Welcome | AccountEmail::AccountDeletionConfirmation => Vec::new(),
    };

    let project = escape_html(project);
    let team = escape_html(team);
    let support = escape_html(support);
    let mut fields = vec![
        ("PROJECT", project.as_str()),
        ("TEAM", team.as_str()),
        ("SUPPORT", support.as_str()),
        ("ASSET_BASE", ASSET_BASE),
    ];
    fields.extend(extra.iter().map(|(name, value)| (*name, value.as_str())));
    let blocks = fill(body, &fields)?;

    let title = escape_html(&title);
    let preheader = escape_html(preheader);
    let year = Utc::now().year().to_string();
    let html = fill(
        LAYOUT,
        &[
            ("TITLE", &title),
            ("PREHEADER", &preheader),
            ("BLOCKS", &blocks),
            ("SUPPORT", &support),
            ("YEAR", &year),
            ("CAMPAIGN", campaign),
            ("ASSET_BASE", ASSET_BASE),
        ],
    )?;
    Ok(AccountMessage { subject, html })
}

#[cfg(test)]
mod tests {
    use super::{fill, placeholders_are_known, TemplateError};

    const FIELDS: &[(&str, &str)] = &[("A", "1"), ("B", "2")];

    fn filled(template: &str) -> Result<String, TemplateError> {
        fill(template, FIELDS)
    }

    #[test]
    fn fill_replaces_placeholders_anywhere_in_the_template() {
        assert_eq!(filled(""), Ok(String::new()));
        assert_eq!(filled("{{A}}"), Ok("1".to_string()));
        assert_eq!(filled("{{A}}{{B}}"), Ok("12".to_string()));
        assert_eq!(filled("{{A}} and {{A}}"), Ok("1 and 1".to_string()));
        assert_eq!(filled("<p>{{B}}</p>"), Ok("<p>2</p>".to_string()));
        // Single braces and stray closers are ordinary text, as in CSS.
        assert_eq!(
            filled("a { b: c; } }} {"),
            Ok("a { b: c; } }} {".to_string())
        );
    }

    #[test]
    fn fill_rejects_unknown_and_unclosed_placeholders() {
        assert_eq!(
            filled("{{C}}"),
            Err(TemplateError::Unknown("C".to_string()))
        );
        assert_eq!(
            filled("{{a}}"),
            Err(TemplateError::Unknown("a".to_string()))
        );
        assert_eq!(filled("{{}}"), Err(TemplateError::Unknown(String::new())));
        assert_eq!(
            filled("{{{A}}"),
            Err(TemplateError::Unknown("{A".to_string()))
        );
        assert_eq!(filled("{{A"), Err(TemplateError::Unclosed));
        assert_eq!(filled("{{A}} then {{"), Err(TemplateError::Unclosed));
    }

    #[test]
    fn fill_never_reads_values_as_placeholders() {
        let fields = [("A", "{{B}}"), ("B", "}} {{")];
        assert_eq!(fill("{{A}}|{{B}}", &fields), Ok("{{B}}|}} {{".to_string()));
    }

    #[test]
    fn compile_time_check_agrees_with_fill() {
        for template in [
            "",
            "plain text",
            "{{A}}",
            "{{A}}{{B}}",
            "x {{B}} y",
            "a { b } }} {",
            "{{C}}",
            "{{a}}",
            "{{}}",
            "{{{A}}",
            "{{A",
            "{{A}} then {{",
            "}}{{A}}",
        ] {
            assert_eq!(
                placeholders_are_known(template, &[&["A"], &["B"]]),
                fill(template, FIELDS).is_ok(),
                "{template}"
            );
        }
    }
}
