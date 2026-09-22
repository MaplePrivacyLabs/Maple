//! App settings persisted to ~/.config/maple-agent/settings.json and local
//! usage aggregation read from the goose usage ledger.

// This module is the desktop frontend's boundary. A headless build (no
// `desktop` feature) uses only a few entry points, so the rest is unused
// there by design.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppSettings {
    /// Default permission policy for new sessions; see [`PermissionMode`].
    #[serde(default)]
    pub default_permission_mode: PermissionMode,
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
    /// Whether run completion, permissions, and questions raise desktop
    /// notifications while the window is not focused.
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

/// Permission policy for a session: whether a gated tool call needs a
/// decision from the user. Modelled on [`crate::ui::theme::Preference`],
/// including the same infallible `parse` so an unknown value on disk
/// degrades to the safer mode instead of failing the whole settings load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// Confirm each gated tool call.
    #[default]
    SmartApprove,
    /// Approve every tool call without asking.
    Auto,
}

impl PermissionMode {
    /// Anything unknown reads as the safer mode.
    pub fn parse(value: &str) -> Self {
        match value {
            "auto" => Self::Auto,
            _ => Self::SmartApprove,
        }
    }

    /// The mode named by `value`, or `None` when it names no mode. Use
    /// this where an unknown value must fall back to a saved default
    /// rather than to the safer mode.
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "smart_approve" => Some(Self::SmartApprove),
            _ => None,
        }
    }

    /// Icon name: a bolt for allow all, a shield for ask first.
    pub fn icon(self) -> &'static str {
        match self {
            Self::SmartApprove => "shield-check",
            Self::Auto => "zap",
        }
    }

    /// The value written to disk and handed to the agent runtime.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SmartApprove => "smart_approve",
            Self::Auto => "auto",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SmartApprove => "Ask first",
            Self::Auto => "Allow all",
        }
    }

    /// One line of explanation under the label.
    pub fn note(self) -> &'static str {
        match self {
            Self::SmartApprove => "Confirm each gated tool call",
            Self::Auto => "Approve every tool call without asking",
        }
    }
}

// Serialized as the bare string it has always been, so settings.json and
// the runtime's mode field keep their format.
impl serde::Serialize for PermissionMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for PermissionMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::parse(&value))
    }
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
            default_permission_mode: PermissionMode::default(),
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

/// One aggregated usage row: per session or per model.
#[derive(Debug, Clone, Default)]
pub struct UsageRow {
    pub label: String,
    pub sessions: u64,
    pub turns: u64,
    pub total_tokens: i64,
    pub cost: f64,
}

#[derive(Debug, Clone, Default)]
pub struct UsageSummary {
    pub totals: UsageRow,
    pub by_model: Vec<UsageRow>,
    pub by_session: Vec<UsageRow>,
}

/// Read usage totals from the goose usage ledger for one account scope.
pub fn load_usage(account_scope: &str) -> UsageSummary {
    let db = crate::backend::account_session_db(account_scope);
    let Some(conn) = crate::backend::open_session_db_read_only(&db) else {
        return UsageSummary::default();
    };
    usage_from_ledger(&conn)
}

