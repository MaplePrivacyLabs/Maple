//! App settings persisted to ~/.config/maple-agent/settings.json and local
//! usage aggregation read from the goose usage ledger.

// This module is the desktop frontend's boundary. A headless build (no
// `desktop` feature) uses only a few entry points, so the rest is unused
// there by design.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppSettings {
    /// Whether tool cards show input/output payloads by default.
    #[serde(default = "default_tool_details")]
    pub tool_details: bool,
    /// Whether new tasks can use the web tools.
    #[serde(default = "default_web_enabled")]
    pub default_web_enabled: bool,
    /// Whether completed tool calls get a one-line model summary.
    #[serde(default = "default_tool_summaries")]
    pub tool_summaries: bool,
    /// Enable modal Vim editing only in the main chat composer.
    #[serde(default)]
    pub composer_vim_enabled: bool,
    /// Enable stable-ID application navigation independently of composer Vim.
    #[serde(default)]
    pub application_vim_enabled: bool,
    /// Per-binding shortcut changes keyed by the stable slot IDs exposed in
    /// Keyboard Shortcuts. A string replaces the physical sequence; `null`
    /// disables that exact slot. Missing entries retain their shipped key.
    #[serde(default)]
    pub shortcut_overrides: std::collections::BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub pinned_roots: Vec<String>,
    /// Sidebar task ids the user pinned, in pin order.
    #[serde(default)]
    pub pinned_tasks: Vec<String>,
    /// Display names for project roots, keyed by absolute path.
    #[serde(default)]
    pub project_names: std::collections::HashMap<String, String>,
    /// Whether run completion and questions raise desktop notifications
    /// while the window is not focused.
    #[serde(default = "default_desktop_notifications")]
    pub desktop_notifications: bool,
    /// Skip looping and reveal animations. gpui reads no OS preference for
    /// this, so it is a Maple setting.
    #[serde(default)]
    pub reduce_motion: bool,
    /// Opening system prompt text for agents this app hosts. Empty means
    /// [`DEFAULT_HARNESS_INSTRUCTIONS`].
    #[serde(default)]
    pub harness_instructions: String,
    /// Window size and state from the last run.
    #[serde(default)]
    pub window: Option<WindowState>,
    /// Color theme: "system", "dark", or "light".
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Chat reading face: "system", "manrope", "geist", or "serif".
    /// Retired `"sf-pro"` values parse as `"system"`.
    #[serde(default = "default_chat_font_family")]
    pub chat_font_family: String,
    /// Chat reading size in px, clamped to 13–18.
    #[serde(default = "default_chat_font_size")]
    pub chat_font_size: u8,
    /// Text-to-speech voice id; see [`TTS_VOICES`].
    #[serde(default = "default_tts_voice")]
    pub tts_voice: String,
    /// Text-to-speech speed multiplier; see [`TTS_SPEEDS`].
    #[serde(default = "default_tts_speed")]
    pub tts_speed: f32,
}

/// Voxtral voice ids with their labels, in the order the settings menu
/// lists them. Mirrors the Maple web app.
pub const TTS_VOICES: [(&str, &str); 20] = [
    ("neutral_female", "Neutral — Female"),
    ("neutral_male", "Neutral — Male"),
    ("casual_female", "Casual — Female"),
    ("casual_male", "Casual — Male"),
    ("cheerful_female", "Cheerful — Female"),
    ("ar_male", "Arabic-accented — Male"),
    ("de_female", "German-accented — Female"),
    ("de_male", "German-accented — Male"),
    ("es_female", "Spanish-accented — Female"),
    ("es_male", "Spanish-accented — Male"),
    ("fr_female", "French-accented — Female"),
    ("fr_male", "French-accented — Male"),
    ("hi_female", "Hindi-accented — Female"),
    ("hi_male", "Hindi-accented — Male"),
    ("it_female", "Italian-accented — Female"),
    ("it_male", "Italian-accented — Male"),
    ("nl_female", "Dutch-accented — Female"),
    ("nl_male", "Dutch-accented — Male"),
    ("pt_female", "Portuguese-accented — Female"),
    ("pt_male", "Portuguese-accented — Male"),
];

