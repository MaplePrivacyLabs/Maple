//! GUI settings cache, synchronized through the host-owned SQLite store.

// This module is the desktop frontend's boundary. A headless build (no
// `desktop` feature) uses only a few entry points, so the rest is unused
// there by design.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RemoteSidebarSettings {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub pinned_tasks: Vec<String>,
}

/// How the sidebar groups and orders tasks: its View options menu.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SidebarView {
    #[serde(deserialize_with = "view_preference_or_default")]
    pub group: SidebarGroup,
    #[serde(deserialize_with = "view_preference_or_default")]
    pub sort: SidebarSort,
}

// View state is optional presentation data. Unknown values (for example after
// a downgrade) or malformed fields must not prevent the host from starting.
fn view_preference_or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde::Deserialize::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarGroup {
    Connection,
    Status,
    List,
    #[default]
    #[serde(other)]
    Project,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarSort {
    Manual,
    #[default]
    #[serde(other)]
    Updated,
}

/// Stable presentation identity; a remote task or project belongs to its computer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SidebarItemKey {
    pub computer: Option<String>,
    pub id: String,
}

/// Account-owned controller state. Project paths never enter global UI settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SidebarOrder {
    pub projects: Vec<SidebarItemKey>,
    pub connections: Vec<Option<String>>,
    pub tasks: Vec<SidebarItemKey>,
    /// Pinned presentation order is independent of each task's ordinary slot.
    pub pinned_tasks: Vec<SidebarItemKey>,
    pub created_before_ms: i64,
}

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
    /// Transient compatibility field; authoritative names come from the host store.
    #[serde(default)]
    pub project_names: std::collections::HashMap<String, String>,
    /// Local presentation preferences for each paired endpoint. A remote
    /// `/work/project` is not the same project as that path on this computer.
    #[serde(default)]
    pub remote_sidebars: std::collections::HashMap<String, RemoteSidebarSettings>,
    #[serde(default, deserialize_with = "view_preference_or_default")]
    pub sidebar_layout: SidebarView,
    /// Whether run completion and questions raise desktop notifications
    /// while the window is not focused.
    #[serde(default = "default_desktop_notifications")]
    pub desktop_notifications: bool,
    /// Background downloads are optional; installation always needs confirmation.
    #[serde(default = "default_automatic_update_downloads")]
    pub automatic_update_downloads: bool,
    /// Explicit permission to send task titles and questions to the operating
    /// system's notification history while the local store uses a password.
    #[serde(default)]
    pub password_notification_details: bool,
    /// Read-only host snapshot metadata, never a persisted UI preference. An
    /// absent verified mode must keep notification contents private.
    #[serde(default = "default_store_password_mode")]
    pub store_password_mode: bool,
    /// Skip looping and reveal animations. gpui reads no OS preference for
    /// this, so it is a Maple setting.
    #[serde(default)]
    pub reduce_motion: bool,
    /// The user's additional instructions, appended after
    /// [`MAPLE_HARNESS_INSTRUCTIONS`]. Empty means none.
    #[serde(default)]
    pub harness_instructions: String,
    /// Window size and state from the last run.
    #[serde(default)]
    pub window: Option<WindowState>,
    /// UI language override; None follows the operating system.
    #[serde(default, deserialize_with = "view_preference_or_default")]
    pub locale: Option<String>,
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

/// Persisted window geometry.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowState {
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
    /// Where the window was, when the platform reports it. Wayland does
    /// not, so the window manager places the window there.
    #[serde(default)]
    pub position: Option<WindowPosition>,
}

/// A window origin relative to one display. Restoring it is skipped when
/// that display is gone, so a monitor change cannot open the window
/// off-screen.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowPosition {
    pub x: f32,
    pub y: f32,
    /// The display's persistent UUID.
    pub display: String,
}

impl WindowState {
    /// Keep a saved size inside a sane range so a corrupt file cannot
    /// open a window too small to use.
    pub fn clamped(self) -> Self {
        Self {
            width: self.width.clamp(640., 8192.),
            height: self.height.clamp(480., 8192.),
            ..self
        }
    }
}

/// Opening system prompt for agents this app hosts: the agent is Maple.
/// Always sent; users add to it through their additional instructions and
/// cannot replace it. The runtime appends its tool and runtime guidance.
pub const MAPLE_HARNESS_INSTRUCTIONS: &str =
    "You are a general-purpose AI agent called Maple, created by Maple AI.
You run in the Maple app's Agent Mode; users know you simply as Maple.
If the user asks about Maple itself, such as its features, models, plans, privacy, or how to use it, you can learn more about yourself from Maple's official website at https://maple.ai and its documentation at https://maple.ai/docs. When web tools are available, open the relevant maple.ai pages before answering; otherwise, point the user to those links rather than guessing.";