/// Aggregate one account's ledger.
///
/// A subagent has a session of its own, and its provider calls land in
/// the ledger under it. Every row counts against the task that delegated
/// the work, so the reader sees what a task cost in total. Goose refuses
/// a subagent of a subagent, so resolving one parent is enough.
fn usage_from_ledger(conn: &rusqlite::Connection) -> UsageSummary {
    let mut summary = UsageSummary::default();

    if let Ok(mut stmt) = conn.prepare(
        "SELECT COUNT(*), COALESCE(SUM(total_tokens),0), COALESCE(SUM(cost),0) \
         FROM usage_ledger",
    ) && let Ok(row) = stmt.query_row([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
        ))
    }) {
        summary.totals = UsageRow {
            label: "All activity".to_string(),
            sessions: 0,
            turns: row.0.max(0) as u64,
            total_tokens: row.1,
            cost: row.2,
        };
    }

    if let Ok(mut stmt) = conn.prepare(
        "SELECT u.model, COUNT(DISTINCT COALESCE(s.parent_session_id, u.session_id)), COUNT(*), \
         COALESCE(SUM(u.total_tokens),0), COALESCE(SUM(u.cost),0) \
         FROM usage_ledger u LEFT JOIN sessions s ON s.id = u.session_id \
         GROUP BY u.model ORDER BY SUM(u.total_tokens) DESC",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        Ok(UsageRow {
            label: row
                .get::<_, Option<String>>(0)?
                .unwrap_or_else(|| "unknown".into()),
            sessions: row.get::<_, i64>(1)?.max(0) as u64,
            turns: row.get::<_, i64>(2)?.max(0) as u64,
            total_tokens: row.get::<_, i64>(3)?,
            cost: row.get::<_, f64>(4)?,
        })
    }) {
        for row in rows.flatten() {
            summary.totals.sessions += row.sessions;
            summary.by_model.push(row);
        }
    }

    if let Ok(mut stmt) = conn.prepare(
        "SELECT COALESCE(parent.name, s.name), COALESCE(s.parent_session_id, u.session_id) AS task, \
         COUNT(*), COALESCE(SUM(u.total_tokens),0), COALESCE(SUM(u.cost),0) \
         FROM usage_ledger u JOIN sessions s ON s.id = u.session_id \
         LEFT JOIN sessions parent ON parent.id = s.parent_session_id \
         GROUP BY task ORDER BY MAX(u.created_timestamp) DESC LIMIT 20",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        Ok(UsageRow {
            label: {
                let name: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
                let id: String = row.get(1)?;
                if name.trim().is_empty() { id } else { name }
            },
            sessions: 1,
            turns: row.get::<_, i64>(2)?.max(0) as u64,
            total_tokens: row.get::<_, i64>(3)?,
            cost: row.get::<_, f64>(4)?,
        })
    }) {
        for row in rows.flatten() {
            summary.by_session.push(row);
        }
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A subagent bills to the task that delegated the work, so the
    /// usage screen shows one row per task and not one per subagent.
    #[test]
    fn subagent_usage_counts_against_its_parent_task() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL DEFAULT '',
                 parent_session_id TEXT
             );
             CREATE TABLE usage_ledger (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL,
                 created_timestamp INTEGER NOT NULL,
                 model TEXT,
                 total_tokens INTEGER,
                 cost REAL
             );
             INSERT INTO sessions VALUES ('task-1', 'Review the parser', NULL);
             INSERT INTO sessions VALUES ('sub-1', 'Delegated task', 'task-1');
             INSERT INTO sessions VALUES ('task-2', 'Other work', NULL);
             INSERT INTO usage_ledger (session_id, created_timestamp, model, total_tokens, cost)
             VALUES ('task-1', 10, 'maple-1', 100, 1.0),
                    ('sub-1',  20, 'maple-1', 400, 4.0),
                    ('task-2', 30, 'maple-1', 700, 7.0);",
        )
        .unwrap();

        let usage = usage_from_ledger(&conn);
        let rows = usage
            .by_session
            .iter()
            .map(|row| (row.label.as_str(), row.turns, row.total_tokens))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![("Other work", 1, 700), ("Review the parser", 2, 500)],
            "the subagent's tokens belong to the task that delegated them"
        );
        // Two tasks ran, not three sessions.
        assert_eq!(usage.by_model.len(), 1);
        assert_eq!(usage.by_model[0].sessions, 2);
        assert_eq!(usage.totals.total_tokens, 1200);
    }

    #[test]
    fn permission_mode_round_trips_as_a_string() {
        for mode in [PermissionMode::SmartApprove, PermissionMode::Auto] {
            let json = serde_json::to_string(&mode).expect("serialize");
            assert_eq!(json, format!("\"{}\"", mode.as_str()));
            let back: PermissionMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn unknown_permission_mode_reads_as_the_safer_one() {
        assert_eq!(PermissionMode::parse("chat"), PermissionMode::SmartApprove);
        assert_eq!(PermissionMode::parse(""), PermissionMode::SmartApprove);
        assert_eq!(PermissionMode::default(), PermissionMode::SmartApprove);
    }

    #[test]
    fn default_settings_keep_the_on_disk_permission_string() {
        let json = serde_json::to_value(AppSettings::default()).expect("serialize");
        assert_eq!(json["default_permission_mode"], "smart_approve");
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
