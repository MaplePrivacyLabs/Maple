//! The vocabulary the agent runtime speaks to its host.
//!
//! Everything here is plain data that crosses the boundary between the
//! runtime and whoever drives it: request and response shapes, the timeline
//! item a UI renders, the event enums, the run handle, and the lease that
//! ties an external surface's tool context to that surface's lifetime.
//! Logic lives in the parent module; this file holds the nouns.

use super::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPythonStatus {
    pub resettable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    pub default_project_root: Option<String>,
    #[serde(default = "default_agent_model")]
    pub default_model: String,
    #[serde(default)]
    pub mcp_servers: Vec<AgentMcpServer>,
    #[serde(
        default,
        rename = "projectTrust",
        alias = "projectSkillsTrust",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub project_trust: Vec<AgentProjectTrust>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_project_roots: Vec<String>,
}

pub(super) fn default_agent_model() -> String {
    DEFAULT_AGENT_MODEL.to_string()
}

pub(super) fn selectable_agent_model_id(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    ![
        "whisper",
        "embed",
        "rerank",
        "transcription",
        "text-to-speech",
        "tts",
        "image-generation",
    ]
    .iter()
    .any(|marker| model.contains(marker))
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_project_root: None,
            default_model: default_agent_model(),
            mcp_servers: Vec::new(),
            project_trust: Vec::new(),
            removed_project_roots: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProjectTrust {
    pub path: String,
    pub trusted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProjectTrustFeature {
    Skills,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProjectTrustStatus {
    pub path: String,
    pub decision: Option<bool>,
    pub available: bool,
    pub protected_features: Vec<AgentProjectTrustFeature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMcpKeyValue {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMcpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        environment: Vec<AgentMcpKeyValue>,
    },
    StreamableHttp {
        url: String,
        #[serde(default)]
        environment: Vec<AgentMcpKeyValue>,
        #[serde(default)]
        headers: Vec<AgentMcpKeyValue>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMcpServer {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_mcp_timeout_seconds")]
    pub timeout_seconds: u64,
    pub transport: AgentMcpTransport,
}

/// A Maple-curated integration that can be discovered on this device.
///
/// Integration discovery is intentionally separate from MCP configuration:
/// an integration may be installed without being enabled, and device-local
/// launch details must not leak into the account's roaming configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIntegration {
    pub id: String,
    pub name: String,
    pub description: String,
    pub availability: AgentIntegrationAvailability,
    /// The backend selected for newly-created tasks. This is `None` until the
    /// integration has been set up or enabled at least once.
    pub backend: Option<AgentIntegrationBackend>,
    /// Version of the implementation built into Maple, when one exists.
    pub version: Option<String>,
    /// Version of a separately-installed compatible application, when one was
    /// discovered. Its presence never grants Maple permission or enables it.
    pub standalone_version: Option<String>,
    /// Host-process permissions needed by the built-in implementation.
    pub permissions: Option<AgentIntegrationPermissions>,
    /// Whether a setup action would still do something. It is false once the
    /// only thing left is something Maple cannot perform, such as restarting
    /// the desktop session, so the interface does not offer a button that
    /// repeats work the user already did.
    pub setup_available: bool,
    pub enabled_for_new_tasks: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentIntegrationBackend {
    Embedded,
    External,
}

/// One host-process permission that a built-in integration needs.
///
/// The set of requirements is platform-shaped: macOS needs two TCC grants that
/// can be read before use, while portal-based desktops grant capability per
/// session at first use and therefore require none up front. Callers must not
/// re-derive that per-platform knowledge; ask [`AgentIntegrationPermissions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentIntegrationPermissionKind {
    Accessibility,
    ScreenRecording,
    /// A compositor helper the desktop cannot work without. GNOME advertises
    /// none of the Wayland protocols that expose window geometry or screen
    /// capture to an ordinary client, so both go through a Shell extension.
    DesktopHelper,
}

impl AgentIntegrationPermissionKind {
    /// The name the operating system itself uses for this permission.
    pub fn label(self) -> &'static str {
        match self {
            Self::Accessibility => "Accessibility",
            Self::ScreenRecording => "Screen Recording",
            Self::DesktopHelper => "GNOME helper extension",
        }
    }

    /// What the user has to do while an operating-system settings window is
    /// open, or `None` when the remedy is not an external window.
    ///
    /// A requirement whose state changes as the user works through it, such as
    /// a compositor helper that is installed and then needs a session restart,
    /// deliberately has no answer here. Its remedy is written once, on the
    /// integration itself, so a second copy cannot describe the wrong step.
    pub fn guidance(self) -> Option<&'static str> {
        match self {
            Self::Accessibility | Self::ScreenRecording => Some(
                "Grant Maple this access in System Settings, then fully quit and reopen Maple.",
            ),
            Self::DesktopHelper => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIntegrationPermission {
    pub kind: AgentIntegrationPermissionKind,
    pub granted: bool,
}

/// Every host-process permission a built-in integration needs, with its
/// current grant state.
///
/// An empty requirement list means the platform needs no pre-flight grant, so
/// [`AgentIntegrationPermissions::ready`] is true. That is the single place
/// where "may this integration run" is decided.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIntegrationPermissions {
    pub required: Vec<AgentIntegrationPermission>,
}

impl AgentIntegrationPermissions {
    /// No pre-flight grant is required on this platform.
    pub fn none_required() -> Self {
        Self::default()
    }

    pub fn with(mut self, kind: AgentIntegrationPermissionKind, granted: bool) -> Self {
        self.required
            .push(AgentIntegrationPermission { kind, granted });
        self
    }

    /// Whether every required permission has been granted.
    pub fn ready(&self) -> bool {
        self.required.iter().all(|permission| permission.granted)
    }

    /// The first permission still to be granted, in the order the platform
    /// wants the user to grant them. Both the setup prompt and the settings
    /// pane that Maple opens are derived from this one answer.
    pub fn first_missing(&self) -> Option<AgentIntegrationPermissionKind> {
        self.required
            .iter()
            .find(|permission| !permission.granted)
            .map(|permission| permission.kind)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentIntegrationAvailability {
    NotDetected,
    SetupRequired,
    Available,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSetIntegrationEnabledRequest {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSetupIntegrationRequest {
    pub id: String,
}

/// An MCP server supplied by an external Agent surface for one leased session.
///
/// Unlike [`AgentMcpServer`], this type is never serialized into Maple's user
/// configuration or Goose session metadata. It may contain short-lived bearer
/// headers owned by the calling surface, so the lease that installs it also
/// owns its removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentTransientMcpServer {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) timeout_seconds: u64,
    pub(crate) transport: AgentTransientMcpTransport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentTransientMcpTransport {
    StreamableHttp {
        url: String,
        headers: Vec<AgentMcpKeyValue>,
    },
}

pub(super) fn default_mcp_timeout_seconds() -> u64 {
    DEFAULT_MCP_TIMEOUT_SECONDS
}

/// A skill-derived slash command the composer can offer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSlashCommand {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}

/// One answer choice, mirroring codex's request_user_input option.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQuestionOption {
    pub label: String,
    pub description: String,
}

/// One question in a request_user_input call: one to three related
/// questions ride a single call and are answered together. The client adds
/// a free-form "Other" answer next to these options.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<AgentQuestionOption>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMcpConnectionError {
    pub name: String,
    pub error: String,
}

pub(super) const TTS_MODEL: &str = "voxtral-tts";
pub(super) const TRANSCRIPTION_MODEL: &str = "whisper-large-v3";

/// Voice endpoints the signed-in account can use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioCapabilities {
    pub transcription: bool,
    pub speech: bool,
}

/// A user-facing message for a failed audio response, or `None` on success.
pub(super) fn audio_error_message(response: &crate::maple_api::AudioResponse) -> Option<String> {
    if (200..300).contains(&response.status) {
        return None;
    }
    if matches!(response.status, 402 | 403) {
        return Some("Voice features need a Pro, Max, or Team plan".to_string());
    }
    Some(match audio_error_detail(&response.body) {
        Some(detail) => format!("Voice request failed: {detail}"),
        None => format!("Voice request failed with HTTP {}", response.status),
    })
}

/// The WAV bytes of a 2xx text-to-speech body. The SDK reports the
/// encrypted envelope's `application/json` content type even after it
/// decrypts the body, so the bytes decide: a WAV header is audio, JSON is
/// a provider error (or a JSON-wrapped base64 clip).
pub(super) fn speech_audio_from_body(body: Vec<u8>) -> Result<Vec<u8>, String> {
    if body.is_empty() {
        return Err("Text-to-speech returned an empty audio file".to_string());
    }
    if body.starts_with(b"RIFF") {
        return Ok(body);
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        // Not WAV and not JSON: some other audio container. Let the
        // decoder decide.
        return Ok(body);
    };
    if let Some(detail) = audio_error_detail(&body) {
        return Err(format!(
            "Text-to-speech provider returned an error: {detail}"
        ));
    }
    if let Some(audio) = find_base64_audio(&value) {
        return Ok(audio);
    }
    let keys = match &value {
        serde_json::Value::Object(map) => map.keys().cloned().collect::<Vec<_>>().join(", "),
        other => format!("{} value", json_type_name(other)),
    };
    log::warn!("text-to-speech JSON body carried no audio; top-level: {keys}");
    Err("Text-to-speech provider returned an error response".to_string())
}

pub(super) fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// The first string anywhere in `value` that base64-decodes to audio.
/// Strings shorter than a WAV header cannot be a clip.
pub(super) fn find_base64_audio(value: &serde_json::Value) -> Option<Vec<u8>> {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};

    match value {
        serde_json::Value::String(text) if text.len() >= 64 => {
            let text = text.trim();
            [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
                .iter()
                .find_map(|engine| engine.decode(text).ok())
                .filter(|decoded| looks_like_audio(decoded))
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_base64_audio),
        serde_json::Value::Object(map) => map.values().find_map(find_base64_audio),
        _ => None,
    }
}

/// WAV, or another container `rodio` may decode; anything but obvious
/// text.
pub(super) fn looks_like_audio(bytes: &[u8]) -> bool {
    bytes.len() >= 64
        && (bytes.starts_with(b"RIFF")
            || bytes.starts_with(b"fLaC")
            || bytes.starts_with(b"OggS")
            || bytes.starts_with(b"ID3")
            || bytes.starts_with(&[0xFF, 0xFB])
            || bytes.starts_with(&[0xFF, 0xF3])
            || (bytes.len() > 12 && &bytes[4..8] == b"ftyp"))
}

/// The `message`, `detail`, or `error` text of a JSON error body.
pub(super) fn audio_error_detail(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    ["message", "detail", "error"]
        .iter()
        .find_map(|key| {
            let field = value.get(key)?;
            field
                .as_str()
                .map(str::to_string)
                .or_else(|| field.get("message")?.as_str().map(str::to_string))
        })
        .map(|detail| detail.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|detail| !detail.is_empty())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionMcpServer {
    pub name: String,
    pub description: String,
    pub transport: String,
    pub enabled: bool,
    pub available: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSetSessionMcpServerRequest {
    pub session_id: String,
    pub name: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStartRequest {
    pub project_root: Option<String>,
    pub model: Option<String>,
    pub mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeStatus {
    pub running: bool,
    pub project_root: Option<String>,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub active_runs: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentProjectRoot {
    pub path: String,
    pub name: String,
    pub last_used_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProjectRootRegistration {
    pub project_root: String,
    pub roots: Vec<RecentProjectRoot>,
    pub config: AgentConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCreateSessionRequest {
    pub project_root: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub context_limit: Option<usize>,
    pub mode: Option<String>,
    pub mcp_server_names: Option<Vec<String>>,
    /// Caller-owned system prompt, appended to Maple's own. Surfaces such
    /// as ACP pass the persona text their client supplies with the task.
    #[serde(default)]
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSendMessageRequest {
    pub session_id: String,
    pub text: String,
    pub model: Option<String>,
    #[serde(default)]
    pub context_limit: Option<usize>,
    pub mode: Option<String>,
    #[serde(default)]
    pub vision_capable: bool,
    #[serde(default)]
    pub steer: bool,
    #[serde(default)]
    pub queue_id: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AgentImageUpload>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRenameSessionRequest {
    pub session_id: String,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPermissionResponse {
    pub session_id: String,
    pub request_id: String,
    pub decision: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentPermissionRequest {
    pub request_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Map<String, Value>,
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPermissionDecision {
    AllowOnce,
    DenyOnce,
    Cancel,
}

impl AgentPermissionDecision {
    pub(super) fn status(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::DenyOnce => "deny_once",
            Self::Cancel => "cancelled",
        }
    }

    pub(super) fn goose_permission(self) -> Permission {
        match self {
            Self::AllowOnce => Permission::AllowOnce,
            Self::DenyOnce => Permission::DenyOnce,
            Self::Cancel => Permission::Cancel,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPermissionRouting {
    Desktop,
    CallingSurface,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPermissionModeRequest {
    pub session_id: String,
    pub mode: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSetSessionWebRequest {
    pub session_id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQueuedMessage {
    pub queue_id: String,
    pub message_id: String,
    pub session_id: String,
    pub text: String,
    pub attachments: Vec<AgentImageAttachment>,
    pub created_ms: u128,
    #[serde(skip)]
    pub(super) message: Message,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDesktopQueueSnapshot {
    pub revision: u64,
    pub items: Vec<AgentQueuedMessage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQueueControlRequest {
    pub session_id: String,
    pub queue_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRunTerminal {
    Completed,
    Cancelled,
    Failed,
}

pub struct AgentRunHandle {
    pub run_id: String,
    pub events: mpsc::Receiver<AgentRunEvent>,
    pub terminal: watch::Receiver<Option<AgentRunTerminal>>,
    pub usage: watch::Receiver<Option<AgentRunUsage>>,
    pub event_overflowed: Arc<AtomicBool>,
    pub(crate) permission_responder: Option<AgentRunPermissionResponder>,
    pub(crate) cancellation: Option<AgentRunCancellation>,
    pub queued: Option<AgentQueuedMessage>,
    pub queue: AgentDesktopQueueSnapshot,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentRunUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cached_read_tokens: u64,
    pub(crate) cached_write_tokens: u64,
}

pub(super) type AgentRunSetup = (
    Arc<Agent>,
    Vec<AgentMcpConnectionError>,
    SharedAgentToolContext,
    bool,
    AgentRunUsage,
);

impl AgentRunUsage {
    pub(super) fn from_accumulated_session(session: &Session) -> Self {
        Self {
            input_tokens: nonnegative_tokens(session.accumulated_usage.input_tokens),
            output_tokens: nonnegative_tokens(session.accumulated_usage.output_tokens),
            total_tokens: nonnegative_tokens(session.accumulated_usage.total_tokens),
            cached_read_tokens: nonnegative_tokens(
                session.accumulated_usage.cache_read_input_tokens,
            ),
            cached_write_tokens: nonnegative_tokens(
                session.accumulated_usage.cache_write_input_tokens,
            ),
        }
    }

    pub(super) fn saturating_delta(self, before: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(before.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(before.output_tokens),
            total_tokens: self.total_tokens.saturating_sub(before.total_tokens),
            cached_read_tokens: self
                .cached_read_tokens
                .saturating_sub(before.cached_read_tokens),
            cached_write_tokens: self
                .cached_write_tokens
                .saturating_sub(before.cached_write_tokens),
        }
    }
}

pub(super) fn nonnegative_tokens(tokens: Option<i32>) -> u64 {
    tokens
        .and_then(|tokens| u64::try_from(tokens).ok())
        .unwrap_or(0)
}

#[derive(Clone)]
pub(crate) struct AgentRunPermissionResponder {
    pub(super) agent: AgentRuntimeHandle,
    pub(super) session_id: Arc<str>,
    pub(super) run_id: Arc<str>,
}

impl AgentRunPermissionResponder {
    pub async fn respond(
        &self,
        request_id: String,
        decision: AgentPermissionDecision,
    ) -> Result<(), String> {
        self.agent
            .permission_respond_for_run(
                self.session_id.as_ref(),
                self.run_id.as_ref(),
                request_id,
                decision,
            )
            .await
    }
}

/// Opaque cancellation capability for one run owned by a calling surface.
///
/// Unlike the Desktop command boundary, an adapter already has the exact run
/// identity. Retaining that identity here prevents it from cancelling another
/// surface's run through a caller-provided run ID.
#[derive(Clone)]
pub(crate) struct AgentRunCancellation {
    pub(super) agent: AgentRuntimeHandle,
    pub(super) session_id: Arc<str>,
    pub(super) run_id: Arc<str>,
    pub(super) routing: AgentPermissionRouting,
}

impl AgentRunCancellation {
    pub async fn cancel(&self) -> Result<(), String> {
        self.agent
            .cancel_run_scoped(
                self.run_id.as_ref(),
                Some(self.session_id.as_ref()),
                self.routing,
            )
            .await
    }
}

pub(crate) struct CreatedAgentSession {
    pub(crate) detail: AgentSessionDetail,
    pub(crate) tool_context_lease: Option<AgentToolContextLease>,
}

/// Controls whether a surface's events are also projected into Maple Desktop.
///
/// This is deliberately independent of tool-context ownership. A calling
/// surface can keep its transient run stream isolated while persisted history
/// remains available when Maple Desktop later loads the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentHostEventPolicy {
    Publish,
    Suppress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DesktopSendDisposition {
    /// Desktop send: stage onto the live run when one exists, otherwise start.
    StageOrStart,
    /// ACP and other exclusive surfaces must not join another run.
    StartOnly,
}

impl AgentHostEventPolicy {
    pub(super) fn publishes(self) -> bool {
        matches!(self, Self::Publish)
    }
}

pub(crate) struct AgentToolContextLease {
    pub(super) service: MapleAgentService,
    pub(super) access: AgentToolContextAccess,
    pub(super) created_cleanup: Option<CreatedAgentSessionCleanup>,
    pub(super) discard_created_on_drop: bool,
    pub(super) cleanup_started: bool,
}

#[derive(Clone)]
pub(super) struct CreatedAgentSessionCleanup {
    pub(super) agent_manager: Arc<AgentManager>,
    pub(super) session_manager: Arc<SessionManager>,
    pub(super) expected: Session,
}

#[derive(Clone)]
pub(crate) struct AgentToolContextAccess {
    pub(super) account_scope: Arc<str>,
    pub(super) session_id: Arc<str>,
    pub(super) installation_id: u64,
    pub(super) context: SharedAgentToolContext,
}

impl AgentToolContextLease {
    pub fn access(&self) -> AgentToolContextAccess {
        self.access.clone()
    }

    pub fn revoke(&self) {
        self.access.context.revoke();
    }

    pub async fn release(mut self) {
        // Stop credential-bearing calls synchronously before waiting for the
        // session lifecycle fence and cached-Agent unload.
        self.access.context.revoke();
        release_tool_context_lease(self.service.clone(), self.access.clone()).await;
        // Mark completion only after the awaited cleanup. If this future is
        // cancelled while waiting for the lifecycle fence, Drop schedules an
        // exact-match retry instead of stranding a revoked leased entry.
        self.cleanup_started = true;
    }

    pub async fn discard_created_if_untouched(mut self) {
        self.discard_created_on_drop = true;
        self.access.context.revoke();
        if let Some(cleanup) = self.created_cleanup.as_ref() {
            cleanup_provisional_created_session(
                self.service.clone(),
                self.access.clone(),
                Arc::clone(&cleanup.agent_manager),
                Arc::clone(&cleanup.session_manager),
                cleanup.expected.clone(),
            )
            .await;
        } else {
            release_tool_context_lease(self.service.clone(), self.access.clone()).await;
        }
        self.cleanup_started = true;
    }
}

pub(super) async fn release_tool_context_lease(
    service: MapleAgentService,
    access: AgentToolContextAccess,
) {
    // A revoked leased entry remains authoritative until this lifecycle
    // section removes it. Desktop callers must never garbage-collect it and
    // reuse the still-cached Agent while transient caller state is attached.
    let _session_lifecycle = service.session_lifecycle.lock().await;
    let (removed, agent_manager) = {
        let mut runtime = service.inner.lock().await;
        let Some(current) = runtime.as_mut() else {
            return;
        };
        if current.account_scope != access.account_scope.as_ref() {
            return;
        }
        let removed = take_matching_tool_context(
            &mut current.session_tool_contexts,
            access.session_id.as_ref(),
            access.installation_id,
            &access.context,
        );
        (removed, Arc::clone(&current.agent_manager))
    };
    if let Some(installed) = removed {
        installed.revoke("Python task ownership ended");
        // Dropping the cached Agent is the fail-closed way to remove every
        // transient MCP client (and any secret-bearing HTTP headers) without
        // mutating the persisted extension set. A later Desktop or ACP use
        // reconstructs the Agent from durable, non-transient metadata.
        if let Err(error) = agent_manager
            .remove_session_if_loaded(access.session_id.as_ref())
            .await
        {
            log::warn!(
                "Failed to unload Agent task {} after external lease release: {error}",
                access.session_id
            );
        }
    }
}

pub(super) async fn cleanup_provisional_created_session(
    service: MapleAgentService,
    access: AgentToolContextAccess,
    agent_manager: Arc<AgentManager>,
    session_manager: Arc<SessionManager>,
    expected: Session,
) {
    // Keep the task fenced until both the secret-bearing cached Agent and the
    // untouched provisional row are gone. If the exact reservation no longer
    // belongs to us, fail closed and leave the durable task alone.
    let _session_lifecycle = service.session_lifecycle.lock().await;
    let permission_modes = {
        let mut runtime = service.inner.lock().await;
        match runtime.as_mut() {
            Some(current) if current.account_scope == access.account_scope.as_ref() => {
                if has_active_session_run(&current.active_runs, access.session_id.as_ref()) {
                    return;
                }
                if let Some(installed) = current
                    .session_tool_contexts
                    .get(access.session_id.as_ref())
                {
                    let exact = installed.installation_id == access.installation_id
                        && installed.context.ptr_eq(&access.context);
                    if !exact {
                        // A replacement owner won the task. Never unload or
                        // delete underneath it.
                        return;
                    }
                }
                if let Some(installed) = take_matching_tool_context(
                    &mut current.session_tool_contexts,
                    access.session_id.as_ref(),
                    access.installation_id,
                    &access.context,
                ) {
                    installed.revoke("Python task ownership ended");
                }
                Some(Arc::clone(&current.permission_modes))
            }
            // Runtime stop/replacement drains the old registry. The captured
            // account-scoped managers still let us remove only the untouched
            // row that this setup created.
            _ => None,
        }
    };
    access.context.revoke();
    if let Err(error) = agent_manager
        .remove_session_if_loaded(access.session_id.as_ref())
        .await
    {
        log::warn!(
            "Failed to unload provisional Agent task {} after setup error: {error}",
            access.session_id
        );
    }
    if let Some(permission_modes) = permission_modes {
        permission_modes
            .lock()
            .await
            .remove(access.session_id.as_ref());
    }

    let current = match session_manager
        .get_session(access.session_id.as_ref(), true)
        .await
    {
        Ok(session) => session,
        Err(_) => return,
    };
    let conversation_is_empty = current
        .conversation
        .as_ref()
        .is_none_or(|conversation| conversation.messages().is_empty());
    let untouched = current.id == expected.id
        && current.created_at == expected.created_at
        && current.working_dir == expected.working_dir
        && current.session_type == expected.session_type
        && current.name == expected.name
        && !current.user_set_name
        && current.message_count == 0
        && conversation_is_empty
        && current.archived_at == expected.archived_at;
    if !untouched {
        log::warn!(
            "Preserving provisional Agent task {} after setup error because it changed while setup was pending",
            access.session_id
        );
        return;
    }
    if let Err(error) = session_manager
        .delete_session(access.session_id.as_ref())
        .await
    {
        log::warn!(
            "Failed to remove provisional Agent task {} after setup error: {error}",
            access.session_id
        );
    }
}

impl Drop for AgentToolContextLease {
    fn drop(&mut self) {
        self.access.context.revoke();
        if self.cleanup_started {
            return;
        }
        let service = self.service.clone();
        let access = self.access.clone();
        let created_cleanup = self.created_cleanup.clone();
        let discard_created = self.discard_created_on_drop;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if discard_created {
                    if let Some(cleanup) = created_cleanup {
                        cleanup_provisional_created_session(
                            service,
                            access,
                            cleanup.agent_manager,
                            cleanup.session_manager,
                            cleanup.expected,
                        )
                        .await;
                    } else {
                        release_tool_context_lease(service, access).await;
                    }
                } else {
                    release_tool_context_lease(service, access).await;
                }
            });
        }
    }
}

#[derive(Debug, Clone)]
pub enum AgentRunEvent {
    SessionUpdated(AgentSessionSummary),
    Started,
    TimelineItem(AgentTimelineItem),
    PermissionRequested {
        request: AgentPermissionRequest,
        item: AgentTimelineItem,
    },
    SetupWarning(String),
    /// A `delegate` call handed a task to a subagent. `id` is the request
    /// ID of that call, which the two events below repeat.
    SubagentStarted {
        id: String,
        task: String,
        /// The subagent runs in the background; the task collects its
        /// result later with `load`.
        background: bool,
    },
    /// The subagent called a tool. Only the latest one is shown.
    SubagentActivity {
        id: String,
        tool: String,
    },
    SubagentFinished {
        id: String,
    },
    HistoryReplaced,
    Error(AgentTimelineItem),
    Finished(AgentRunTerminal),
    QueueChanged(AgentDesktopQueueSnapshot),
    QueuePromoted {
        snapshot: AgentDesktopQueueSnapshot,
        queue_id: String,
        item: AgentTimelineItem,
    },
}

#[derive(Debug, Clone)]
pub enum AgentServiceEvent {
    RuntimeStatus(AgentRuntimeStatus),
    /// The agent asked the user one or more related questions (ask_user
    /// tool); every question in the batch is answered in one card.
    Question {
        session_id: String,
        request_id: String,
        questions: Vec<AgentQuestion>,
    },
    SessionCreated(AgentSessionSummary),
    SessionUpdated {
        session_id: String,
        run_id: Option<String>,
        session: AgentSessionSummary,
    },
    TimelineItem {
        session_id: String,
        run_id: Option<String>,
        item: AgentTimelineItem,
    },
    Run {
        session_id: String,
        run_id: String,
        event: AgentRunEvent,
    },
    /// Streamed answer to a `/btw` side question. The question and the
    /// answer are never stored in the session.
    SideQuestion {
        session_id: String,
        request_id: String,
        event: SideQuestionEvent,
    },
}

/// One subagent that is still working for a task. A caller that opens
/// the task after the run ended reads these to rebuild its live view.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSubagent {
    /// Request ID of the `delegate` call that started it.
    pub id: String,
    pub task: String,
    /// It works in the background; the task collects the result later.
    pub background: bool,
    /// How long it has worked, which survives a caller restart better
    /// than a start time from another clock.
    pub elapsed_ms: u64,
    /// The tool it called most recently.
    pub activity: Option<String>,
}

/// One finished exchange of a `/btw` thread, replayed on a follow-up so
/// the model sees the earlier side questions and answers.
#[derive(Debug, Clone)]
pub struct SideQuestionTurn {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Clone)]
pub enum SideQuestionEvent {
    Chunk(String),
    Finished,
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionSummary {
    pub id: String,
    pub title: String,
    pub project_root: String,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub message_count: usize,
    pub model: Option<String>,
    pub mode: String,
    /// Whether the task can use `web_search` / `open_url`.
    pub web_enabled: bool,
    /// Hidden from the main task list; can be restored.
    pub archived: bool,
    /// Created by an ACP client (an editor or Buzz), not in the desktop app.
    pub acp: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionDetail {
    pub session: AgentSessionSummary,
    pub timeline: Vec<AgentTimelineItem>,
    pub mcp_errors: Vec<AgentMcpConnectionError>,
    pub queue: AgentDesktopQueueSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTimelineItem {
    pub id: String,
    pub item_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    pub created_ms: u128,
    pub merge: String,
}

#[cfg(test)]
mod speech_body_tests {
    use super::speech_audio_from_body;

    #[test]
    fn wav_bytes_pass_through() {
        let wav = b"RIFF\x00\x00\x00\x00WAVE".to_vec();
        assert_eq!(speech_audio_from_body(wav.clone()).unwrap(), wav);
    }

    #[test]
    fn json_error_is_reported() {
        let body = br#"{"error":{"message":"voice not found"}}"#.to_vec();
        assert_eq!(
            speech_audio_from_body(body).unwrap_err(),
            "Text-to-speech provider returned an error: voice not found"
        );
    }

    #[test]
    fn json_wrapped_base64_is_decoded_wherever_it_sits() {
        use base64::Engine;
        let mut wav = b"RIFF\x00\x00\x00\x00WAVE".to_vec();
        wav.resize(128, 0);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&wav);
        for body in [
            format!(r#"{{"audio":"{encoded}"}}"#),
            format!(r#"{{"result":{{"clip":{{"b64":"{encoded}"}}}},"id":"x"}}"#),
            format!(r#"[{{"data":"{encoded}"}}]"#),
        ] {
            assert_eq!(speech_audio_from_body(body.into_bytes()).unwrap(), wav);
        }
    }

    #[test]
    fn json_without_audio_is_an_error() {
        let body = br#"{"status":"ok","note":"short"}"#.to_vec();
        assert!(speech_audio_from_body(body).is_err());
    }

    #[test]
    fn empty_body_is_an_error() {
        assert!(speech_audio_from_body(Vec::new()).is_err());
    }
}