impl AppSettings {
    pub fn notification_details_enabled(&self) -> bool {
        !self.store_password_mode || self.password_notification_details
    }

    /// The harness instructions to hand the runtime: Maple's own, then the
    /// user's additional instructions when any are saved.
    pub fn effective_harness_instructions(&self) -> String {
        let additional = self.harness_instructions.trim();
        if additional.is_empty() {
            MAPLE_HARNESS_INSTRUCTIONS.to_string()
        } else {
            format!("{MAPLE_HARNESS_INSTRUCTIONS}\n\n{additional}")
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

fn default_automatic_update_downloads() -> bool {
    true
}

fn default_store_password_mode() -> bool {
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
            remote_sidebars: std::collections::HashMap::new(),
            sidebar_layout: SidebarView::default(),
            desktop_notifications: default_desktop_notifications(),
            automatic_update_downloads: default_automatic_update_downloads(),
            password_notification_details: false,
            store_password_mode: default_store_password_mode(),
            reduce_motion: false,
            harness_instructions: String::new(),
            window: None,
            locale: None,
            theme: default_theme(),
            chat_font_family: default_chat_font_family(),
            chat_font_size: default_chat_font_size(),
            tts_voice: default_tts_voice(),
            tts_speed: default_tts_speed(),
        }
    }
}

/// Settings are a GUI cache. Only the host reads or writes their store rows.
#[derive(Clone)]
enum SettingsSink {
    Host(std::sync::Weak<crate::backend::AgentBackend>),
    #[cfg(test)]
    Test(std::sync::Arc<maple_agent::store::Store>),
}

#[derive(Clone)]
struct PendingSidebarOrder {
    backend: std::sync::Arc<crate::backend::AgentBackend>,
    order: SidebarOrder,
}

struct SettingsCache {
    generation: u64,
    value: AppSettings,
    pending: serde_json::Map<String, serde_json::Value>,
    words: std::collections::BTreeSet<String>,
    /// Shared account snapshots keep cached screens from replaying stale order.
    sidebar_orders: std::collections::BTreeMap<String, SidebarOrder>,
    /// Account-owned writes stay in memory and never enter global UI settings.
    pending_sidebar_orders: std::collections::BTreeMap<String, PendingSidebarOrder>,
    sink: Option<SettingsSink>,
}

static CACHE: std::sync::LazyLock<std::sync::Mutex<SettingsCache>> =
    std::sync::LazyLock::new(|| {
        std::sync::Mutex::new(SettingsCache {
            generation: 0,
            value: AppSettings::default(),
            pending: Default::default(),
            words: Default::default(),
            sidebar_orders: Default::default(),
            pending_sidebar_orders: Default::default(),
            sink: None,
        })
    });

/// Install the host snapshot before constructing the main window. Pending
/// edits survive a reconnect and are replayed over the fresh host snapshot.
pub fn attach_backend(backend: std::sync::Arc<crate::backend::AgentBackend>, value: AppSettings) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.pending.remove("store_password_mode");
    let mut json = serde_json::to_value(value).expect("settings serialize");
    merge_patch(&mut json, &serde_json::Value::Object(cache.pending.clone()));
    cache.value = serde_json::from_value(json).unwrap_or_default();
    cache.generation = cache.generation.wrapping_add(1);
    cache.sink = Some(SettingsSink::Host(std::sync::Arc::downgrade(&backend)));
    drop(cache);
    queue_flush(None);
}

pub fn clear_decrypted_cache() {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.value = AppSettings::default();
    // Account-owned paths cannot survive losing the verified account/store
    // context. Normal navigation and Quit flush them before this boundary.
    cache.pending_sidebar_orders.clear();
    cache.sidebar_orders.clear();
    // Retain unsynchronized edits privately for this generation, but stop
    // retries until an Open snapshot supplies a fresh host attachment.
    cache.sink = None;
    cache.generation = cache.generation.wrapping_add(1);
}

/// A destructive local reset or a different verified store generation must
/// not replay edits from the retired database into the new one.
pub fn discard_decrypted_cache() {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.value = AppSettings::default();
    cache.pending.clear();
    cache.words.clear();
    cache.pending_sidebar_orders.clear();
    cache.sidebar_orders.clear();
    cache.sink = None;
    cache.generation = cache.generation.wrapping_add(1);
}

pub fn load_settings() -> AppSettings {
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .value
        .clone()
}

fn value_patch(before: &serde_json::Value, after: &serde_json::Value) -> Option<serde_json::Value> {
    if before == after {
        return None;
    }
    if let (Some(before), Some(after)) = (before.as_object(), after.as_object()) {
        let mut patch = serde_json::Map::new();
        for (key, value) in after {
            // Missing and explicit null differ: null disables an inherited
            // shortcut, whereas an absent key retains the shipped binding.
            let changed = match before.get(key) {
                Some(previous) => value_patch(previous, value),
                None => Some(value.clone()),
            };
            if let Some(changed) = changed {
                patch.insert(key.clone(), changed);
            }
        }
        for key in before.keys().filter(|key| !after.contains_key(*key)) {
            patch.insert(key.clone(), serde_json::json!({"$unset":true}));
        }
        Some(serde_json::Value::Object(patch))
    } else {
        Some(after.clone())
    }
}