/// Speech speeds the settings menu offers.
pub const TTS_SPEEDS: [f32; 6] = [0.8, 1.0, 1.2, 1.5, 1.8, 2.0];
const DEFAULT_TTS_VOICE: &str = "casual_female";
const DEFAULT_TTS_SPEED: f32 = 1.0;

/// Label for a voice id; the id itself when it is unknown.
pub fn tts_voice_label(voice: &str) -> &str {
    TTS_VOICES
        .iter()
        .find(|(id, _)| *id == voice)
        .map(|(_, label)| *label)
        .unwrap_or(voice)
}

fn default_tts_voice() -> String {
    DEFAULT_TTS_VOICE.to_string()
}

fn default_tts_speed() -> f32 {
    DEFAULT_TTS_SPEED
}

/// Persisted window geometry. Position is left to the window manager:
/// Wayland does not expose it, and a stale position can open the window
/// off-screen after a monitor change.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowState {
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

impl WindowState {
    /// Keep a saved size inside a sane range so a corrupt file cannot
    /// open a window too small to use.
    pub fn clamped(self) -> Self {
        Self {
            width: self.width.clamp(640., 8192.),
            height: self.height.clamp(480., 8192.),
            maximized: self.maximized,
        }
    }
}

/// Opening system prompt for agents this app hosts: the agent is Maple.
/// The runtime appends its tool and runtime guidance after this text.
pub const DEFAULT_HARNESS_INSTRUCTIONS: &str =
    "You are a general-purpose AI agent called Maple, created by Maple AI.
You run in the Maple app's Agent Mode; users know you simply as Maple.";

impl AppSettings {
    /// The harness instructions to hand the runtime: the saved text, or the
    /// default when nothing is saved.
    pub fn effective_harness_instructions(&self) -> String {
        let saved = self.harness_instructions.trim();
        if saved.is_empty() {
            DEFAULT_HARNESS_INSTRUCTIONS.to_string()
        } else {
            saved.to_string()
        }
    }
}

fn default_theme() -> String {
    "system".to_string()
}

fn default_chat_font_family() -> String {
    "system".to_string()
}

fn default_chat_font_size() -> u8 {
    14
}

fn default_web_enabled() -> bool {
    true
}

fn default_tool_details() -> bool {
    false
}

fn default_desktop_notifications() -> bool {
    true
}

fn default_tool_summaries() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            tool_details: default_tool_details(),
            default_web_enabled: default_web_enabled(),
            tool_summaries: default_tool_summaries(),
            composer_vim_enabled: false,
            application_vim_enabled: false,
            shortcut_overrides: std::collections::BTreeMap::new(),
            pinned_roots: Vec::new(),
            pinned_tasks: Vec::new(),
            project_names: std::collections::HashMap::new(),
            desktop_notifications: default_desktop_notifications(),
            reduce_motion: false,
            harness_instructions: String::new(),
            window: None,
            theme: default_theme(),
            chat_font_family: default_chat_font_family(),
            chat_font_size: default_chat_font_size(),
            tts_voice: default_tts_voice(),
            tts_speed: default_tts_speed(),
        }
    }
}

#[cfg(not(test))]
fn settings_file() -> PathBuf {
    crate::backend::app_config_root().join("settings.json")
}

#[cfg(test)]
fn settings_file() -> PathBuf {
    test_settings_file()
}

/// Under `cargo test`, the settings file must never resolve to the
/// developer's real settings.json: background writes queued by widget
/// tests would silently rewrite it, and a test process killed mid-write
/// would litter the real config directory with abandoned temp files.
/// Tests that verify the on-disk resolution set XDG_CONFIG_HOME
/// explicitly; every other test lands in a per-process scratch root.
#[cfg(test)]
fn test_settings_file() -> PathBuf {
    static SCRATCH: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
        std::env::temp_dir().join(format!("maple-agent-test-config-{}", std::process::id()))
    });
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
        // Resolve from the captured value, not a second read: another
        // test may restore the variable in between.
        let base = PathBuf::from(base);
        if base.is_absolute() {
            return base
                .join(crate::backend::APP_DIR_NAME)
                .join("settings.json");
        }
    }
    SCRATCH.join("settings.json")
}

