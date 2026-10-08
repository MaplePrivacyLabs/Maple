//! Missing working-directory diagnostics from `core/session-cwd.ts`.
use super::session_manager::SessionManager;
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path};
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCwdIssue {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_cwd: String,
    pub fallback_cwd: String,
}
pub trait SessionCwdSource {
    fn get_cwd(&self) -> &str;
    fn get_session_file(&self) -> Option<String>;
}
impl SessionCwdSource for SessionManager {
    fn get_cwd(&self) -> &str {
        SessionManager::get_cwd(self)
    }
    fn get_session_file(&self) -> Option<String> {
        SessionManager::get_session_file(self)
    }
}
pub fn get_missing_session_cwd_issue(
    source: &impl SessionCwdSource,
    fallback_cwd: &str,
) -> Option<SessionCwdIssue> {
    let file = source.get_session_file().filter(|s| !s.is_empty())?;
    let cwd = source.get_cwd();
    if cwd.is_empty() || Path::new(cwd).exists() {
        return None;
    }
    Some(SessionCwdIssue {
        session_file: Some(file),
        session_cwd: cwd.into(),
        fallback_cwd: fallback_cwd.into(),
    })
}
pub fn format_missing_session_cwd_error(issue: &SessionCwdIssue) -> String {
    let file = issue
        .session_file
        .as_ref()
        .filter(|s| !s.is_empty())
        .map(|s| format!("\nSession file: {s}"))
        .unwrap_or_default();
    format!(
        "Stored session working directory does not exist: {}{}\nCurrent working directory: {}",
        issue.session_cwd, file, issue.fallback_cwd
    )
}
pub fn format_missing_session_cwd_prompt(issue: &SessionCwdIssue) -> String {
    format!(
        "cwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
        issue.session_cwd, issue.fallback_cwd
    )
}
#[derive(Clone, Debug)]
pub struct MissingSessionCwdError {
    pub issue: SessionCwdIssue,
}
impl MissingSessionCwdError {
    pub fn name(&self) -> &str {
        "MissingSessionCwdError"
    }
}
impl fmt::Display for MissingSessionCwdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_missing_session_cwd_error(&self.issue))
    }
}
impl std::error::Error for MissingSessionCwdError {}
pub fn assert_session_cwd_exists(
    source: &impl SessionCwdSource,
    fallback_cwd: &str,
) -> Result<(), MissingSessionCwdError> {
    if let Some(issue) = get_missing_session_cwd_issue(source, fallback_cwd) {
        Err(MissingSessionCwdError { issue })
    } else {
        Ok(())
    }
}