fn compose_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    if patch.get("$unset") == Some(&serde_json::Value::Bool(true)) {
        *target = patch.clone();
    } else if let Some(fields) = patch.as_object() {
        if !target.is_object() {
            *target = serde_json::json!({});
        }
        for (key, value) in fields {
            compose_patch(
                target
                    .as_object_mut()
                    .expect("object")
                    .entry(key.clone())
                    .or_insert(serde_json::Value::Null),
                value,
            );
        }
    } else {
        *target = patch.clone();
    }
}

fn edit_cache(update: impl FnOnce(&mut AppSettings)) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let before = serde_json::to_value(&cache.value).expect("settings serialize");
    let store_password_mode = cache.value.store_password_mode;
    update(&mut cache.value);
    cache.value.store_password_mode = store_password_mode;
    cache.value.pinned_roots.clear();
    let after = serde_json::to_value(&cache.value).expect("settings serialize");
    let mut patch = value_patch(&before, &after).unwrap_or_else(|| serde_json::json!({}));
    // Account presentation state is owned by dedicated paged/account APIs.
    // These legacy struct fields are only transient UI compatibility values.
    for key in [
        "pinned_roots",
        "pinned_tasks",
        "project_names",
        "remote_sidebars",
    ] {
        patch.as_object_mut().expect("settings patch").remove(key);
    }
    let mut pending = serde_json::Value::Object(std::mem::take(&mut cache.pending));
    compose_patch(&mut pending, &patch);
    cache.pending = pending.as_object_mut().expect("settings patch").clone();
}

pub fn update_settings_in_background(update: impl FnOnce(&mut AppSettings) + Send + 'static) {
    edit_cache(update);
    queue_flush(None);
}

pub(crate) fn cached_sidebar_order(user: &str) -> Option<SidebarOrder> {
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sidebar_orders
        .get(user)
        .cloned()
}

/// A late host reply cannot replace ordering already edited by another screen.
pub(crate) fn install_sidebar_order(user: String, order: SidebarOrder) -> SidebarOrder {
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sidebar_orders
        .entry(user)
        .or_insert(order)
        .clone()
}

/// Retire only this account's decrypted presentation state and queued writes.
pub(crate) fn retire_sidebar_account(user: &str) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.sidebar_orders.remove(user);
    cache.pending_sidebar_orders.remove(user);
}

/// Queue account-owned ordering through the same Quit barrier as
/// ordinary settings. The captured account/backend are never rebound on retry.
pub(crate) fn save_sidebar_order(
    backend: std::sync::Arc<crate::backend::AgentBackend>,
    user: String,
    order: SidebarOrder,
) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.sidebar_orders.insert(user.clone(), order.clone());
    cache
        .pending_sidebar_orders
        .insert(user, PendingSidebarOrder { backend, order });
    drop(cache);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    queue_flush(Some(SettingsAck::Async(sender)));
    receiver
}

/// A synchronous barrier for existing GUI state tests. Runtime callers use
/// the asynchronous flush_pending barrier before ordinary Quit.
#[cfg(test)]
pub(crate) fn update_settings_and_wait(update: impl FnOnce(&mut AppSettings) + Send + 'static) {
    edit_cache(update);
    let (sender, receiver) = std::sync::mpsc::channel();
    queue_flush(Some(SettingsAck::Sync(sender)));
    let _ = receiver.recv_timeout(std::time::Duration::from_secs(5));
}

pub fn save_window_state(state: WindowState) {
    update_settings_in_background(move |settings| settings.window = Some(state));
}

pub async fn flush_pending() -> Result<(), String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    queue_flush(Some(SettingsAck::Async(sender)));
    receiver
        .await
        .map_err(|_| "Maple settings synchronization stopped".to_string())?
}

pub(crate) fn pending_dictionary_words() -> Vec<String> {
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .words
        .iter()
        .cloned()
        .collect()
}

pub(crate) fn queue_dictionary_word(word: String) {
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .words
        .insert(word);
    queue_flush(None);
}

enum SettingsAck {
    #[cfg(test)]
    Sync(std::sync::mpsc::Sender<Result<(), String>>),
    Async(tokio::sync::oneshot::Sender<Result<(), String>>),
}