pub fn load_settings() -> AppSettings {
    let path = settings_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Cannot read settings at {}: {error}", path.display());
            }
            return AppSettings::default();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|error| {
        log::warn!(
            "Settings at {} are not valid; using defaults: {error}",
            path.display()
        );
        AppSettings::default()
    })
}

/// Serializes tests that swap `XDG_CONFIG_HOME` process-wide: while a swap
/// is live, no other test may resolve or write the settings file. Shared
/// with the chat screen's persisted-defaults test.
#[cfg(test)]
pub(crate) static SETTINGS_IO_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Record the window state for the next launch. Runs on the UI thread at
/// quit and waits for the write, so updates queued earlier also land.
pub fn save_window_state(state: WindowState) {
    update_settings_and_wait(move |settings| settings.window = Some(state));
}

fn save_settings(settings: &AppSettings) {
    let path = settings_file();
    if let Err(error) = maple_agent::private_file::write_private_json(&path, settings) {
        log::error!("Cannot save settings to {}: {error}", path.display());
    }
}

type SettingsUpdate = Box<dyn FnOnce(&mut AppSettings) + Send>;

/// One queued change and, optionally, a channel to signal once it is on disk.
struct SettingsWrite {
    update: SettingsUpdate,
    done: Option<std::sync::mpsc::Sender<()>>,
}

/// The single writer thread. Every change goes through it in call order
/// as a read-modify-write of the file, so two callers that change
/// different fields both land and a later change to one field always
/// wins over an earlier one.
fn settings_writer() -> &'static std::sync::mpsc::Sender<SettingsWrite> {
    static WRITER: std::sync::OnceLock<std::sync::mpsc::Sender<SettingsWrite>> =
        std::sync::OnceLock::new();
    WRITER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<SettingsWrite>();
        std::thread::Builder::new()
            .name("settings-writer".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    // Coalesce a burst of changes into one write.
                    let mut batch = vec![first];
                    while let Ok(next) = rx.try_recv() {
                        batch.push(next);
                    }
                    let mut settings = load_settings();
                    let mut acks = Vec::new();
                    for write in batch {
                        (write.update)(&mut settings);
                        acks.extend(write.done);
                    }
                    save_settings(&settings);
                    for ack in acks {
                        let _ = ack.send(());
                    }
                }
            })
            .expect("spawn settings writer");
        tx
    })
}

fn queue_settings_write(write: SettingsWrite) {
    if settings_writer().send(write).is_err() {
        log::error!("Settings writer is gone; change not saved");
    }
}

/// Apply `update` to the settings file from the writer thread so a
/// toggle never blocks the UI thread on disk I/O.
pub fn update_settings_in_background(update: impl FnOnce(&mut AppSettings) + Send + 'static) {
    queue_settings_write(SettingsWrite {
        update: Box::new(update),
        done: None,
    });
}

