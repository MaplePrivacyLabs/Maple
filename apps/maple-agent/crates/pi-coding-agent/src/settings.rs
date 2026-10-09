use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use pi_agent_core::{QueueMode, ToolExecutionMode};
use pi_ai::ThinkingLevel;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compaction::CompactionSettings;
use crate::trust::DefaultProjectTrust;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetrySettings {
    pub enabled: bool,
    pub max_retries: u32,
    /// The first delay; each retry doubles it.
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2_000,
            max_delay_ms: 60_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImageSettings {
    /// Resize images the `read` tool sends so they fit inline image limits.
    pub auto_resize: bool,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self { auto_resize: true }
    }
}

/// Session behaviour. User settings are overlaid by project settings, key by key.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub compaction: CompactionSettings,
    pub retry: RetrySettings,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub tool_execution: ToolExecutionMode,
    pub default_thinking_level: Option<ThinkingLevel>,
    /// Extra skill files or folders.
    pub skills: Vec<PathBuf>,
    /// Extra prompt template folders.
    pub prompts: Vec<PathBuf>,
    /// Text added to the system prompt, in place of `APPEND_SYSTEM.md`.
    pub append_system_prompt: Option<String>,
    /// The shell `bash` runs instead of the one it finds; a leading `~` is the home
    /// folder.
    pub shell_path: Option<PathBuf>,
    /// Run before every `bash` command, for example `shopt -s expand_aliases`.
    pub shell_command_prefix: Option<String>,
    pub images: ImageSettings,
    /// What to do with a project that has resources of its own and no decision yet.
    /// Only the user's settings can set it.
    pub default_project_trust: DefaultProjectTrust,
}

/// Overlay `overlay` onto `base`: objects merge key by key, anything else replaces.
pub fn merge_json(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn read_json(path: &Path) -> io::Result<Option<Value>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl Settings {
    /// Load user settings overlaid with project settings. Missing files are skipped; a
    /// file that does not parse is an error rather than silently ignored.
    pub fn load(user: Option<&Path>, project: Option<&Path>) -> io::Result<Self> {
        let mut merged = serde_json::to_value(Settings::default())?;
        for (path, is_project) in [(user, false), (project, true)] {
            let Some(path) = path else { continue };
            if let Some(mut value) = read_json(path)? {
                if is_project && let Some(object) = value.as_object_mut() {
                    // A project cannot decide how far it is trusted.
                    object.remove("defaultProjectTrust");
                }
                merge_json(&mut merged, value);
            }
        }
        serde_json::from_value(merged)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_settings_overlay_user_settings_key_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.json");
        let project = dir.path().join("project.json");
        fs::write(&user, json!({ "compaction": { "reserveTokens": 1000, "keepRecentTokens": 5 }, "steeringMode": "all" }).to_string()).unwrap();
        fs::write(
            &project,
            json!({ "compaction": { "keepRecentTokens": 9 }, "defaultThinkingLevel": "high" })
                .to_string(),
        )
        .unwrap();

        let settings = Settings::load(Some(&user), Some(&project)).unwrap();
        assert_eq!(settings.compaction.reserve_tokens, 1000);
        assert_eq!(settings.compaction.keep_recent_tokens, 9);
        assert!(settings.compaction.enabled);
        assert_eq!(settings.steering_mode, QueueMode::All);
        assert_eq!(settings.default_thinking_level, Some(ThinkingLevel::High));
        assert_eq!(settings.retry, RetrySettings::default());
    }

    #[test]
    fn only_the_user_sets_the_default_project_trust() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.json");
        let project = dir.path().join("project.json");
        fs::write(&user, json!({ "defaultProjectTrust": "never" }).to_string()).unwrap();
        fs::write(
            &project,
            json!({ "defaultProjectTrust": "always" }).to_string(),
        )
        .unwrap();
        let settings = Settings::load(Some(&user), Some(&project)).unwrap();
        assert_eq!(settings.default_project_trust, DefaultProjectTrust::Never);
        let settings = Settings::load(None, Some(&project)).unwrap();
        assert_eq!(settings.default_project_trust, DefaultProjectTrust::Ask);
    }

    #[test]
    fn missing_files_give_defaults_and_broken_files_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Settings::load(Some(&dir.path().join("none.json")), None).unwrap(),
            Settings::default()
        );
        let broken = dir.path().join("broken.json");
        fs::write(&broken, "{ nope").unwrap();
        assert!(Settings::load(Some(&broken), None).is_err());
    }
}