fn queue_flush(ack: Option<SettingsAck>) {
    static WRITER: std::sync::OnceLock<std::sync::mpsc::Sender<Option<SettingsAck>>> =
        std::sync::OnceLock::new();
    let sender = WRITER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<Option<SettingsAck>>();
        std::thread::Builder::new()
            .name("settings-host-sync".into())
            .spawn(move || {
                loop {
                    let first = match receiver.recv_timeout(std::time::Duration::from_secs(2)) {
                        Ok(ack) => ack,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let mut acks = first.into_iter().collect::<Vec<_>>();
                    while let Ok(ack) = receiver.try_recv() {
                        acks.extend(ack);
                    }
                    let result = synchronize_once();
                    for ack in acks {
                        match ack {
                            #[cfg(test)]
                            SettingsAck::Sync(sender) => {
                                let _ = sender.send(result.clone());
                            }
                            SettingsAck::Async(sender) => {
                                let _ = sender.send(result.clone());
                            }
                        }
                    }
                }
            })
            .expect("settings sync worker");
        sender
    });
    if sender.send(ack).is_err() {
        log::error!("Settings synchronization stopped; edits remain in memory");
    }
}

fn synchronize_once() -> Result<(), String> {
    let (generation, sink, patch, words, sidebar_orders) = {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if cache.pending.is_empty()
            && cache.words.is_empty()
            && cache.pending_sidebar_orders.is_empty()
        {
            return Ok(());
        }
        (
            cache.generation,
            cache.sink.clone(),
            cache.pending.clone(),
            cache.words.clone(),
            cache.pending_sidebar_orders.clone(),
        )
    };
    match sink.ok_or("Maple host is unavailable; settings edits remain pending")? {
        SettingsSink::Host(backend) => {
            let backend = backend
                .upgrade()
                .ok_or("Maple host is unavailable; settings edits remain pending")?;
            backend.runtime_handle().block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    if !patch.is_empty() {
                        backend
                            .set_ui_settings(serde_json::Value::Object(patch.clone()))
                            .await?;
                    }
                    for word in &words {
                        backend.add_word(word.clone()).await?;
                    }
                    backend.flush_ui_settings().await
                })
                .await
                .map_err(|_| "Maple settings synchronization timed out".to_string())?
            })?;
        }
        #[cfg(test)]
        SettingsSink::Test(store) => {
            apply_host_patch(&store, None, &serde_json::Value::Object(patch.clone()))?;
            for word in &words {
                store.add_dictionary_word(word).map_err(|e| e.to_string())?;
            }
        }
    }
    {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if cache.generation != generation {
            return if sidebar_orders.is_empty() {
                Ok(())
            } else {
                Err("Maple settings state changed; sidebar order edits remain pending".into())
            };
        }
        for (key, value) in patch {
            if cache.pending.get(&key) == Some(&value) {
                cache.pending.remove(&key);
            }
        }
        for word in words {
            cache.words.remove(&word);
        }
    }
    let mut first_error = None;
    for (user, pending) in sidebar_orders {
        {
            let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if cache.generation != generation || cache.sink.is_none() {
                first_error.get_or_insert_with(|| {
                    "Maple settings state changed; sidebar order edits remain pending".to_string()
                });
                break;
            }
        }
        let result = pending.backend.runtime_handle().block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                pending
                    .backend
                    .set_sidebar_order(&user, pending.order.clone()),
            )
            .await
            .map_err(|_| "Maple sidebar order synchronization timed out".to_string())?
        });
        match result {
            Ok(()) => {
                let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                if cache.generation == generation
                    && cache
                        .pending_sidebar_orders
                        .get(&user)
                        .is_some_and(|current| {
                            current.order == pending.order
                                && std::sync::Arc::ptr_eq(&current.backend, &pending.backend)
                        })
                {
                    cache.pending_sidebar_orders.remove(&user);
                }
            }
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    if let Some(patch) = patch.as_object() {
        if !target.is_object() {
            *target = serde_json::json!({});
        }
        let target = target.as_object_mut().expect("object installed");
        for (key, value) in patch {
            if value.get("$unset") == Some(&serde_json::Value::Bool(true)) {
                target.remove(key);
            } else {
                merge_patch(
                    target.entry(key.clone()).or_insert(serde_json::Value::Null),
                    value,
                );
            }
        }
    } else {
        *target = patch.clone();
    }
}

#[cfg(test)]
fn remote_sidebar_patch(
    before: &RemoteSidebarSettings,
    after: &RemoteSidebarSettings,
) -> serde_json::Value {
    let mut patch = value_patch(
        &serde_json::to_value(before).expect("sidebar serialize"),
        &serde_json::to_value(after).expect("sidebar serialize"),
    )
    .unwrap_or_else(|| serde_json::json!({}));
    if before.pinned_tasks != after.pinned_tasks {
        let old: std::collections::HashSet<_> = before.pinned_tasks.iter().collect();
        let new: std::collections::HashSet<_> = after.pinned_tasks.iter().collect();
        patch["pinned_tasks"] = serde_json::json!({
            "add": after.pinned_tasks.iter().filter(|id| !old.contains(id)).collect::<Vec<_>>(),
            "remove": before.pinned_tasks.iter().filter(|id| !new.contains(id)).collect::<Vec<_>>(),
        });
    }
    patch
}