/// Apply `update` and block until it and every earlier update are on disk.
pub fn update_settings_and_wait(update: impl FnOnce(&mut AppSettings) + Send + 'static) {
    let (done, rx) = std::sync::mpsc::channel::<()>();
    queue_settings_write(SettingsWrite {
        update: Box::new(update),
        done: Some(done),
    });
    let _ = rx.recv();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings file written while the app still saved a permission mode
    /// and a shortcut for the approval card loads as it is; the next save
    /// drops the mode, and the retired shortcut slot is ignored at run time
    /// (see `shortcuts::tests::an_override_for_a_retired_slot_is_dropped_quietly`).
    #[test]
    fn settings_with_a_saved_permission_mode_still_load() {
        let settings: AppSettings = serde_json::from_str(
            r#"{"default_permission_mode":"auto","tool_details":true,
                "shortcut_overrides":{"chat.allow_permission":null,"chat.new_task":"secondary-shift-n"}}"#,
        )
        .expect("an old settings file still loads");
        assert!(settings.tool_details);
        assert_eq!(
            settings.shortcut_overrides.get("chat.allow_permission"),
            Some(&None)
        );
        let json = serde_json::to_value(&settings).expect("serialize");
        assert!(json.get("default_permission_mode").is_none());
    }

    #[test]
    fn existing_settings_files_default_composer_vim_to_off() {
        let mut json = serde_json::to_value(AppSettings::default()).expect("serialize");
        json.as_object_mut()
            .expect("settings object")
            .remove("composer_vim_enabled");
        let settings: AppSettings = serde_json::from_value(json).expect("deserialize old file");
        assert!(!settings.composer_vim_enabled);
    }

    #[test]
    fn existing_settings_files_default_application_vim_to_off() {
        let mut json = serde_json::to_value(AppSettings::default()).expect("serialize");
        json.as_object_mut()
            .expect("settings object")
            .remove("application_vim_enabled");
        let settings: AppSettings = serde_json::from_value(json).expect("deserialize old file");
        assert!(!settings.application_vim_enabled);
    }

    #[test]
    fn existing_settings_files_default_chat_reading_to_system_14() {
        let mut json = serde_json::to_value(AppSettings::default()).expect("serialize");
        let object = json.as_object_mut().expect("settings object");
        object.remove("chat_font_family");
        object.remove("chat_font_size");
        let settings: AppSettings = serde_json::from_value(json).expect("deserialize old file");
        assert_eq!(settings.chat_font_family, "system");
        assert_eq!(settings.chat_font_size, 14);
    }

    /// A queued background change must land on disk and survive a reload:
    /// this is the contract every settings control relies on.
    #[test]
    fn queued_updates_reach_disk_and_survive_a_reload() {
        let _guard = SETTINGS_IO_LOCK.lock();
        let dir = std::env::temp_dir().join(format!(
            "maple-agent-settings-roundtrip-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };

        update_settings_in_background(|settings| settings.tts_voice = "neutral_male".into());
        // A second queued change must not lose the first: both go through
        // the writer thread in order.
        update_settings_in_background(|settings| settings.tts_speed = 1.5);
        update_settings_and_wait(|_| {});

        let reloaded = load_settings();
        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(reloaded.tts_voice, "neutral_male");
        assert!((reloaded.tts_speed - 1.5).abs() < 0.01);
    }

    /// Without an explicit XDG_CONFIG_HOME, a test build must resolve the
    /// settings file outside the developer's real config directory, or
    /// background writes from widget tests would rewrite it.
    #[test]
    fn unit_tests_never_resolve_the_real_settings_file() {
        let _guard = SETTINGS_IO_LOCK.lock();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };

        let path = settings_file();

        if let Some(value) = previous {
            unsafe { std::env::set_var("XDG_CONFIG_HOME", value) };
        }

        assert!(
            !path.starts_with(dirs::config_dir().unwrap_or_default()),
            "test resolution must not touch the real config root: {}",
            path.display()
        );
    }

    #[test]
    fn existing_settings_files_default_shortcut_overrides_to_empty() {
        let mut json = serde_json::to_value(AppSettings::default()).expect("serialize");
        json.as_object_mut()
            .expect("settings object")
            .remove("shortcut_overrides");
        let settings: AppSettings = serde_json::from_value(json).expect("deserialize old file");
        assert!(settings.shortcut_overrides.is_empty());
    }

    #[test]
    fn shortcut_overrides_distinguish_remapped_disabled_and_default_slots() {
        let mut settings = AppSettings::default();
        settings
            .shortcut_overrides
            .insert("chat.new_task".into(), Some("secondary-shift-n".into()));
        settings
            .shortcut_overrides
            .insert("chat.focus_search".into(), None);

        let json = serde_json::to_value(&settings).expect("serialize");
        assert_eq!(
            json["shortcut_overrides"]["chat.new_task"],
            "secondary-shift-n"
        );
        assert!(json["shortcut_overrides"]["chat.focus_search"].is_null());
        assert!(
            json["shortcut_overrides"]
                .get("chat.toggle_sidebar")
                .is_none()
        );

        let restored: AppSettings = serde_json::from_value(json).expect("deserialize");
        assert_eq!(restored.shortcut_overrides, settings.shortcut_overrides);
    }
}