pub(crate) fn patch_remote_sidebar(
    value: &mut RemoteSidebarSettings,
    patch: &serde_json::Value,
) -> Result<(), String> {
    let fields = patch
        .as_object()
        .ok_or("Remote sidebar patch must be an object")?;
    if fields
        .keys()
        .any(|key| !["display_name", "pinned_tasks"].contains(&key.as_str()))
    {
        return Err("Unknown remote sidebar setting".into());
    }
    let mut patch = patch.clone();
    if let Some(operations) = patch
        .get("pinned_tasks")
        .and_then(serde_json::Value::as_object)
    {
        if operations
            .keys()
            .any(|key| !["add", "remove"].contains(&key.as_str()))
        {
            return Err("Invalid pin operation".into());
        }
        let add: Vec<String> = serde_json::from_value(
            operations
                .get("add")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        )
        .map_err(|_| "Invalid pin additions")?;
        let remove: Vec<String> = serde_json::from_value(
            operations
                .get("remove")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        )
        .map_err(|_| "Invalid pin removals")?;
        let mut pins = value.pinned_tasks.clone();
        pins.retain(|id| !remove.contains(id));
        for id in add {
            if !pins.contains(&id) {
                pins.push(id);
            }
        }
        patch["pinned_tasks"] = serde_json::json!(pins);
    }
    let mut json =
        serde_json::to_value(&*value).map_err(|_| "Could not encode remote sidebar settings")?;
    merge_patch(&mut json, &patch);
    for (key, empty) in [
        ("display_name", serde_json::json!("")),
        ("pinned_tasks", serde_json::json!([])),
    ] {
        if json.get(key).is_some_and(serde_json::Value::is_null) {
            json[key] = empty;
        }
    }
    *value = serde_json::from_value(json).map_err(|_| "Invalid remote sidebar setting value")?;
    Ok(())
}

pub(crate) fn timestamp_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub(crate) fn load_host_settings(
    store: &maple_agent::store::Store,
    _account: Option<maple_agent::store::AccountId>,
) -> Result<AppSettings, String> {
    let value = store
        .app_setting("ui")
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| serde_json::json!({}));
    let mut settings: AppSettings =
        serde_json::from_value(value).map_err(|_| "Saved UI settings are invalid".to_string())?;
    settings.store_password_mode = store.lock_status().map_err(|error| error.to_string())?.mode
        == maple_agent::store::lifecycle::StoreMode::Password;
    settings.pinned_roots.clear();
    settings.pinned_tasks.clear();
    settings.project_names.clear();
    settings.remote_sidebars.clear();
    if let Some(value) = store
        .app_setting("agent.harness")
        .map_err(|e| e.to_string())?
    {
        settings.harness_instructions = value
            .as_str()
            .ok_or("Saved harness settings are invalid")?
            .to_owned();
    }
    if let Some(value) = store
        .app_setting("notifications")
        .map_err(|e| e.to_string())?
    {
        settings.desktop_notifications = value
            .as_bool()
            .ok_or("Saved notification settings are invalid")?;
    }
    Ok(settings)
}

pub(crate) fn apply_host_patch(
    store: &maple_agent::store::Store,
    account: Option<maple_agent::store::AccountId>,
    patch: &serde_json::Value,
) -> Result<(), String> {
    let fields = patch
        .as_object()
        .ok_or("Settings patch must be an object")?;
    if fields.contains_key("store_password_mode") {
        return Err("Store mode is read-only host metadata".into());
    }
    let before = load_host_settings(store, account)?;
    let mut json = serde_json::to_value(&before).map_err(|_| "Could not encode UI settings")?;
    for key in fields.keys() {
        if json.get(key).is_none() {
            return Err("Unknown UI setting".into());
        }
    }
    merge_patch(&mut json, patch);
    let mut settings: AppSettings =
        serde_json::from_value(json).map_err(|_| "Invalid UI setting value")?;
    settings.pinned_roots.clear();
    let now = timestamp_ms();
    if [
        "pinned_roots",
        "pinned_tasks",
        "project_names",
        "remote_sidebars",
    ]
    .iter()
    .any(|key| fields.contains_key(*key))
    {
        return Err("Account presentation settings require their dedicated host operation".into());
    }
    let mut global = serde_json::to_value(&settings).map_err(|_| "Could not encode UI settings")?;
    for key in [
        "pinned_roots",
        "pinned_tasks",
        "project_names",
        "remote_sidebars",
        "harness_instructions",
        "desktop_notifications",
        "store_password_mode",
    ] {
        global.as_object_mut().expect("settings object").remove(key);
    }
    store
        .set_app_setting("ui", &global, now)
        .map_err(|e| e.to_string())?;
    if fields.contains_key("harness_instructions") {
        store
            .set_app_setting(
                "agent.harness",
                &serde_json::Value::String(settings.harness_instructions),
                now,
            )
            .map_err(|e| e.to_string())?;
    }
    if fields.contains_key("desktop_notifications") {
        store
            .set_app_setting(
                "notifications",
                &serde_json::Value::Bool(settings.desktop_notifications),
                now,
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) static SETTINGS_IO_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[cfg(test)]
pub(crate) fn install_test_settings(value: AppSettings) {
    let mut cache = CACHE.lock().unwrap();
    let generation = cache.generation.wrapping_add(1);
    *cache = SettingsCache {
        generation,
        value,
        pending: Default::default(),
        words: Default::default(),
        sidebar_orders: Default::default(),
        pending_sidebar_orders: Default::default(),
        sink: None,
    };
}

#[cfg(test)]
pub(crate) fn install_test_store(store: std::sync::Arc<maple_agent::store::Store>) {
    let value = load_host_settings(&store, None).unwrap();
    let mut cache = CACHE.lock().unwrap();
    let generation = cache.generation.wrapping_add(1);
    *cache = SettingsCache {
        generation,
        value,
        pending: Default::default(),
        words: Default::default(),
        sidebar_orders: Default::default(),
        pending_sidebar_orders: Default::default(),
        sink: Some(SettingsSink::Test(store)),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_preferences_are_optional_and_survive_host_patches() {
        for value in [serde_json::json!({}), serde_json::json!({"locale": 7})] {
            let settings: AppSettings = serde_json::from_value(value).unwrap();
            assert_eq!(settings.locale, None);
        }
        let root = tempfile::tempdir().unwrap();
        let store =
            maple_agent::store::Store::open(maple_agent::store::StorePaths::new(root.path()))
                .unwrap();
        for locale in [Some("en"), Some("future-locale"), None] {
            apply_host_patch(&store, None, &serde_json::json!({"locale": locale})).unwrap();
            apply_host_patch(&store, None, &serde_json::json!({"reduce_motion": true})).unwrap();
            assert_eq!(
                load_host_settings(&store, None).unwrap().locale.as_deref(),
                locale
            );
            assert_eq!(
                store.app_setting("ui").unwrap().unwrap()["locale"],
                serde_json::json!(locale)
            );
        }
    }

    #[test]
    fn the_sidebar_layout_round_trips_and_defaults_to_projects() {
        let settings: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.sidebar_layout, SidebarView::default());
        assert_eq!(settings.sidebar_layout.group, SidebarGroup::Project);
        let view = SidebarView {
            group: SidebarGroup::List,
            sort: SidebarSort::Manual,
        };
        let json = serde_json::to_value(AppSettings {
            sidebar_layout: view,
            ..AppSettings::default()
        })
        .unwrap();
        assert_eq!(json["sidebar_layout"]["group"], "list");
        let settings: AppSettings = serde_json::from_value(json).unwrap();
        assert_eq!(settings.sidebar_layout, view);
    }

    #[test]
    fn sidebar_layout_defaults_unknown_and_invalid_choices_independently() {
        for invalid in [
            serde_json::json!("future-choice"),
            serde_json::json!(null),
            serde_json::json!(false),
            serde_json::json!(42),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let settings: AppSettings = serde_json::from_value(serde_json::json!({
                "sidebar_layout": {"group": invalid, "sort": "manual"},
                "theme": "dark"
            }))
            .unwrap();
            assert_eq!(settings.sidebar_layout.group, SidebarGroup::Project);
            assert_eq!(settings.sidebar_layout.sort, SidebarSort::Manual);
            assert_eq!(settings.theme, "dark");

            let settings: AppSettings = serde_json::from_value(serde_json::json!({
                "sidebar_layout": {"group": "connection", "sort": invalid}
            }))
            .unwrap();
            assert_eq!(settings.sidebar_layout.group, SidebarGroup::Connection);
            assert_eq!(settings.sidebar_layout.sort, SidebarSort::Updated);

            let settings: AppSettings = serde_json::from_value(serde_json::json!({
                "sidebar_layout": invalid
            }))
            .unwrap();
            assert_eq!(settings.sidebar_layout, SidebarView::default());
        }
    }

    #[test]
    fn invalid_view_preferences_do_not_block_host_settings_or_later_edits() {
        let root = tempfile::tempdir().unwrap();
        let store =
            maple_agent::store::Store::open(maple_agent::store::StorePaths::new(root.path()))
                .unwrap();
        for invalid in [
            serde_json::json!({"group": "future-group", "sort": "future-sort"}),
            serde_json::json!({"group": 7, "sort": {"invalid": true}}),
            serde_json::Value::Null,
        ] {
            store
                .set_app_setting(
                    "ui",
                    &serde_json::json!({"sidebar_layout": invalid, "theme": "dark"}),
                    1,
                )
                .unwrap();
            let loaded = load_host_settings(&store, None).unwrap();
            assert_eq!(loaded.sidebar_layout, SidebarView::default());
            assert_eq!(loaded.theme, "dark");
            apply_host_patch(&store, None, &serde_json::json!({"reduce_motion": true})).unwrap();
            let loaded = load_host_settings(&store, None).unwrap();
            assert!(loaded.reduce_motion);
            assert_eq!(loaded.theme, "dark");
            assert_eq!(loaded.sidebar_layout, SidebarView::default());
        }
    }

    #[test]
    fn remote_sidebar_preferences_cannot_override_owner_project_names() {
        let mut settings: RemoteSidebarSettings = serde_json::from_str(
            r#"{"display_name":"Lab","pinned_tasks":["task-1"],"project_names":{"/work/one":"Old alias"}}"#,
        )
        .unwrap();
        assert_eq!(settings.pinned_tasks, ["task-1"]);
        assert!(
            serde_json::to_value(&settings)
                .unwrap()
                .get("project_names")
                .is_none()
        );
        assert!(
            patch_remote_sidebar(&mut settings, &serde_json::json!({"project_names":{}})).is_err()
        );
    }

    #[test]
    fn notification_details_require_password_mode_opt_in_and_known_plain_mode() {
        let mut settings: AppSettings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(settings.store_password_mode);
        assert!(!settings.password_notification_details);
        assert!(!settings.notification_details_enabled());

        settings.store_password_mode = false;
        assert!(settings.notification_details_enabled());
        settings.store_password_mode = true;
        assert!(!settings.notification_details_enabled());
        settings.password_notification_details = true;
        assert!(settings.notification_details_enabled());
    }

    #[test]
    fn notification_mode_is_host_owned_and_opt_in_is_persisted() {
        let root = tempfile::tempdir().unwrap();
        let store =
            maple_agent::store::Store::open(maple_agent::store::StorePaths::new(root.path()))
                .unwrap();
        // Even an old or forged persisted mode bit cannot override the host.
        store
            .set_app_setting("ui", &serde_json::json!({"store_password_mode": true}), 1)
            .unwrap();
        assert!(
            !load_host_settings(&store, None)
                .unwrap()
                .store_password_mode
        );
        apply_host_patch(
            &store,
            None,
            &serde_json::json!({"password_notification_details": true}),
        )
        .unwrap();
        assert!(
            load_host_settings(&store, None)
                .unwrap()
                .password_notification_details
        );
        let saved = store.app_setting("ui").unwrap().unwrap();
        assert!(saved.get("store_password_mode").is_none());
        assert!(
            apply_host_patch(
                &store,
                None,
                &serde_json::json!({"store_password_mode": false}),
            )
            .is_err()
        );
    }

    #[test]
    fn local_preference_edits_cannot_override_verified_notification_mode() {
        let _guard = SETTINGS_IO_LOCK.lock();
        install_test_settings(AppSettings::default());
        edit_cache(|settings| settings.store_password_mode = false);
        let cache = CACHE.lock().unwrap();
        assert!(cache.value.store_password_mode);
        assert!(!cache.pending.contains_key("store_password_mode"));
    }

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
    fn existing_settings_files_default_automatic_update_downloads_to_on() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"theme":"dark","desktop_notifications":false}"#)
                .expect("deserialize settings from before automatic downloads");
        assert!(settings.automatic_update_downloads);
        assert_eq!(settings.theme, "dark");
        assert!(!settings.desktop_notifications);
        assert!(AppSettings::default().automatic_update_downloads);
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

    #[test]
    fn queued_updates_reach_the_host_store_without_sidecar_files() {
        let _guard = SETTINGS_IO_LOCK.lock();
        let root = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            maple_agent::store::Store::open(maple_agent::store::StorePaths::new(root.path()))
                .unwrap(),
        );
        install_test_store(store.clone());
        update_settings_in_background(|settings| settings.tts_voice = "neutral_male".into());
        update_settings_in_background(|settings| settings.tts_speed = 1.5);
        let view = SidebarView {
            group: SidebarGroup::List,
            sort: SidebarSort::Manual,
        };
        update_settings_in_background(move |settings| settings.sidebar_layout = view);
        update_settings_in_background(|settings| settings.automatic_update_downloads = false);
        update_settings_in_background(|settings| {
            settings
                .shortcut_overrides
                .insert("chat.new_task".into(), None);
        });
        update_settings_and_wait(|_| {});
        let reloaded = load_host_settings(&store, None).unwrap();
        assert_eq!(reloaded.tts_voice, "neutral_male");
        assert_eq!(reloaded.sidebar_layout, view);
        assert!((reloaded.tts_speed - 1.5).abs() < 0.01);
        assert!(!reloaded.automatic_update_downloads);
        assert_eq!(
            reloaded.shortcut_overrides.get("chat.new_task"),
            Some(&None)
        );
        update_settings_and_wait(|settings| {
            settings.shortcut_overrides.remove("chat.new_task");
        });
        assert!(
            !load_host_settings(&store, None)
                .unwrap()
                .shortcut_overrides
                .contains_key("chat.new_task")
        );
        assert!(
            !store
                .paths()
                .root()
                .join("staging/config/settings.json")
                .exists()
        );
    }

    #[test]
    fn settings_without_a_host_keep_edits_pending_in_memory() {
        let _guard = SETTINGS_IO_LOCK.lock();
        *CACHE.lock().unwrap() = SettingsCache {
            generation: 0,
            value: AppSettings::default(),
            pending: Default::default(),
            words: Default::default(),
            sidebar_orders: Default::default(),
            pending_sidebar_orders: Default::default(),
            sink: None,
        };
        edit_cache(|settings| settings.theme = "dark".into());
        assert!(synchronize_once().is_err());
        let cache = CACHE.lock().unwrap();
        assert_eq!(cache.value.theme, "dark");
        assert_eq!(cache.pending.get("theme"), Some(&serde_json::json!("dark")));
    }

    #[test]
    fn maintenance_hides_cached_values_but_reset_discards_pending_generation() {
        let _guard = SETTINGS_IO_LOCK.lock();
        install_test_settings(AppSettings::default());
        edit_cache(|settings| settings.harness_instructions = "private pending prompt".into());
        CACHE.lock().unwrap().words.insert("private-word".into());
        clear_decrypted_cache();
        assert!(load_settings().harness_instructions.is_empty());
        assert!(
            CACHE
                .lock()
                .unwrap()
                .pending
                .contains_key("harness_instructions")
        );
        discard_decrypted_cache();
        let cache = CACHE.lock().unwrap();
        assert!(cache.pending.is_empty());
        assert!(cache.words.is_empty());
        assert!(cache.sink.is_none());
    }

    #[test]
    fn nested_shortcut_patches_preserve_disables_removals_and_other_edits() {
        let before = serde_json::json!({"shortcut_overrides":{"a":"primary-a","b":"primary-b"}});
        let first = serde_json::json!({"shortcut_overrides":{"a":null,"b":"primary-b"}});
        let second = serde_json::json!({"shortcut_overrides":{"a":null}});
        let mut pending = value_patch(&before, &first).unwrap();
        compose_patch(&mut pending, &value_patch(&first, &second).unwrap());
        let mut host = serde_json::json!({"shortcut_overrides":{"a":"primary-a","b":"primary-b","c":"primary-c"}});
        merge_patch(&mut host, &pending);
        assert_eq!(
            host,
            serde_json::json!({"shortcut_overrides":{"a":null,"c":"primary-c"}})
        );
    }

    #[test]
    fn settings_snapshot_does_not_include_account_navigation_metadata() {
        let root = tempfile::tempdir().unwrap();
        let store =
            maple_agent::store::Store::open(maple_agent::store::StorePaths::new(root.path()))
                .unwrap();
        let account = store.upsert_account("account", 1).unwrap();
        store
            .set_account_setting(
                account,
                "remote.sidebars",
                &serde_json::json!({"peer":{"display_name":"private"}}),
                1,
            )
            .unwrap();
        let value = load_host_settings(&store, Some(account)).unwrap();
        assert!(value.pinned_tasks.is_empty());
        assert!(value.project_names.is_empty());
        assert!(value.remote_sidebars.is_empty());
    }

    #[test]
    fn remote_pin_patches_preserve_other_windows_additions() {
        let original = RemoteSidebarSettings::default();
        let mut first = original.clone();
        first.pinned_tasks.push("first".into());
        let mut second = original.clone();
        second.pinned_tasks.push("second".into());
        let mut host = original.clone();
        patch_remote_sidebar(&mut host, &remote_sidebar_patch(&original, &first)).unwrap();
        patch_remote_sidebar(&mut host, &remote_sidebar_patch(&original, &second)).unwrap();
        assert_eq!(host.pinned_tasks, vec!["first", "second"]);
    }

    #[test]
    fn disabling_an_inherited_shortcut_is_a_real_patch() {
        let before = serde_json::json!({"shortcut_overrides":{}});
        let after = serde_json::json!({"shortcut_overrides":{"inherited":null}});
        let patch = value_patch(&before, &after).unwrap();
        let mut host = before;
        merge_patch(&mut host, &patch);
        assert_eq!(host, after);
    }
}
