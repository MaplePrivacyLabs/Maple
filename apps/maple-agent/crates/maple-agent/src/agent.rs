// The ACP host (`crate::acp`) drives this runtime through `pub(crate)`
// hooks: surface sessions, tool-context leases, run-scoped permission and
// cancellation handles. Without the `acp` feature nobody calls them, and
// that is expected.
#![cfg_attr(not(feature = "acp"), allow(dead_code))]
mod attachments;
#[cfg(target_os = "macos")]
mod bounded_process;
mod code_mode;
#[cfg(embedded_cua)]
mod cua;
mod developer_tools;
#[cfg(target_os = "linux")]
mod gnome_helper;
// The computer-use half of this module is reachable only from `cua`, which
// exists only where Maple can host the CUA runtime.
#[cfg_attr(not(embedded_cua), allow(dead_code))]
mod image_mediation;
mod integrations;
#[cfg(target_os = "macos")]
mod macos_login_path;
mod mcp;
pub(crate) mod provider;
mod questions;
mod shell_permission;
mod system_prompt;
mod timeline;
mod tool_context;
mod transient_mcp;
mod types;
mod web_permission;
mod web_tools;

use crate::maple_api::{MapleApiSession, account_scope};
pub use attachments::AgentImageUpload;
use attachments::{AgentAttachmentStore, AgentImageAttachment, PreparedAgentImage};
use code_mode::PythonTaskBinding;
#[cfg(test)]
use developer_tools::EXTERNAL_MCP_TOOL_NAME;
use developer_tools::MapleDeveloperClient;
use futures_util::StreamExt;
use goose::agents::SUBAGENT_TOOL_REQUEST_TYPE;
use goose::agents::extension::Envs;
use goose::agents::mcp_client::McpClientTrait;
use goose::agents::platform_extensions::summon::EXTENSION_NAME as SUMMON_EXTENSION_NAME;
use goose::agents::{
    Agent, AgentConfig as GooseAgentConfig, AgentEvent, ExtensionConfig, GoosePlatform,
    SessionConfig, ToolCallContext,
};
use goose::config::{
    ConfigError, DEFAULT_EXTENSION_DESCRIPTION, DEFAULT_EXTENSION_TIMEOUT, GooseMode,
    PermissionManager,
};
use goose::conversation::message::{
    ActionRequiredData, ErrorContent, Message, MessageContent, MessageErrorKind,
    SystemNotificationContent, SystemNotificationType,
};
use goose::conversation::{Conversation, fix_conversation};
use goose::execution::manager::{AgentManager, AgentManagerGetResult, RuntimeContext};
use goose::permission::permission_confirmation::PrincipalType;
use goose::permission::{Permission, PermissionConfirmation};
use goose::session::session_manager::{Session, SessionType};
use goose::session::{ExtensionState, SessionManager};
use goose::skills::{EXTENSION_NAME as SKILLS_EXTENSION_NAME, SkillsClient};
use icu_properties::{CodePointSetData, props::DefaultIgnorableCodePoint};
use integrations::*;
use mcp::*;
use provider::{MAPLE_PROVIDER_NAME, MapleProvider};
use rmcp::model::{
    CallToolResult, ContentBlock, InitializeResult, JsonObject, ListToolsResult, ServerNotification,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shell_permission::{
    ShellPermissionClassifier, ShellPermissionOutcome, ShellPermissionRequest,
    local_read_image_request_id, local_read_request_id,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};
pub use timeline::*;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
#[cfg(feature = "acp")]
pub(crate) use tool_context::SENSITIVE_BRIDGE_ENV;
use tool_context::SharedAgentToolContext;
pub use tool_context::{AgentToolContextSpec, default_tool_context_spec};
use transient_mcp::{TransientMcpConfig, TransientMcpRouter};
pub use types::*;
use web_permission::{
    OpenUrlPermissionRequest, WebPermissionClassifier, WebPermissionContext, WebPermissionOutcome,
};
use web_tools::WebToolState;

const DEFAULT_AGENT_MODEL: &str = "glm-5-2";
const LEGACY_AGENT_DEFAULT_MODEL: &str = "auto:powerful";
const DEFAULT_GOOSE_MODE: &str = "smart_approve";
// Keep Goose on its ActionRequired path so Maple can apply the currently selected
// policy at every tool boundary, including when the user changes it mid-run.
const GOOSE_PERMISSION_ROUTING_MODE: GooseMode = GooseMode::SmartApprove;

/// Start the explicit, host-owned setup flow for a curated integration.
///
/// Desktop callers must invoke this directly from the user's UI action rather
/// than a backend worker so macOS can attribute and present its privacy UI in
/// the host application context. Persisting the selected backend remains a
/// separate asynchronous operation after the OS reports both grants.
pub fn begin_integration_setup(
    request: &AgentSetupIntegrationRequest,
) -> Result<AgentIntegrationPermissions, String> {
    require_known_integration(&request.id)?;
    #[cfg(embedded_cua)]
    {
        Ok(cua::request_embedded_cua_permissions())
    }
    #[cfg(not(embedded_cua))]
    {
        Err("Built-in CUA setup is not available on this operating system yet".to_string())
    }
}
#[cfg(test)]
const MAPLE_DEVELOPER_TOOLS: [&str; 11] = [
    "read",
    "shell",
    "python_code",
    "edit",
    "write",
    "read_image",
    "todo_write",
    "request_user_input",
    "web_search",
    "open_url",
    EXTERNAL_MCP_TOOL_NAME,
];
const MAPLE_SKILLS_TOOLS: [&str; 1] = ["load_skill"];
/// Goose's `summon` platform extension. Its tools are unprefixed, like
/// Maple's own, so the model sees `delegate` and `load`.
const MAPLE_SUBAGENT_TOOLS: [&str; 2] = [SUBAGENT_DELEGATE_TOOL, SUBAGENT_LOAD_TOOL];
/// Runs one task in a subagent with its own context.
const SUBAGENT_DELEGATE_TOOL: &str = "delegate";
/// Loads a recipe, an agent file, or the result of a background subagent.
const SUBAGENT_LOAD_TOOL: &str = "load";
/// Maple-owned record in the session's extension data: whether the web
/// tools (`web_search`, `open_url`) are offered to the model for this task.
/// Absent means enabled; Maple manages its built-in tools itself rather
/// than through Goose's per-session extension state.
const MAPLE_WEB_STATE_KEY: &str = "maple_web";
const MAPLE_WEB_STATE_VERSION: &str = "1";
// Goose currently renders the runtime registration key as the model-facing
// extension heading, so keep this concise and reserve it from user MCP names.
const MAPLE_SKILLS_CLIENT_KEY: &str = "maple-skills-extension";
const MAPLE_GOOSE_PERMISSION_CONFIG: &str = r#"user:
  always_allow:
  - load_skill
  - todo_write
  - request_user_input
  - load
  ask_before:
  - delegate
  - read
  - shell
  - python_code
  - edit
  - write
  - read_image
  - web_search
  - open_url
  - external_mcp
  never_allow: []
"#;
const RUN_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(not(test))]
const SESSION_TITLE_GENERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
const SESSION_TITLE_GENERATION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(1_500);
const SESSION_TITLE_MODEL: &str = "llama3-3-70b";
const SESSION_TITLE_TEMPERATURE: f32 = 0.7;
const SESSION_TITLE_MAX_TOKENS: i32 = 15;
const SESSION_TITLE_MAX_INPUT_CHARS: usize = 500;
/// Tool-call summaries ride the same cheap model as session titles.
const TOOL_SUMMARY_MODEL: &str = "llama3-3-70b";
const TOOL_SUMMARY_TEMPERATURE: f32 = 0.2;
const TOOL_SUMMARY_MAX_TOKENS: i32 = 48;
const TOOL_SUMMARY_MAX_INPUT_CHARS: usize = 4000;
const TOOL_SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const SIDE_QUESTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Framing for a `/btw` question. It lives in the user message, not the
/// system prompt, so the request keeps the session's cached prefix.
const SIDE_QUESTION_PREFIX: &str = "The user asks a quick side question about the task so far. Answer it directly and briefly in plain prose. Do not call tools and do not continue the task; the task carries on separately and this exchange is not part of it.";
const TOOL_SUMMARY_SYSTEM_PROMPT: &str = "You summarize one tool call for a coding agent's activity feed. Reply with ONE short line of at most 12 words that says what the call did. No prefix, no quotes, no explanations.";
const THINKING_SUMMARY_SYSTEM_PROMPT: &str = "You summarize a coding agent's reasoning for its activity feed. Reply with ONE short line of at most 12 words that says what the agent thought about or decided. No prefix, no quotes, no explanations.";
const SESSION_TITLE_SYSTEM_PROMPT: &str = "You are a helpful assistant that generates concise, meaningful titles (3-5 words) for chat conversations based on the user's first message. Return only the title without quotes or explanations.";
const DEFAULT_AGENT_SESSION_TITLE: &str = "New task";
/// Title an ACP task is created under when the caller supplies none.
const ACP_SESSION_FALLBACK_TITLE: &str = "Maple ACP";
const DEFAULT_MCP_TIMEOUT_SECONDS: u64 = 300;
/// Reported when Stop wins the race against a task's MCP server startup.
const MCP_STARTUP_CANCELLED_ERROR: &str = "Agent run was stopped while starting MCP servers";
const MAX_AGENT_SESSION_TITLE_CHARS: usize = 80;
const MAX_AGENT_ERROR_CHARS: usize = 1_200;
const MAX_MCP_CONNECTION_ERRORS: usize = 3;
const MAX_MCP_SERVER_NAME_CHARS: usize = 64;
const MAX_MCP_CONNECTION_ERROR_CHARS: usize = 200;
const MCP_CONNECTION_ERROR_PREFIX: &str = "Some MCP servers could not connect:";
const AGENT_RUN_EVENT_CAPACITY: usize = 256;
const AGENT_SERVICE_OPEN: u8 = 0;
const AGENT_SERVICE_DRAINING: u8 = 1;
const AGENT_SERVICE_DRAINING_ERROR: &str =
    "Maple Agent services are draining and cannot accept new work";
pub(crate) const AGENT_TOOL_CONTEXT_INACTIVE_ERROR: &str =
    "Agent tool context access is no longer active";
static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_QUEUE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TOOL_CONTEXT_INSTALLATION_ID: AtomicU64 = AtomicU64::new(1);

/// Caller-supplied system prompt per task id. A surface such as ACP sets it
/// when it creates the task; the Goose agent for that task gets it as a
/// system prompt extension the moment the agent is built. In memory only:
/// the caller owns the prompt and resends it on a new session.
static SESSION_SYSTEM_PROMPTS: once_cell::sync::Lazy<std::sync::Mutex<HashMap<String, String>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

fn store_session_system_prompt(session_id: &str, prompt: Option<String>) {
    let mut prompts = SESSION_SYSTEM_PROMPTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match prompt.filter(|prompt| !prompt.trim().is_empty()) {
        Some(prompt) => {
            prompts.insert(session_id.to_string(), prompt);
        }
        None => {
            prompts.remove(session_id);
        }
    }
}

fn session_system_prompt(session_id: &str) -> Option<String> {
    SESSION_SYSTEM_PROMPTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)
        .cloned()
}
const MAX_DESKTOP_QUEUE_ITEMS: usize = 16;
const QUEUED_MESSAGE_ATTACHMENTS_ERROR: &str =
    "New images cannot be added while sending a queued message";
const MAX_DESKTOP_QUEUE_TEXT_BYTES: usize = 32 * 1024;
const MAPLE_IMAGE_ATTACHMENTS_OPERATION: &str = "mapleImageAttachments";

/// Maple's context-limit rule: both fields present and equal is the value,
/// exactly one present wins, and absent or disagreeing metadata is unknown.
pub(crate) fn reconcile_context_limit(
    context_window: Option<u64>,
    max_context_tokens: Option<u64>,
) -> Option<i64> {
    let context_window = context_window
        .and_then(|value| i64::try_from(value).ok())
        .filter(|value| *value > 0);
    let max_context_tokens = max_context_tokens
        .and_then(|value| i64::try_from(value).ok())
        .filter(|value| *value > 0);
    match (context_window, max_context_tokens) {
        (None, None) => None,
        (Some(value), None) | (None, Some(value)) => Some(value),
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(_), Some(_)) => None,
    }
}

fn validate_session_model_lock(
    message_count: usize,
    persisted_model: Option<&str>,
    requested_model: &str,
) -> Result<(), String> {
    if message_count == 0 {
        return Ok(());
    }
    let Some(persisted_model) = persisted_model else {
        return Ok(());
    };
    if persisted_model == requested_model {
        return Ok(());
    }
    Err(format!(
        "This task is locked to model {persisted_model}. Start a new task to use {requested_model}."
    ))
}

struct ActiveAgentRun {
    agent: Arc<Agent>,
    permission_routing: AgentPermissionRouting,
    token: CancellationToken,
    tool_context: SharedAgentToolContext,
    session_id: String,
    events: AgentRunEventPublisher,
    cancelled_permission_ids: CancelledPermissionIds,
    accepting_queue: Arc<AtomicBool>,
    steered_unacked: Arc<Mutex<Vec<Message>>>,
    task_handle: tokio::task::JoinHandle<()>,
}

struct DesktopSessionQueue {
    revision: u64,
    items: VecDeque<AgentQueuedMessage>,
    editing_queue_id: Option<String>,
}

impl DesktopSessionQueue {
    fn snapshot(&self) -> AgentDesktopQueueSnapshot {
        AgentDesktopQueueSnapshot {
            revision: self.revision,
            items: self.items.iter().cloned().collect(),
        }
    }
}

fn empty_desktop_queue_snapshot() -> AgentDesktopQueueSnapshot {
    AgentDesktopQueueSnapshot {
        revision: 0,
        items: Vec::new(),
    }
}

struct ActiveAgentSessionTitleTask {
    run_id: String,
    token: CancellationToken,
    task_handle: tokio::task::JoinHandle<()>,
}

type PendingPermissionKey = (String, String);
#[derive(Debug, Clone, PartialEq)]
struct PendingAgentPermission {
    run_id: String,
    routing: AgentPermissionRouting,
    request: AgentPermissionRequest,
}
type PendingPermissions = Arc<Mutex<HashMap<PendingPermissionKey, PendingAgentPermission>>>;
type IssuedPermissionIds = Arc<Mutex<HashSet<String>>>;

enum AgentPermissionResponseScope {
    Desktop,
    CallingSurface { run_id: String },
}
type CancelledPermissionIds = Arc<Mutex<HashSet<String>>>;
type SessionPermissionModes = Arc<Mutex<HashMap<String, GooseMode>>>;

struct AgentRuntime {
    agent_manager: Arc<AgentManager>,
    session_manager: Arc<SessionManager>,
    maple_api_session: Arc<MapleApiSession>,
    active_runs: HashMap<String, ActiveAgentRun>,
    session_title_tasks: HashMap<String, ActiveAgentSessionTitleTask>,
    session_tool_contexts: HashMap<String, InstalledAgentToolContext>,
    permission_modes: SessionPermissionModes,
    web_tool_state: Arc<WebToolState>,
    project_root: PathBuf,
    model: String,
    mode: String,
    account_scope: String,
    /// Cancelled when this runtime stops. Detached helpers such as side
    /// questions derive their tokens from it so logout and Stop end them.
    lifetime: CancellationToken,
}

struct InstalledAgentToolContext {
    installation_id: u64,
    context: SharedAgentToolContext,
    owner: AgentToolContextOwner,
    python: Option<Arc<PythonTaskBinding>>,
}

impl InstalledAgentToolContext {
    /// Close admission synchronously; observing cleanup never holds Maple's locks.
    fn retire_python(&self, reason: &str) {
        if let Some(binding) = &self.python {
            let cleanup = binding.retire(reason.to_string());
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if let Err(error) = cleanup.await {
                        log::warn!("Python worker cleanup remains pending: {error}");
                    }
                });
            }
        }
    }

    fn revoke(&self, reason: &str) {
        self.context.revoke();
        self.retire_python(reason);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentToolContextOwner {
    Maple,
    Leased,
}

struct PendingAgentToolContextInstallation {
    context: SharedAgentToolContext,
    cleanup: Option<PendingAgentToolContextCleanup>,
    committed: bool,
}

enum PendingAgentToolContextCleanup {
    Lease {
        service: MapleAgentService,
        access: AgentToolContextAccess,
    },
    Created {
        service: MapleAgentService,
        access: AgentToolContextAccess,
        agent_manager: Arc<AgentManager>,
        session_manager: Arc<SessionManager>,
        expected: Box<Session>,
    },
}

impl PendingAgentToolContextInstallation {
    fn new(context: SharedAgentToolContext) -> Self {
        Self {
            context,
            cleanup: None,
            committed: false,
        }
    }

    fn arm_lease_cleanup(&mut self, service: MapleAgentService, access: AgentToolContextAccess) {
        self.cleanup = Some(PendingAgentToolContextCleanup::Lease { service, access });
    }

    fn arm_created_cleanup(
        &mut self,
        service: MapleAgentService,
        access: AgentToolContextAccess,
        agent_manager: Arc<AgentManager>,
        session_manager: Arc<SessionManager>,
        expected: Session,
    ) {
        self.cleanup = Some(PendingAgentToolContextCleanup::Created {
            service,
            access,
            agent_manager,
            session_manager,
            expected: Box::new(expected),
        });
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PendingAgentToolContextInstallation {
    fn drop(&mut self) {
        if !self.committed {
            self.context.revoke();
            let Some(cleanup) = self.cleanup.take() else {
                return;
            };
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    match cleanup {
                        PendingAgentToolContextCleanup::Lease { service, access } => {
                            release_tool_context_lease(service, access).await;
                        }
                        PendingAgentToolContextCleanup::Created {
                            service,
                            access,
                            agent_manager,
                            session_manager,
                            expected,
                        } => {
                            cleanup_provisional_created_session(
                                service,
                                access,
                                agent_manager,
                                session_manager,
                                *expected,
                            )
                            .await;
                        }
                    }
                });
            }
        }
    }
}

async fn run_external_surface_setup<T>(
    setup_cancel: &CancellationToken,
    tool_context: &SharedAgentToolContext,
    cancellation_error: &'static str,
    setup: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    // The exact lease is installed before setup releases Maple's lifecycle
    // fences. Make every later await cancellation-selectable, not just the MCP
    // handshake, so a close/disconnect cannot strand an unpublished leased
    // context while session reload or Agent configuration is stalled.
    tokio::select! {
        biased;
        _ = setup_cancel.cancelled() => {
            tool_context.revoke();
            Err(cancellation_error.to_string())
        }
        result = setup => result,
    }
}

fn take_matching_tool_context(
    contexts: &mut HashMap<String, InstalledAgentToolContext>,
    session_id: &str,
    installation_id: u64,
    context: &SharedAgentToolContext,
) -> Option<InstalledAgentToolContext> {
    let matches = contexts.get(session_id).is_some_and(|installed| {
        installed.installation_id == installation_id && installed.context.ptr_eq(context)
    });
    matches.then(|| {
        contexts
            .remove(session_id)
            .expect("matching Agent tool context must still exist")
    })
}

fn matching_leased_tool_context(
    contexts: &HashMap<String, InstalledAgentToolContext>,
    session_id: &str,
    installation_id: u64,
    context: &SharedAgentToolContext,
) -> bool {
    contexts.get(session_id).is_some_and(|installed| {
        installed.owner == AgentToolContextOwner::Leased
            && installed.installation_id == installation_id
            && installed.context.ptr_eq(context)
            && !installed.context.is_revoked()
    })
}

fn ensure_external_surface_loadable_session(session: &Session) -> Result<(), String> {
    if is_caller_mediated_mode(session.goose_mode) || session.goose_mode == GooseMode::Auto {
        Ok(())
    } else {
        Err(
            "Maple ACP can load only Read only and Approve all tasks; this task's saved approval mode was left unchanged"
                .to_string(),
        )
    }
}

fn is_unprompted_acp_session(session: &Session) -> bool {
    session.session_type == SessionType::Acp
        && session.message_count == 0
        && !session.user_set_name
        && session
            .conversation
            .as_ref()
            .is_none_or(|conversation| conversation.messages().is_empty())
}

/// How long a zero-message ACP task may sit before the desktop runtime
/// treats it as stranded. A live `maple-gpui acp` process creates its
/// provisional row moments before the first prompt; only rows well past
/// that window are crash leftovers.
const UNPROMPTED_ACP_SESSION_SWEEP_AGE: chrono::Duration = chrono::Duration::minutes(10);

/// Whether a session is one of the user's tasks.
///
/// Goose has seven session types and makes sessions of its own for work
/// that is not a task: one per subagent, one per scheduled run, and more
/// with every release. Name the two Maple owns instead of excluding the
/// rest, so a new Goose type cannot appear in the task list by default.
fn is_user_facing_session(session: &Session) -> bool {
    match session.session_type {
        // A task the user started here, or one an ACP client started.
        SessionType::User | SessionType::Acp => true,
        SessionType::SubAgent
        | SessionType::Scheduled
        | SessionType::Hidden
        | SessionType::Terminal
        | SessionType::Gateway => false,
    }
}

fn is_stale_unprompted_acp_session(session: &Session, now: chrono::DateTime<chrono::Utc>) -> bool {
    is_unprompted_acp_session(session)
        && now.signed_duration_since(session.updated_at) >= UNPROMPTED_ACP_SESSION_SWEEP_AGE
}

async fn sweep_unprompted_acp_sessions(session_manager: &SessionManager) {
    let sessions = match session_manager.list_all_sessions().await {
        Ok(sessions) => sessions,
        Err(error) => {
            log::warn!("Failed to inspect stale provisional ACP tasks: {error}");
            return;
        }
    };
    let now = chrono::Utc::now();
    for session in sessions {
        if is_stale_unprompted_acp_session(&session, now)
            && let Err(error) = session_manager.delete_session(&session.id).await
        {
            log::warn!(
                "Failed to remove stale provisional ACP task {}: {error}",
                session.id
            );
        }
    }
}

fn resolve_session_tool_context(
    contexts: &mut HashMap<String, InstalledAgentToolContext>,
    account_scope: &str,
    session_id: &str,
    access: Option<&AgentToolContextAccess>,
    default_spec: &AgentToolContextSpec,
) -> Result<SharedAgentToolContext, String> {
    if let Some(access) = access {
        if access.account_scope.as_ref() != account_scope
            || access.session_id.as_ref() != session_id
        {
            return Err("Agent tool context access does not match this task".to_string());
        }
        let installed = contexts
            .get(session_id)
            .filter(|installed| {
                installed.owner == AgentToolContextOwner::Leased
                    && installed.installation_id == access.installation_id
                    && installed.context.ptr_eq(&access.context)
                    && !installed.context.is_revoked()
            })
            .ok_or_else(|| AGENT_TOOL_CONTEXT_INACTIVE_ERROR.to_string())?;
        return Ok(installed.context.clone());
    }

    if let Some(installed) = contexts.get(session_id) {
        if installed.owner == AgentToolContextOwner::Leased {
            return Err("Agent task is controlled by another Agent surface".to_string());
        }
        if installed.context.is_revoked() {
            contexts.remove(session_id);
        } else {
            return Ok(installed.context.clone());
        }
    }
    let context = SharedAgentToolContext::new(default_spec.clone());
    contexts.insert(
        session_id.to_string(),
        InstalledAgentToolContext {
            installation_id: next_tool_context_installation_id(),
            context: context.clone(),
            owner: AgentToolContextOwner::Maple,
            python: None,
        },
    );
    Ok(context)
}

fn python_task_key(account_scope: &str, session_id: &str) -> String {
    // The existing context installation fences authority. A stable task key also
    // keeps replacement owners behind this task's exact process cleanup barrier.
    format!("{account_scope}:{session_id}")
}

fn resolve_session_python(
    service: &MapleAgentService,
    runtime: &mut AgentRuntime,
    session: &Session,
    context: &SharedAgentToolContext,
    routing: AgentPermissionRouting,
) -> Result<Arc<PythonTaskBinding>, String> {
    let installed = runtime
        .session_tool_contexts
        .get_mut(&session.id)
        .filter(|installed| installed.context.ptr_eq(context) && !context.is_revoked())
        .ok_or_else(|| AGENT_TOOL_CONTEXT_INACTIVE_ERROR.to_string())?;
    if let Some(binding) = &installed.python {
        if !binding.matches_root(&session.working_dir) {
            return Err("Python task root does not match the persisted Agent task".to_string());
        }
        if !binding.is_closed() {
            return Ok(Arc::clone(binding));
        }
    }
    let state_loss_reason = installed
        .python
        .as_ref()
        .and_then(|binding| binding.status().state_loss_reason)
        .or_else(|| {
            (session.message_count > 0).then(|| {
                "This Python owner starts fresh. Any previous scratchpad state is not restored after ownership changes or Maple restarts.".to_string()
            })
        });
    let diagnostics = python_capacity_diagnostics(
        Arc::downgrade(&service.inner),
        runtime.account_scope.clone(),
        session.id.clone(),
        installed.installation_id,
        context.clone(),
        runtime.lifetime.clone(),
        Arc::clone(&runtime.session_manager),
        routing,
    );
    let binding = Arc::new(
        PythonTaskBinding::new(
            service.python_runtime.clone(),
            python_task_key(&runtime.account_scope, &session.id),
            session.id.clone(),
            session.working_dir.clone(),
            context.clone(),
        )
        .with_capacity_diagnostics(diagnostics)
        .with_state_loss_reason(state_loss_reason),
    );
    installed.python = Some(Arc::clone(&binding));
    Ok(binding)
}

#[allow(clippy::too_many_arguments)]
fn python_capacity_diagnostics(
    runtime: Weak<Mutex<Option<AgentRuntime>>>,
    account_scope: String,
    session_id: String,
    installation_id: u64,
    context: SharedAgentToolContext,
    lifetime: CancellationToken,
    session_manager: Arc<SessionManager>,
    routing: AgentPermissionRouting,
) -> code_mode::CapacityDiagnostics {
    Arc::new(move |holders| {
        let runtime = runtime.clone();
        let account_scope = account_scope.clone();
        let session_id = session_id.clone();
        let context = context.clone();
        let lifetime = lifetime.clone();
        let session_manager = Arc::clone(&session_manager);
        Box::pin(async move {
            let mut accessible = HashMap::new();
            if !context.is_revoked()
                && !lifetime.is_cancelled()
                && let Some(runtime) = runtime.upgrade()
            {
                let guard = runtime.lock().await;
                if let Some(current) = guard.as_ref()
                    && current.account_scope == account_scope
                    && Arc::ptr_eq(&current.session_manager, &session_manager)
                    && current
                        .session_tool_contexts
                        .get(&session_id)
                        .is_some_and(|installed| {
                            installed.installation_id == installation_id
                                && installed.context.ptr_eq(&context)
                        })
                {
                    for (id, installed) in &current.session_tool_contexts {
                        // ACP exposes an exact task capability, not authority to
                        // inspect another connection's tasks in the same account.
                        if routing == AgentPermissionRouting::CallingSurface && id != &session_id {
                            continue;
                        }
                        if let Some(binding) = &installed.python {
                            accessible
                                .insert(binding.key().to_string(), (id.clone(), installed.owner));
                        }
                    }
                }
            }
            let mut named = Vec::new();
            for holder in holders.iter().take(maple_code_mode::MAX_WORKERS) {
                if let Some((id, owner)) = accessible.get(&holder.key)
                    && let Ok(session) = session_manager.get_session(id, false).await
                {
                    named.push((session.name, *owner, holder.phase.clone()));
                }
            }
            // Metadata I/O ran outside lifecycle locks; revoked owners must not
            // receive names from the account that just stopped or changed.
            if context.is_revoked() || lifetime.is_cancelled() {
                named.clear();
            }
            format_python_capacity(holders.len(), named, routing)
        })
    })
}

fn format_python_capacity(
    count: usize,
    named: Vec<(String, AgentToolContextOwner, maple_code_mode::WorkerPhase)>,
    routing: AgentPermissionRouting,
) -> String {
    let inaccessible = count.saturating_sub(named.len());
    let mut message = format!(
        "All {} Python worker slots are occupied.",
        maple_code_mode::MAX_WORKERS
    );
    for (title, owner, phase) in named {
        let title = title
            .chars()
            .take(MAX_AGENT_SESSION_TITLE_CHARS)
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        let phase = match phase {
            maple_code_mode::WorkerPhase::Empty => "empty",
            maple_code_mode::WorkerPhase::Starting => "starting",
            maple_code_mode::WorkerPhase::Idle => "idle",
            maple_code_mode::WorkerPhase::Executing => "executing",
            maple_code_mode::WorkerPhase::Retiring => "retiring",
            maple_code_mode::WorkerPhase::CleanupPending => "cleanup pending",
        };
        let remedy = if owner == AgentToolContextOwner::Leased {
            "its owning client can close the session or connection"
        } else {
            "finish or Stop its active run, then use Reset Python"
        };
        message.push_str(&format!("\n- {title}: {phase}; {remedy}."));
    }
    if inaccessible > 0 {
        message.push_str(&format!("\n{inaccessible} slot(s) belong to other or retired owners; their task details are unavailable."));
    }
    if routing == AgentPermissionRouting::CallingSurface {
        message.push_str("\nIn a session that owns a worker, call python_code with {\"code\":\"\",\"reset\":true}, or have its caller use session/close or close the connection. Idle session/cancel does not free a worker.");
    } else {
        message.push_str(
            "\nUse Reset Python on an idle task, or close the owning external session/connection.",
        );
    }
    message.push_str(" Slots become available only after cleanup completes. No task was evicted.");
    message
}

impl AgentRuntime {
    fn desktop_status(&self) -> AgentRuntimeStatus {
        AgentRuntimeStatus {
            running: true,
            project_root: Some(path_string(&self.project_root)),
            model: Some(self.model.clone()),
            mode: Some(self.mode.clone()),
            // AgentRuntimeStatus is Maple Desktop's projection. Calling surfaces
            // retain their own run handles and lifecycle signals instead of
            // becoming actionable through the Tauri command boundary.
            active_runs: active_run_status(self.active_runs.iter().map(|(run_id, run)| {
                (
                    run_id.as_str(),
                    run.session_id.as_str(),
                    run.permission_routing,
                )
            })),
        }
    }
}

fn active_run_status<'a>(
    runs: impl IntoIterator<Item = (&'a str, &'a str, AgentPermissionRouting)>,
) -> HashMap<String, String> {
    runs.into_iter()
        .filter(|(_, _, routing)| *routing == AgentPermissionRouting::Desktop)
        .map(|(run_id, session_id, _)| (session_id.to_string(), run_id.to_string()))
        .collect()
}

#[derive(Clone)]
pub struct AgentPathLayout {
    config_root: PathBuf,
    local_data_root: PathBuf,
}

impl AgentPathLayout {
    pub fn from_app_roots(app_config_root: PathBuf, app_local_data_root: PathBuf) -> Self {
        Self {
            config_root: app_config_root.join("agent"),
            local_data_root: app_local_data_root.join("agent"),
        }
    }
}

pub trait AgentEventSink: Send + Sync + 'static {
    fn emit(&self, event: &AgentServiceEvent);
}

#[derive(Clone)]
pub(crate) struct AgentEventDispatcher {
    sink: Arc<dyn AgentEventSink>,
}

impl AgentEventDispatcher {
    fn new(sink: Arc<dyn AgentEventSink>) -> Self {
        Self { sink }
    }
}

#[derive(Clone)]
struct AgentRunEventPublisher {
    dispatcher: AgentEventDispatcher,
    session_id: Arc<str>,
    run_id: Arc<str>,
    sender: mpsc::Sender<AgentRunEvent>,
    order: Arc<Mutex<()>>,
    host_events: AgentHostEventPolicy,
    overflowed: Arc<AtomicBool>,
}

impl AgentRunEventPublisher {
    fn new(
        dispatcher: AgentEventDispatcher,
        session_id: String,
        run_id: String,
        host_events: AgentHostEventPolicy,
    ) -> (Self, mpsc::Receiver<AgentRunEvent>) {
        let (sender, receiver) = mpsc::channel(AGENT_RUN_EVENT_CAPACITY);
        (
            Self {
                dispatcher,
                session_id: Arc::from(session_id),
                run_id: Arc::from(run_id),
                sender,
                order: Arc::new(Mutex::new(())),
                host_events,
                overflowed: Arc::new(AtomicBool::new(false)),
            },
            receiver,
        )
    }

    fn overflow_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.overflowed)
    }

    async fn publish(&self, event: AgentRunEvent) {
        let _order = self.order.lock().await;
        if self.host_events.publishes() {
            emit_agent_event(
                &self.dispatcher,
                AgentServiceEvent::Run {
                    session_id: self.session_id.to_string(),
                    run_id: self.run_id.to_string(),
                    event: event.clone(),
                },
            );
        }
        // Desktop deliberately drops this receiver after obtaining the run ID.
        // ACP retains it as an isolated, bounded stream for the run. A slow
        // protocol consumer must never backpressure Goose or lifecycle cleanup.
        // Queue saturation is retained as an explicit error signal so no ACP
        // caller can mistake a truncated stream for a complete response.
        match self.sender.try_send(event) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.overflowed.store(true, Ordering::Release);
            }
        }
    }
}

#[derive(Clone)]
pub struct MapleAgentHostResources {
    paths: AgentPathLayout,
    events: AgentEventDispatcher,
    default_tool_context: AgentToolContextSpec,
    /// Opening system prompt text from the host: who the agent is and how
    /// it behaves. A task's caller-supplied prompt (ACP) replaces it. The
    /// host may change it at any time; agents built afterwards use the new
    /// text.
    harness_instructions: Arc<std::sync::RwLock<String>>,
}

impl MapleAgentHostResources {
    pub fn new(
        paths: AgentPathLayout,
        event_sink: Arc<dyn AgentEventSink>,
        default_tool_context: AgentToolContextSpec,
        harness_instructions: String,
    ) -> Self {
        Self {
            paths,
            events: AgentEventDispatcher::new(event_sink),
            default_tool_context,
            harness_instructions: Arc::new(std::sync::RwLock::new(harness_instructions)),
        }
    }

    fn harness_instructions(&self) -> String {
        self.harness_instructions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[derive(Clone)]
pub struct MapleAgentService {
    /// Routes ask_user questions to the UI and answers back.
    host: MapleAgentHostResources,
    inner: Arc<Mutex<Option<AgentRuntime>>>,
    /// Process owners and permits survive replacement of the account runtime.
    python_runtime: maple_code_mode::Runtime,
    runtime_lifecycle: Arc<Mutex<()>>,
    #[cfg(target_os = "macos")]
    login_shell_search_paths: Arc<tokio::sync::OnceCell<Vec<String>>>,
    account_generations: Arc<Mutex<HashMap<String, u64>>>,
    session_lifecycle: Arc<Mutex<()>>,
    session_title_lifecycles: SessionTitleLifecycles,
    pending_permissions: PendingPermissions,
    live_timelines: LiveTimelines,
    /// Subagents per task. A background subagent outlives the run that
    /// started it, so this cannot live inside one run.
    subagents: SessionSubagents,
    desktop_queues: Arc<Mutex<HashMap<(String, String), DesktopSessionQueue>>>,
    admission: Arc<AtomicU8>,
    /// The question broker this service installed as the process global.
    questions: questions::QuestionBroker,
}

#[derive(Clone)]
pub struct AgentRuntimeHandle {
    service: MapleAgentService,
    user_id: Arc<str>,
    account_scope: Arc<str>,
    generation: u64,
}

type LiveTimelines = Arc<Mutex<HashMap<String, LiveTimelineEntry>>>;
type SessionSubagents = Arc<Mutex<HashMap<String, SubagentTracker>>>;

/// Orders title-sensitive reads, writes, and update events within one account task.
/// Weak entries avoid retaining completed task locks for the service lifetime.
type SessionTitleLifecycles = Arc<Mutex<HashMap<(String, String), Weak<Mutex<()>>>>>;

async fn resolve_session_title_lifecycle(
    lifecycles: &SessionTitleLifecycles,
    account_scope: &str,
    session_id: &str,
) -> Arc<Mutex<()>> {
    let key = (account_scope.to_string(), session_id.to_string());
    let mut lifecycles = lifecycles.lock().await;
    if let Some(lifecycle) = lifecycles.get(&key).and_then(Weak::upgrade) {
        return lifecycle;
    }
    lifecycles.retain(|_, lifecycle| lifecycle.strong_count() > 0);
    let lifecycle = Arc::new(Mutex::new(()));
    lifecycles.insert(key, Arc::downgrade(&lifecycle));
    lifecycle
}

#[derive(Clone, Debug, PartialEq)]
struct LiveTimelineEntry {
    routing: AgentPermissionRouting,
    timeline: LiveTimeline,
}

#[derive(Clone, Debug, PartialEq)]
enum LiveTimeline {
    /// The current turn is still emitting events, so this is the authoritative
    /// presentation suffix from its real-user boundary onward.
    Streaming(Vec<AgentTimelineItem>),
    /// Goose finished the turn. Most terminal messages are persisted, but its
    /// synthetic provider errors and notices can be live-only. Resolve that
    /// distinction against the conversation already loaded for the next view.
    Completed(LiveMessageCandidate),
    /// A Maple/Goose task failure is never part of provider history. Keep only
    /// its bounded user-facing error row between views and retries.
    Failed(Vec<AgentTimelineItem>),
}

impl LiveTimeline {
    fn items(&self) -> &[AgentTimelineItem] {
        match self {
            Self::Streaming(items) => items,
            Self::Completed(candidate) => &candidate.items,
            Self::Failed(items) => items,
        }
    }

    fn items_mut(&mut self) -> &mut Vec<AgentTimelineItem> {
        match self {
            Self::Streaming(items) => items,
            Self::Completed(candidate) => &mut candidate.items,
            Self::Failed(items) => items,
        }
    }
}

impl MapleAgentService {
    pub fn new(host: MapleAgentHostResources) -> Self {
        let questions = questions::init_global(host.events.clone());
        Self {
            host,
            questions,
            inner: Arc::new(Mutex::new(None)),
            python_runtime: maple_code_mode::Runtime::new(Default::default()),
            runtime_lifecycle: Arc::new(Mutex::new(())),
            #[cfg(target_os = "macos")]
            login_shell_search_paths: Arc::new(tokio::sync::OnceCell::new()),
            account_generations: Arc::new(Mutex::new(HashMap::new())),
            session_lifecycle: Arc::new(Mutex::new(())),
            session_title_lifecycles: Arc::new(Mutex::new(HashMap::new())),
            pending_permissions: Arc::new(Mutex::new(HashMap::new())),
            live_timelines: Arc::new(Mutex::new(HashMap::new())),
            subagents: Arc::new(Mutex::new(HashMap::new())),
            desktop_queues: Arc::new(Mutex::new(HashMap::new())),
            admission: Arc::new(AtomicU8::new(AGENT_SERVICE_OPEN)),
        }
    }

    #[cfg(test)]
    fn question_broker(&self) -> questions::QuestionBroker {
        self.questions.clone()
    }

    /// Bind subsequent operations to one Maple account and one data generation.
    ///
    /// Desktop commands create a fresh handle at their boundary. Long-lived
    /// adapters such as ACP retain a handle, which makes account clearing an
    /// explicit revocation point instead of silently rebinding the adapter.
    pub async fn handle_for_user(&self, user_id: &str) -> Result<AgentRuntimeHandle, String> {
        let account_scope = account_scope(user_id)?;
        let generation = account_generation(self, &account_scope).await;
        Ok(AgentRuntimeHandle {
            service: self.clone(),
            user_id: Arc::from(user_id),
            account_scope: Arc::from(account_scope),
            generation,
        })
    }

    /// Deliver the user's answer to a pending ask_user question.
    pub async fn answer_question(&self, request_id: &str, answer: String) -> bool {
        // The ask_user tool registers its pending question in the
        // process-global broker (`questions::global()`). This service
        // installed its own broker as that global, so answer through it
        // first; fall back to the global in case a newer service replaced
        // it, or the run blocks forever waiting for a reply.
        if self.questions.answer(request_id, answer.clone()).await {
            return true;
        }
        match questions::global() {
            Some(broker) if !broker.same_as(&self.questions) => {
                broker.answer(request_id, answer).await
            }
            _ => false,
        }
    }

    /// Slash commands available in `working_dir`: the installed skills,
    /// normalized the way goose's slash-command layer does.
    pub fn list_slash_commands(&self, working_dir: Option<&str>) -> Vec<AgentSlashCommand> {
        goose::slash_commands::skill_slash_command::list_commands(
            working_dir.map(std::path::Path::new),
        )
        .into_iter()
        .map(|entry| AgentSlashCommand {
            name: entry.name,
            description: entry.description,
            input_hint: entry.input_hint,
        })
        .collect()
    }

    /// Expand `/command args` from the installed skills into the prompt that
    /// activates the skill. `None` when no skill matches the command.
    pub fn resolve_slash_command(
        &self,
        working_dir: Option<&str>,
        command: &str,
        args: &str,
    ) -> Result<Option<String>, String> {
        goose::slash_commands::skill_slash_command::resolve_command(
            command,
            args,
            working_dir.map(std::path::Path::new),
        )
    }
    /// Stop admitting mutations before host teardown begins. Existing work and
    /// cleanup operations remain able to drain through their dedicated paths.
    pub fn begin_draining(&self) {
        self.admission
            .store(AGENT_SERVICE_DRAINING, Ordering::Release);
    }

    /// Reopen admission only when a requested update restart was abandoned and
    /// the current Maple process will continue serving the user.
    pub fn reopen_after_failed_shutdown(&self) {
        self.admission.store(AGENT_SERVICE_OPEN, Ordering::Release);
    }

    pub fn ensure_accepting_new_work(&self) -> Result<(), String> {
        if self.admission.load(Ordering::Acquire) == AGENT_SERVICE_OPEN {
            Ok(())
        } else {
            Err(AGENT_SERVICE_DRAINING_ERROR.to_string())
        }
    }
}

impl AgentRuntimeHandle {
    /// The task's current display title, or `None` when it does not exist.
    /// Semantic titles land asynchronously between turns, so callers sync
    /// on this rather than waiting for an event.
    pub async fn session_display_title(&self, session_id: &str) -> Result<Option<String>, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let session_manager = {
            let runtime = state.inner.lock().await;
            match runtime.as_ref() {
                Some(current) => {
                    ensure_runtime_account(current, self.account_scope.as_ref())?;
                    Arc::clone(&current.session_manager)
                }
                None => return Ok(None),
            }
        };
        let session = session_manager
            .get_session(session_id, false)
            .await
            .map_err(|error| format!("Failed to load Agent task: {error}"))?;
        Ok(Some(session.name))
    }

    /// Slash commands available in `working_dir` (installed skills).
    pub fn slash_commands(&self, working_dir: Option<&str>) -> Vec<AgentSlashCommand> {
        self.service.list_slash_commands(working_dir)
    }

    /// Expand `/command args` from the installed skills into the prompt that
    /// activates the skill. `None` when no skill matches the command.
    pub fn expand_slash_command(
        &self,
        working_dir: Option<&str>,
        command: &str,
        args: &str,
    ) -> Result<Option<String>, String> {
        self.service
            .resolve_slash_command(working_dir, command, args)
    }

    /// Compact a session's history now (manual /compact). The runtime's
    /// compaction summarizes the conversation and replaces its history;
    /// callers should reload the session afterwards.
    pub async fn compact_session(&self, session_id: String) -> Result<(), String> {
        let state = &self.service;
        let runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let (agent_manager, session_manager, maple_api_session) = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, &self.account_scope)?;
            // Compaction rewrites the session history; running it under an
            // active turn would race the turn's own history writes.
            if has_active_session_run(&current.active_runs, &session_id) {
                return Err("This Agent task is already running".to_string());
            }
            (
                Arc::clone(&current.agent_manager),
                Arc::clone(&current.session_manager),
                Arc::clone(&current.maple_api_session),
            )
        };
        let session = session_manager
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to load Agent task: {error}"))?;
        // A cold agent must be created through the session-aware path so it
        // gets the Maple system prompt and the session's locked model instead
        // of goose's default-provider fallback, which would persist the
        // runtime-global model into the session.
        let agent = get_or_create_session_agent(
            &agent_manager,
            &maple_api_session,
            &session,
            &state.host.harness_instructions(),
            RuntimeContext::default(),
        )
        .await?
        .agent;
        // Compaction is a full model round-trip. Holding the lifecycle fence
        // for its duration would block every other runtime operation.
        drop(runtime_lifecycle_guard);
        agent
            .execute_command("/compact", &session_id)
            .await
            .map_err(|error| format!("Compaction failed: {error}"))?;
        Ok(())
    }

    /// Summarize one completed tool call with the cheap title model
    /// (llama3-3-70b over the Maple provider). Used by the transcript's
    /// collapsed tool cards; `None` when the model returned nothing usable.
    pub async fn summarize_tool_call(
        &self,
        session_id: &str,
        tool_name: &str,
        input: Option<&serde_json::Value>,
        output_text: &str,
    ) -> Result<Option<String>, String> {
        let truncate = |text: &str| -> String {
            text.chars()
                .take(TOOL_SUMMARY_MAX_INPUT_CHARS)
                .collect::<String>()
        };
        let input_line = match input {
            Some(value) if !value.is_null() => {
                truncate(&serde_json::to_string(value).unwrap_or_default())
            }
            _ => String::new(),
        };
        let prompt = format!(
            "Tool: {tool_name}\nInput: {input_line}\nOutput: {}",
            truncate(output_text)
        );
        self.run_summary_model(session_id, TOOL_SUMMARY_SYSTEM_PROMPT, prompt)
            .await
    }

    /// Summarize one finished thinking block with the same cheap title
    /// model. Used as the header of the transcript's thinking rows.
    pub async fn summarize_thinking(
        &self,
        session_id: &str,
        thinking_text: &str,
    ) -> Result<Option<String>, String> {
        let prompt = thinking_text
            .chars()
            .take(TOOL_SUMMARY_MAX_INPUT_CHARS)
            .collect::<String>();
        self.run_summary_model(session_id, THINKING_SUMMARY_SYSTEM_PROMPT, prompt)
            .await
    }

    /// One round-trip to the title model shared by the tool-call and
    /// thinking summaries.
    async fn run_summary_model(
        &self,
        session_id: &str,
        system_prompt: &str,
        prompt: String,
    ) -> Result<Option<String>, String> {
        let state = &self.service;
        let runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        // The summary only needs the account's Maple transport. Building a
        // provider from it avoids `get_or_create_agent`, which would start
        // the session's MCP servers just to label one tool call.
        let maple_api_session = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, &self.account_scope)?;
            Arc::clone(&current.maple_api_session)
        };
        // Release the lifecycle fence before the model round-trip so sends,
        // cancels, and shutdown do not wait on a summary.
        drop(runtime_lifecycle_guard);
        let provider: Arc<dyn goose::providers::base::Provider> =
            Arc::new(MapleProvider::new(maple_api_session));
        let mut model_config =
            goose::model_config::model_config_from_user_config_with_session_settings(
                MAPLE_PROVIDER_NAME,
                TOOL_SUMMARY_MODEL,
                None,
                None,
                None,
            )
            .map_err(|error| format!("Failed to configure Agent summary model: {error}"))?;
        model_config.request_params = None;
        model_config.reasoning = Some(false);
        let model_config = model_config
            .with_temperature(Some(TOOL_SUMMARY_TEMPERATURE))
            .with_max_tokens(Some(TOOL_SUMMARY_MAX_TOKENS));

        let messages = [Message::user().with_text(prompt)];
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let generation = provider::with_run_cancellation(
            cancel_token.clone(),
            goose::session_context::with_session_id(
                Some(session_id.to_string()),
                provider.complete(&model_config, system_prompt, &messages, &[]),
            ),
        );
        tokio::pin!(generation);
        let completion = tokio::select! {
            result = &mut generation => result,
            _ = tokio::time::sleep(TOOL_SUMMARY_TIMEOUT) => {
                cancel_token.cancel();
                // Keep the provider's credential-reconciliation task from
                // outliving the timeout, like title generation does.
                let _ = generation.await;
                return Err("Summary timed out".to_string());
            }
        }
        .map_err(|error| format!("Failed to summarize: {error}"))?;
        Ok(normalize_tool_summary(&completion.0.as_concat_text()))
    }

    /// Answer a `/btw` side question against a fork of the session. The
    /// request repeats the session's system prompt, tool list, and message
    /// history so the provider reuses its prompt cache, then the earlier
    /// turns of the side thread (`prior`) and the new question. The tools
    /// are sent but the model may not call them (`tool_choice: none`).
    /// Nothing is written to the session; the answer streams out as
    /// `AgentServiceEvent::SideQuestion` events tagged with `request_id`.
    pub async fn ask_side_question(
        &self,
        session_id: &str,
        request_id: String,
        prior: Vec<SideQuestionTurn>,
        question: String,
    ) -> Result<(), String> {
        let question = question.trim().to_string();
        if question.is_empty() {
            return Err("Question cannot be empty".to_string());
        }
        let state = &self.service;
        let runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let (agent_manager, session_manager, maple_api_session, cancel_token) = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, &self.account_scope)?;
            (
                Arc::clone(&current.agent_manager),
                Arc::clone(&current.session_manager),
                Arc::clone(&current.maple_api_session),
                // Stop and logout cancel the runtime lifetime; the detached
                // streaming task below must end with it.
                current.lifetime.child_token(),
            )
        };
        let session = session_manager
            .get_session(session_id, true)
            .await
            .map_err(|error| format!("Failed to load Agent task: {error}"))?;
        // A session that already ran in this process keeps its Agent, so the
        // tools and prompt below are the ones its runs send.
        let harness_instructions = state.host.harness_instructions();
        let agent = get_or_create_session_agent(
            &agent_manager,
            &maple_api_session,
            &session,
            &harness_instructions,
            RuntimeContext::default(),
        )
        .await?
        .agent;
        // Release the lifecycle fence before the model round-trip so sends,
        // cancels, and shutdown do not wait on a side question.
        drop(runtime_lifecycle_guard);

        let (tools, _toolshim_tools, mut system_prompt, model_config) = agent
            .prepare_tools_and_prompt(session_id, &session.working_dir)
            .await
            .map_err(|error| format!("Failed to prepare the side question: {error}"))?;
        if let Some(addendum) = project_instructions_addendum(&session) {
            system_prompt = format!("{system_prompt}\n\n{addendum}");
        }
        let provider: Arc<dyn goose::providers::base::Provider> = match agent.provider().await {
            Ok(provider) => provider,
            Err(_) => Arc::new(MapleProvider::new(maple_api_session)),
        };
        let model_config = model_config
            .with_default_thinking_effort(
                goose::config::Config::global().get_goose_thinking_effort(),
            )
            .with_merged_request_params(HashMap::from([(
                "tool_choice".to_string(),
                json!("none"),
            )]));
        let mut messages = session
            .conversation
            .map(|conversation| conversation.messages().clone())
            .unwrap_or_default();
        // The framing goes on the first side question only, so a follow-up
        // keeps the earlier side turns as a stable prefix too.
        let mut side_questions = prior
            .iter()
            .map(|turn| turn.question.as_str())
            .chain(std::iter::once(question.as_str()));
        let first = side_questions.next().unwrap_or_default();
        messages.push(Message::user().with_text(format!("{SIDE_QUESTION_PREFIX}\n\n{first}")));
        for (turn, next_question) in prior.iter().zip(side_questions) {
            messages.push(Message::assistant().with_text(turn.answer.clone()));
            messages.push(Message::user().with_text(next_question));
        }
        let messages = messages_for_provider(messages);

        let events = state.host.events.clone();
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            let emit = |event: SideQuestionEvent| {
                emit_agent_event(
                    &events,
                    AgentServiceEvent::SideQuestion {
                        session_id: session_id.clone(),
                        request_id: request_id.clone(),
                        event,
                    },
                );
            };
            let generation = provider::with_run_cancellation(
                cancel_token.clone(),
                goose::session_context::with_session_id(Some(session_id.clone()), async {
                    let mut stream = provider
                        .stream(&model_config, &system_prompt, &messages, &tools)
                        .await
                        .map_err(|error| error.to_string())?;
                    while let Some(item) = stream.next().await {
                        let (message, _usage) = item.map_err(|error| error.to_string())?;
                        let Some(message) = message else {
                            continue;
                        };
                        for content in &message.content {
                            if let MessageContent::Text(text) = content
                                && !text.text.is_empty()
                            {
                                emit(SideQuestionEvent::Chunk(text.text.clone()));
                            }
                        }
                    }
                    Ok::<(), String>(())
                }),
            );
            tokio::pin!(generation);
            let result = tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    Err("Side question cancelled".to_string())
                }
                result = &mut generation => result,
                _ = tokio::time::sleep(SIDE_QUESTION_TIMEOUT) => {
                    cancel_token.cancel();
                    let _ = generation.await;
                    Err("Side question timed out".to_string())
                }
            };
            match result {
                Ok(()) => emit(SideQuestionEvent::Finished),
                Err(error) => emit(SideQuestionEvent::Error(error)),
            }
        });
        Ok(())
    }

    /// Deliver the user's answer to an ask_user question from this
    /// account's runtime. False when nothing was pending.
    pub async fn answer_question_via_handle(
        &self,
        request_id: &str,
        answer: String,
    ) -> Result<bool, String> {
        Ok(self.service.answer_question(request_id, answer).await)
    }

    pub async fn verify_generation(&self) -> Result<(), String> {
        ensure_account_generation(&self.service, &self.account_scope, self.generation).await
    }

    pub fn ensure_accepting_new_work(&self) -> Result<(), String> {
        self.service.ensure_accepting_new_work()
    }
}

fn ensure_runtime_account(runtime: &AgentRuntime, account_scope: &str) -> Result<(), String> {
    ensure_account_scope(&runtime.account_scope, account_scope)
}

fn ensure_account_scope(current_scope: &str, requested_scope: &str) -> Result<(), String> {
    if current_scope == requested_scope {
        Ok(())
    } else {
        Err("Agent runtime belongs to a different signed-in account".to_string())
    }
}

async fn account_generation(state: &MapleAgentService, account_scope: &str) -> u64 {
    *state
        .account_generations
        .lock()
        .await
        .get(account_scope)
        .unwrap_or(&0)
}

async fn ensure_account_generation(
    state: &MapleAgentService,
    account_scope: &str,
    expected: u64,
) -> Result<(), String> {
    if account_generation(state, account_scope).await == expected {
        Ok(())
    } else {
        Err("Agent Mode data changed while this operation was waiting".to_string())
    }
}

#[cfg(test)]
async fn advance_account_generation(state: &MapleAgentService, account_scope: &str) -> u64 {
    let mut generations = state.account_generations.lock().await;
    let generation = generations.entry(account_scope.to_string()).or_default();
    *generation = generation
        .checked_add(1)
        .expect("Agent Mode exhausted its account operation generation");
    *generation
}

fn next_queue_id() -> String {
    let sequence = NEXT_QUEUE_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .expect("Agent Mode exhausted its queue ID sequence");
    format!("queue_{}_{sequence}", unix_ms())
}

/// One printable line from a summary completion: reasoning blocks removed,
/// quotes stripped, whitespace collapsed.
fn normalize_tool_summary(raw: &str) -> Option<String> {
    let without_reasoning = strip_session_title_reasoning_blocks(raw);
    let printable: String = without_reasoning
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
        .collect();
    let first_line = printable
        .lines()
        .find(|line| !line.trim().is_empty())?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let line = first_line
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(&first_line)
        .trim();
    let summary: String = line.chars().take(100).collect();
    (!summary.is_empty()).then_some(summary)
}

fn next_run_id() -> String {
    let sequence = NEXT_RUN_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .expect("Agent Mode exhausted its run ID sequence");
    format!("run_{}_{sequence}", unix_ms())
}

fn next_tool_context_installation_id() -> u64 {
    NEXT_TOOL_CONTEXT_INSTALLATION_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .expect("Agent Mode exhausted its tool context installation IDs")
}

fn session_title_from_prompt(prompt: &str) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_AGENT_SESSION_TITLE_CHARS {
        return collapsed;
    }

    let mut title = collapsed
        .chars()
        .take(MAX_AGENT_SESSION_TITLE_CHARS - 1)
        .collect::<String>();
    title.truncate(title.trim_end().len());
    title.push('…');
    title
}

fn normalize_user_provided_session_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("Agent task title cannot be empty".to_string());
    }
    if title
        .chars()
        .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
    {
        return Err(
            "Agent task title must be a single line without control characters".to_string(),
        );
    }
    let default_ignorable = CodePointSetData::new::<DefaultIgnorableCodePoint>();
    if !title.chars().any(|character| {
        !character.is_whitespace()
            && !character.is_control()
            && !default_ignorable.contains(character)
    }) {
        return Err("Agent task title must contain visible characters".to_string());
    }
    if title.chars().count() > MAX_AGENT_SESSION_TITLE_CHARS {
        return Err(format!(
            "Agent task title must be {MAX_AGENT_SESSION_TITLE_CHARS} characters or fewer"
        ));
    }
    Ok(title.to_string())
}

fn should_name_session_from_prompt(session: &Session) -> bool {
    session.message_count == 0
        && !session.user_set_name
        // ACP tasks start under their connector's placeholder instead of
        // the desktop one; both are unnamed-prompt states.
        && (session.name == DEFAULT_AGENT_SESSION_TITLE
            || session.name == ACP_SESSION_FALLBACK_TITLE)
}

fn strip_session_title_reasoning_blocks(raw: &str) -> String {
    let mut value = raw.to_string();
    for tag in ["think", "analysis"] {
        loop {
            let lowercase = value.to_ascii_lowercase();
            let Some(start) = lowercase.find(&format!("<{tag}")) else {
                break;
            };
            let Some(open_end_offset) = lowercase[start..].find('>') else {
                value.truncate(start);
                break;
            };
            let content_start = start + open_end_offset + 1;
            let close = format!("</{tag}>");
            let Some(close_offset) = lowercase[content_start..].find(&close) else {
                value.truncate(start);
                break;
            };
            let end = content_start + close_offset + close.len();
            value.replace_range(start..end, "");
        }
    }
    value
}

fn normalize_generated_session_title(raw: &str) -> Option<String> {
    let without_reasoning = strip_session_title_reasoning_blocks(raw);
    let printable = without_reasoning
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
        .collect::<String>();
    let first_line = printable
        .lines()
        .find(|line| !line.trim().is_empty())?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let title = first_line
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            first_line
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(&first_line)
        .trim();
    if title.is_empty() {
        return None;
    }
    Some(session_title_from_prompt(title))
}

// Maple keeps Goose's provider and session persistence boundaries, while using
// the same dedicated first-message title policy as Maple Chat. Scheduling stays
// account/session owned instead of using Goose's detached first-three-turn job.
struct AgentSessionTitleGeneration<'a> {
    session_manager: &'a SessionManager,
    agent: &'a Agent,
    session_lifecycle: &'a Arc<Mutex<()>>,
    session_title_lifecycle: &'a Arc<Mutex<()>>,
    session_id: &'a str,
    first_prompt: &'a str,
    expected_fallback_title: &'a str,
    cancel_token: &'a CancellationToken,
    event_target: Option<(&'a AgentEventDispatcher, AgentHostEventPolicy)>,
}

async fn generate_agent_session_title(
    generation: AgentSessionTitleGeneration<'_>,
) -> Result<Option<AgentSessionSummary>, String> {
    let AgentSessionTitleGeneration {
        session_manager,
        agent,
        session_lifecycle,
        session_title_lifecycle,
        session_id,
        first_prompt,
        expected_fallback_title,
        cancel_token: title_cancel_token,
        event_target,
    } = generation;
    let provider = agent
        .provider()
        .await
        .map_err(|error| format!("Failed to resolve Agent title provider: {error}"))?;
    let mut model_config =
        goose::model_config::model_config_from_user_config_with_session_settings(
            provider.get_name(),
            SESSION_TITLE_MODEL,
            None,
            None,
            None,
        )
        .map_err(|error| format!("Failed to configure Agent title model: {error}"))?;
    model_config.request_params = None;
    model_config.reasoning = Some(false);
    let model_config = model_config
        .with_temperature(Some(SESSION_TITLE_TEMPERATURE))
        .with_max_tokens(Some(SESSION_TITLE_MAX_TOKENS));
    let bounded_prompt = first_prompt
        .chars()
        .take(SESSION_TITLE_MAX_INPUT_CHARS)
        .collect::<String>();
    let messages = [Message::user().with_text(format!(
        "Generate a concise, contextual title (3-5 words) for a chat that starts with this message: \"{bounded_prompt}\""
    ))];
    let generation = provider::with_run_cancellation(
        title_cancel_token.clone(),
        goose::session_context::with_session_id(
            Some(session_id.to_string()),
            provider.complete(&model_config, SESSION_TITLE_SYSTEM_PROMPT, &messages, &[]),
        ),
    );
    tokio::pin!(generation);
    let completion = tokio::select! {
        result = &mut generation => result,
        _ = tokio::time::sleep(SESSION_TITLE_GENERATION_TIMEOUT) => {
            title_cancel_token.cancel();
            // Maple's transport retains a credential-reconciliation task until
            // the provider future settles. Drain it so timeout preserves the
            // same account-lifetime barrier as Stop/logout.
            let _ = generation.await;
            return Err("Agent task title generation timed out".to_string());
        }
    }
    .map_err(|error| format!("Failed to generate Agent task title: {error}"))?;
    let Some(title) = normalize_generated_session_title(&completion.0.as_concat_text()) else {
        return Ok(None);
    };

    // Serialize the authority check and write with Maple's other session
    // mutations. Future manual-title entry points must use this same lifecycle
    // lock so a classifier result cannot pass its check and then overwrite them.
    let _session_lifecycle_guard = tokio::select! {
        biased;
        _ = title_cancel_token.cancelled() => return Ok(None),
        guard = session_lifecycle.lock() => guard,
    };
    if title_cancel_token.is_cancelled() {
        return Ok(None);
    }
    let _session_title_lifecycle_guard = tokio::select! {
        biased;
        _ = title_cancel_token.cancelled() => return Ok(None),
        guard = session_title_lifecycle.lock() => guard,
    };
    if title_cancel_token.is_cancelled() {
        return Ok(None);
    }

    // Preserve explicit/source titles and any later title authority. This also
    // prevents a stale auxiliary result from replacing a newer generation.
    let session = session_manager
        .get_session(session_id, false)
        .await
        .map_err(|error| format!("Failed to load Agent task before naming: {error}"))?;
    if session.user_set_name || session.name != expected_fallback_title {
        return Ok(None);
    }
    session_manager
        .update(session_id)
        .system_generated_name(title)
        .apply()
        .await
        .map_err(|error| format!("Failed to persist Agent task title: {error}"))?;
    let session = session_manager
        .get_session(session_id, false)
        .await
        .map_err(|error| format!("Failed to load generated Agent task title: {error}"))?;
    let summary = session_summary(&session);
    if !title_cancel_token.is_cancelled()
        && let Some((dispatcher, _)) = event_target.filter(|(_, policy)| policy.publishes())
    {
        // Keep the authority lock through publication. A later manual title
        // therefore persists and publishes after this semantic snapshot.
        emit_agent_event(
            dispatcher,
            AgentServiceEvent::SessionUpdated {
                session_id: session_id.to_string(),
                run_id: None,
                session: summary.clone(),
            },
        );
    }
    Ok(Some(summary))
}

async fn restore_unused_agent_session_fallback_under_lifecycle(
    session_manager: &SessionManager,
    session_id: &str,
    expected_fallback_title: &str,
) -> Result<Option<AgentSessionSummary>, String> {
    let session = session_manager
        .get_session(session_id, false)
        .await
        .map_err(|error| format!("Failed to inspect unused Agent task title: {error}"))?;
    if session.message_count != 0
        || session.user_set_name
        || session.name != expected_fallback_title
    {
        return Ok(None);
    }
    session_manager
        .update(session_id)
        .system_generated_name(DEFAULT_AGENT_SESSION_TITLE.to_string())
        .apply()
        .await
        .map_err(|error| format!("Failed to restore unused Agent task title: {error}"))?;
    let session = session_manager
        .get_session(session_id, false)
        .await
        .map_err(|error| format!("Failed to load restored Agent task title: {error}"))?;
    Ok(Some(session_summary(&session)))
}

struct AgentSessionTitleJob {
    start: oneshot::Receiver<()>,
    settled: oneshot::Receiver<()>,
    session_manager: Arc<SessionManager>,
    agent: Arc<Agent>,
    session_lifecycle: Arc<Mutex<()>>,
    session_title_lifecycle: Arc<Mutex<()>>,
    session_id: String,
    first_prompt: String,
    expected_fallback_title: String,
    cancel_token: CancellationToken,
    dispatcher: AgentEventDispatcher,
    host_events: AgentHostEventPolicy,
}

async fn run_agent_session_title_task(job: AgentSessionTitleJob) {
    let AgentSessionTitleJob {
        mut start,
        mut settled,
        session_manager,
        agent,
        session_lifecycle,
        session_title_lifecycle,
        session_id,
        first_prompt,
        expected_fallback_title,
        cancel_token,
        dispatcher,
        host_events,
    } = job;
    let should_start = tokio::select! {
        biased;
        _ = cancel_token.cancelled() => return,
        started = &mut start => started.is_ok(),
    };
    if !should_start {
        // A dropped sender (run task aborted or panicked before settling)
        // means the run is over just as much as an explicit send. Only an
        // actual cancellation skips the fallback-title repair below.
        let settled = tokio::select! {
            biased;
            _ = cancel_token.cancelled() => false,
            _ = &mut settled => !cancel_token.is_cancelled(),
        };
        if !settled {
            return;
        }
        let session_lifecycle_guard = tokio::select! {
            biased;
            _ = cancel_token.cancelled() => return,
            guard = session_lifecycle.lock() => guard,
        };
        let _session_title_lifecycle_guard = tokio::select! {
            biased;
            _ = cancel_token.cancelled() => return,
            guard = session_title_lifecycle.lock() => guard,
        };
        let persisted_session = match session_manager.get_session(&session_id, false).await {
            Ok(session) => session,
            Err(error) => {
                if !cancel_token.is_cancelled() {
                    log::warn!("Failed to inspect Agent task after title start closed: {error}");
                }
                return;
            }
        };
        // ACP tasks are created under their connector's default title, not
        // the prompt-derived fallback desktop uses, so both placeholders
        // stay eligible. A title the caller supplied (`user_set_name` or
        // anything else) is never overwritten.
        let persisted_message_needs_title = persisted_session.message_count > 0
            && !persisted_session.user_set_name
            && (persisted_session.name == expected_fallback_title
                || persisted_session.name == DEFAULT_AGENT_SESSION_TITLE
                || persisted_session.name == ACP_SESSION_FALLBACK_TITLE);
        if !persisted_message_needs_title {
            if persisted_session.message_count == 0 {
                match restore_unused_agent_session_fallback_under_lifecycle(
                    session_manager.as_ref(),
                    &session_id,
                    &expected_fallback_title,
                )
                .await
                {
                    Ok(Some(session)) if host_events.publishes() => emit_agent_event(
                        &dispatcher,
                        AgentServiceEvent::SessionUpdated {
                            session_id: session_id.clone(),
                            run_id: None,
                            session,
                        },
                    ),
                    Ok(_) => {}
                    Err(error) => log::warn!("{error}"),
                }
            }
            return;
        }
        // Goose can persist the first message and then fail before returning
        // its reply stream. The semantic title is still valid in that case.
        drop(session_lifecycle_guard);
    }

    let result = generate_agent_session_title(AgentSessionTitleGeneration {
        session_manager: session_manager.as_ref(),
        agent: agent.as_ref(),
        session_lifecycle: &session_lifecycle,
        session_title_lifecycle: &session_title_lifecycle,
        session_id: &session_id,
        first_prompt: &first_prompt,
        expected_fallback_title: &expected_fallback_title,
        cancel_token: &cancel_token,
        event_target: Some((&dispatcher, host_events)),
    })
    .await;
    match result {
        Ok(_) => {}
        Err(error) if !cancel_token.is_cancelled() => {
            log::warn!("{error}");
        }
        Err(_) => {}
    }
}

async fn remove_agent_session_title_task(
    state_inner: &Arc<Mutex<Option<AgentRuntime>>>,
    account_scope: &str,
    session_id: &str,
    run_id: &str,
) {
    let mut runtime = state_inner.lock().await;
    let Some(current) = runtime.as_mut() else {
        return;
    };
    if current.account_scope != account_scope {
        return;
    }
    let owns_registration = current
        .session_title_tasks
        .get(session_id)
        .is_some_and(|task| task.run_id == run_id);
    if owns_registration {
        current.session_title_tasks.remove(session_id);
    }
}

async fn take_pending_permissions_for_runs(
    pending_permissions: &PendingPermissions,
    run_ids: &[String],
) -> Vec<(PendingPermissionKey, PendingAgentPermission)> {
    let mut pending = pending_permissions.lock().await;
    let keys = pending
        .keys()
        .filter(|key| {
            pending
                .get(*key)
                .is_some_and(|request| run_ids.contains(&request.run_id))
        })
        .cloned()
        .collect::<Vec<_>>();
    keys.into_iter()
        .filter_map(|key| pending.remove(&key).map(|request| (key, request)))
        .collect()
}

async fn cancel_pending_permissions_for_runs(
    pending_permissions: &PendingPermissions,
    run_ids: &[String],
    agents_by_run: &HashMap<String, Arc<Agent>>,
) -> Vec<(PendingPermissionKey, PendingAgentPermission)> {
    let mut cancelled = Vec::new();
    for ((session_id, request_id), request) in
        take_pending_permissions_for_runs(pending_permissions, run_ids).await
    {
        if let Some(agent) = agents_by_run.get(&request.run_id) {
            agent
                .handle_confirmation(
                    request_id.clone(),
                    PermissionConfirmation {
                        principal_type: PrincipalType::Tool,
                        permission: Permission::Cancel,
                    },
                )
                .await;
        } else {
            log::warn!(
                "Failed to resolve the running Agent for pending permission {request_id} in {session_id}"
            );
        }
        cancelled.push(((session_id, request_id), request));
    }
    cancelled
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingPermissionRegistration {
    Registered,
    Existing,
    Rejected,
}

async fn register_pending_permission(
    pending_permissions: &PendingPermissions,
    issued_permission_ids: &IssuedPermissionIds,
    session_id: &str,
    run_id: &str,
    routing: AgentPermissionRouting,
    request: AgentPermissionRequest,
    cancel_token: &CancellationToken,
) -> PendingPermissionRegistration {
    if cancel_token.is_cancelled() {
        return PendingPermissionRegistration::Rejected;
    }
    let key = (session_id.to_string(), request.request_id.clone());
    let pending_request = PendingAgentPermission {
        run_id: run_id.to_string(),
        routing,
        request,
    };
    {
        let mut pending = pending_permissions.lock().await;
        match pending.get(&key) {
            Some(existing) if existing == &pending_request => {
                return PendingPermissionRegistration::Existing;
            }
            Some(_) => {
                // Reusing a Goose request ID with different ownership or payload
                // invalidates the old capability. Leaving it resolvable would let
                // a stale caller approve a different operation under the reused ID.
                pending.remove(&key);
                return PendingPermissionRegistration::Rejected;
            }
            None => {}
        }
    }
    {
        let mut issued = issued_permission_ids.lock().await;
        if !issued.insert(key.1.clone()) {
            pending_permissions.lock().await.remove(&key);
            return PendingPermissionRegistration::Rejected;
        }
    }
    let mut pending = pending_permissions.lock().await;
    match pending.entry(key.clone()) {
        std::collections::hash_map::Entry::Occupied(existing) => {
            existing.remove();
            return PendingPermissionRegistration::Rejected;
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(pending_request);
        }
    }
    if cancel_token.is_cancelled() {
        pending.remove(&key);
        PendingPermissionRegistration::Rejected
    } else {
        PendingPermissionRegistration::Registered
    }
}

async fn stop_runtime_for_user(state: &MapleAgentService, user_id: &str) -> Result<(), String> {
    let account_scope = account_scope(user_id)?;
    stop_runtime_inner(state, Some(&account_scope)).await
}

async fn stop_runtime_inner(
    state: &MapleAgentService,
    requested_scope: Option<&str>,
) -> Result<(), String> {
    let session_lifecycle_guard = state.session_lifecycle.lock().await;
    let (active_runs, session_title_tasks, web_tool_state, tool_contexts) = {
        let mut runtime = state.inner.lock().await;
        let Some(current) = runtime.as_mut() else {
            return Ok(());
        };
        if let Some(account_scope) = requested_scope {
            ensure_runtime_account(current, account_scope)?;
        }
        // Ends detached helpers (side questions) that hold no run entry.
        current.lifetime.cancel();
        (
            std::mem::take(&mut current.active_runs),
            std::mem::take(&mut current.session_title_tasks),
            Arc::clone(&current.web_tool_state),
            std::mem::take(&mut current.session_tool_contexts),
        )
    };

    for installed in tool_contexts.into_values() {
        installed.revoke("Python task ownership ended");
    }
    {
        let mut queues = state.desktop_queues.lock().await;
        match requested_scope {
            Some(account_scope) => {
                queues.retain(|(scope, _), _| scope != account_scope);
            }
            None => queues.clear(),
        }
    }

    let run_ids = active_runs.keys().cloned().collect::<Vec<_>>();
    let agents_by_run = active_runs
        .iter()
        .map(|(run_id, run)| (run_id.clone(), Arc::clone(&run.agent)))
        .collect::<HashMap<_, _>>();
    let cancelled_permission_ids_by_run = active_runs
        .iter()
        .map(|(run_id, run)| (run_id.clone(), Arc::clone(&run.cancelled_permission_ids)))
        .collect::<HashMap<_, _>>();
    let mut task_handles = Vec::with_capacity(active_runs.len() + session_title_tasks.len());
    for (_, active_run) in active_runs {
        // Cancel first so an ActionRequired event racing this snapshot will
        // take the immediate-cancel path in register_pending_permission.
        active_run.accepting_queue.store(false, Ordering::Release);
        active_run.tool_context.cancel_run(&active_run.token);
        task_handles.push(active_run.task_handle);
    }
    for (_, title_task) in session_title_tasks {
        title_task.token.cancel();
        task_handles.push(title_task.task_handle);
    }
    let cancelled_permissions =
        cancel_pending_permissions_for_runs(&state.pending_permissions, &run_ids, &agents_by_run)
            .await;
    for ((_, request_id), pending) in cancelled_permissions {
        if let Some(cancelled_permission_ids) = cancelled_permission_ids_by_run.get(&pending.run_id)
        {
            cancelled_permission_ids.lock().await.insert(request_id);
        }
    }
    drop(session_lifecycle_guard);

    join_agent_tasks(task_handles, RUN_SHUTDOWN_TIMEOUT).await;

    state.pending_permissions.lock().await.clear();
    state.live_timelines.lock().await.clear();
    state.subagents.lock().await.clear();
    web_tool_state.clear_all().await;
    *state.inner.lock().await = None;
    Ok(())
}

async fn join_agent_tasks(
    mut task_handles: Vec<tokio::task::JoinHandle<()>>,
    graceful_timeout: std::time::Duration,
) {
    let graceful = futures_util::future::join_all(task_handles.iter_mut());
    if tokio::time::timeout(graceful_timeout, graceful)
        .await
        .is_err()
    {
        for task_handle in &task_handles {
            task_handle.abort();
        }
        // Once abort is requested, join every task without another timeout.
        // Dropping a still-running JoinHandle detaches it and could leave an OS
        // child or old-account event source alive after a new runtime starts.
        let _ = futures_util::future::join_all(task_handles).await;
    }
}

impl MapleAgentService {
    /// Replace the host's opening system prompt text. Agents built after
    /// this call use it; agents already running keep their prompt.
    pub fn set_harness_instructions(&self, harness_instructions: String) {
        *self
            .host
            .harness_instructions
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = harness_instructions;
    }
}

impl AgentRuntimeHandle {
    pub async fn status(&self) -> Result<AgentRuntimeStatus, String> {
        let _runtime_lifecycle_guard = self.service.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let runtime = self.service.inner.lock().await;
        if let Some(current) = runtime.as_ref() {
            ensure_runtime_account(current, &self.account_scope)?;
            return Ok(current.desktop_status());
        }
        Ok(stopped_status())
    }

    /// The subagents still working for a task. A caller that opens a task
    /// whose run has ended reads this to show the background subagents
    /// that work on.
    pub async fn session_subagents(&self, session_id: &str) -> Vec<AgentSubagent> {
        self.service
            .subagents
            .lock()
            .await
            .get(session_id)
            .map(SubagentTracker::snapshot)
            .unwrap_or_default()
    }

    pub async fn start(
        &self,
        maple_api_session: Arc<MapleApiSession>,
        request: Option<AgentStartRequest>,
    ) -> Result<AgentRuntimeStatus, String> {
        let _runtime_lifecycle_guard = self.service.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        start_runtime_for_user(&self.service, maple_api_session, &self.user_id, request).await
    }
}

async fn start_runtime_for_user(
    state: &MapleAgentService,
    maple_api_session: Arc<MapleApiSession>,
    user_id: &str,
    request: Option<AgentStartRequest>,
) -> Result<AgentRuntimeStatus, String> {
    let account_scope = account_scope(user_id)?;
    {
        let runtime = state.inner.lock().await;
        if let Some(current) = runtime.as_ref() {
            ensure_runtime_account(current, &account_scope)?;
            return Ok(current.desktop_status());
        }
    }

    ensure_account_scope(maple_api_session.account_scope(), &account_scope).map_err(|_| {
        "Maple API authentication belongs to a different signed-in account".to_string()
    })?;
    // No eager `validate_user` here: the scope check above binds the session
    // to this account locally, and every API call revalidates (and refreshes)
    // credentials anyway. Skipping the boot-time round trip keeps ACP and
    // desktop startup off the network's critical path.

    let mut agent_config = load_agent_config_inner(&state.host.paths, user_id)
        .map_err(|error| format!("Failed to load Agent config: {error}"))?;
    let request = request.unwrap_or(AgentStartRequest {
        project_root: None,
        model: None,
        mode: None,
    });

    let project_root = resolve_project_root(request.project_root.as_deref(), &agent_config)
        .map_err(|e| format!("Failed to resolve Agent Mode project root: {e}"))?;
    // Trust the root the desktop user explicitly launched under so its
    // project-local skills load without a separate prompt. Maple's Tauri app
    // asks first; this app treats launching in a directory as the choice.
    // A saved "do not trust" answer stays as it is.
    if project_trust_status(&agent_config, &project_root, true)
        .decision
        .is_none()
    {
        apply_project_trust(&mut agent_config, &project_root, true);
        save_agent_config_inner(&state.host.paths, user_id, &agent_config)
            .map_err(|error| error.to_string())?;
    }
    let model = request
        .model
        .unwrap_or_else(|| agent_config.default_model.clone());
    let mode = request
        .mode
        .unwrap_or_else(|| DEFAULT_GOOSE_MODE.to_string());
    parse_user_permission_mode(&mode)?;

    let config_dir = agent_config_dir(&state.host.paths, user_id).map_err(|e| e.to_string())?;
    let goose_path_root = config_dir.join("goose");
    fs::create_dir_all(goose_path_root.join("config"))
        .map_err(|e| format!("Failed to create Goose config dir: {e}"))?;
    let history_dir = account_history_dir(&state.host.paths, user_id)?;
    // Goose resolves the global AGENTS.md relative to this path root, not the
    // real home. Link the user's ~/.agents/AGENTS.md in so it is honored.
    link_global_agents_md(&goose_path_root);
    // This account-scoped PermissionManager is the one AgentManager actually
    // inspects. Force every Maple-routed tool through ActionRequired before it
    // is constructed so stale Goose AlwaysAllow entries cannot bypass Maple.
    let goose_config_dir = goose_path_root.join("config");
    reset_maple_owned_permission_file(&goose_config_dir.join("permission.yaml"))?;
    // Rewriting that file drops any tool entry Maple added, so the embedded
    // CUA tools must be pinned into it again before the next desktop run.
    #[cfg(embedded_cua)]
    cua::reset_pinned_tool_permissions();

    #[cfg(target_os = "macos")]
    let login_shell_search_paths = Some(
        state
            .login_shell_search_paths
            .get_or_init(macos_login_path::resolve_login_shell_search_paths)
            .await
            .as_slice(),
    );
    #[cfg(not(target_os = "macos"))]
    let login_shell_search_paths: Option<&[String]> = None;

    configure_embedded_goose(
        &agent_root_dir(&state.host.paths)
            .map_err(|e| e.to_string())?
            .join("goose-runtime"),
        &model,
        DEFAULT_GOOSE_MODE,
        login_shell_search_paths,
    )?;
    let session_manager = Arc::new(SessionManager::new(history_dir));
    // A crash can strand a zero-message ACP probe before its connection-owned
    // rollback runs. Such rows have never admitted user work; sweep them before
    // the runtime becomes visible, while renamed or messaged tasks survive.
    sweep_unprompted_acp_sessions(session_manager.as_ref()).await;
    let permission_manager = Arc::new(PermissionManager::new(goose_config_dir));
    let goose_config = GooseAgentConfig::new(
        Arc::clone(&session_manager),
        permission_manager,
        None,
        GOOSE_PERMISSION_ROUTING_MODE,
        // Maple schedules its title-specific provider request as a tracked,
        // first-turn-only task below. Enabling Goose's scheduler would add a
        // detached first-three-turn job and could retitle source-defined tasks.
        true,
        GoosePlatform::GooseDesktop,
    )
    .with_use_login_shell_path(true)
    .with_ordered_tool_calls(
        [
            "python_code".to_string(),
            "developer__python_code".to_string(),
        ],
        code_mode::MAX_PYTHON_BATCH_CALLS,
    );
    let agent_manager = Arc::new(
        AgentManager::new(goose_config, None)
            .await
            .map_err(|e| format!("Failed to create Goose agent manager: {e}"))?,
    );
    // Goose cannot reconstruct this account-scoped provider from its built-in
    // registry. Keep it available for new or uncached sessions; each turn still
    // reapplies the session's selected model below.
    agent_manager
        .set_default_provider(Arc::new(MapleProvider::new(Arc::clone(&maple_api_session))))
        .await;

    let runtime = AgentRuntime {
        agent_manager,
        session_manager,
        maple_api_session,
        active_runs: HashMap::new(),
        session_title_tasks: HashMap::new(),
        session_tool_contexts: HashMap::new(),
        permission_modes: Arc::new(Mutex::new(HashMap::new())),
        web_tool_state: Arc::new(WebToolState::default()),
        project_root: project_root.clone(),
        model: model.clone(),
        mode: mode.clone(),
        account_scope,
        lifetime: CancellationToken::new(),
    };
    let status = runtime.desktop_status();

    {
        let mut guard = state.inner.lock().await;
        *guard = Some(runtime);
    }

    // Starting a runtime is project use, not an explicit folder add. In particular, a
    // session-derived root may be absent from a legacy capped recent-roots file; registering it
    // here would incorrectly move that visible project to the top of the manual order.
    agent_config.default_project_root = Some(path_string(&project_root));
    agent_config.default_model = model;
    // The runtime is already up; a failed save must not abort the start, but
    // a silent failure hides why the next launch forgets this project.
    if let Err(error) = save_agent_config_inner(&state.host.paths, user_id, &agent_config) {
        log::warn!("Failed to save Agent config after runtime start: {error}");
    }

    emit_agent_event(
        &state.host.events,
        AgentServiceEvent::RuntimeStatus(status.clone()),
    );

    Ok(status)
}

impl AgentRuntimeHandle {
    pub async fn stop(&self) -> Result<AgentRuntimeStatus, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        stop_runtime_for_user(state, &self.user_id).await?;
        Ok(stopped_status())
    }

    pub async fn restart(
        &self,
        maple_api_session: Arc<MapleApiSession>,
        request: Option<AgentStartRequest>,
    ) -> Result<AgentRuntimeStatus, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        stop_runtime_for_user(state, &self.user_id).await?;
        start_runtime_for_user(state, maple_api_session, &self.user_id, request).await
    }
}

impl AgentRuntimeHandle {
    pub async fn load_config(&self) -> Result<AgentConfig, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        load_agent_config_inner(&state.host.paths, &self.user_id).map_err(|e| e.to_string())
    }

    pub async fn save_config(&self, config: AgentConfig) -> Result<(), String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        // MCP definitions have a dedicated mutation command. Preserve them here so
        // a delayed project/model preference save cannot overwrite newer servers.
        let mut next =
            load_agent_config_inner(&state.host.paths, &self.user_id).map_err(|e| e.to_string())?;
        next.default_project_root = config.default_project_root;
        next.default_model = config.default_model;
        save_agent_config_inner(&state.host.paths, &self.user_id, &next).map_err(|e| e.to_string())
    }

    pub async fn list_mcp_servers(&self) -> Result<Vec<AgentMcpServer>, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let config =
            load_agent_config_inner(&state.host.paths, &self.user_id).map_err(|e| e.to_string())?;
        normalize_mcp_servers(config.mcp_servers)
    }

    /// Maple-curated integrations discovered on this device.
    ///
    /// The external manifest probe runs without holding the runtime lifecycle
    /// lock. A generation check on each side prevents a stale account handle
    /// from publishing or mutating device-local state after logout.
    pub async fn list_integrations(&self) -> Result<Vec<AgentIntegration>, String> {
        self.verify_generation().await?;
        let detected = detect_integrations().await;
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        project_integrations(&state.host.paths, &self.user_id, &detected)
    }

    /// Change whether one curated integration is selected for newly created
    /// tasks. Existing task snapshots are intentionally untouched.
    pub async fn set_integration_enabled(
        &self,
        request: AgentSetIntegrationEnabledRequest,
    ) -> Result<Vec<AgentIntegration>, String> {
        require_known_integration(&request.id)?;
        self.verify_generation().await?;
        let detected = detect_integrations().await;
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        set_integration_default(&state.host.paths, &self.user_id, &request, &detected)
    }

    /// Finish the host-owned setup flow for a built-in integration by selecting
    /// it for new tasks once the OS reports both grants. The desktop UI invokes
    /// [`begin_integration_setup`] synchronously from the initiating user action
    /// before calling this method.
    pub async fn setup_integration(
        &self,
        request: AgentSetupIntegrationRequest,
    ) -> Result<Vec<AgentIntegration>, String> {
        require_known_integration(&request.id)?;
        self.verify_generation().await?;
        // A desktop that needs a compositor helper gets one here, before
        // detection runs, so the projection the caller receives already
        // reflects the install.
        #[cfg(embedded_cua)]
        {
            cua::install_desktop_helper().await?;
        }
        let detected = detect_integrations().await;
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        select_embedded_integration_backend(&state.host.paths, &self.user_id, &detected)
    }

    pub async fn save_mcp_servers(
        &self,
        servers: Vec<AgentMcpServer>,
    ) -> Result<Vec<AgentMcpServer>, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let servers = normalize_mcp_servers(servers)?;
        let mut config =
            load_agent_config_inner(&state.host.paths, &self.user_id).map_err(|e| e.to_string())?;
        validate_new_mcp_integration_collisions(&config.mcp_servers, &servers)?;
        config.mcp_servers = servers.clone();
        save_agent_config_inner(&state.host.paths, &self.user_id, &config)
            .map_err(|e| e.to_string())?;

        Ok(servers)
    }

    pub async fn list_recent_project_roots(&self) -> Result<Vec<RecentProjectRoot>, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        load_recent_project_roots_inner(&state.host.paths, &self.user_id).map_err(|e| e.to_string())
    }

    pub async fn save_recent_project_root(
        &self,
        path: String,
    ) -> Result<AgentProjectRootRegistration, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let project_root = normalize_project_root(Path::new(&path))?;
        let mut config = load_agent_config_inner(&state.host.paths, &self.user_id)
            .map_err(|error| error.to_string())?;
        let canonical_path = path_string(&project_root);
        let restoring = config
            .removed_project_roots
            .iter()
            .any(|removed| removed == &canonical_path);
        let roots = if restoring {
            restore_explicit_project_root_inner(&state.host.paths, &self.user_id, &project_root)
        } else {
            register_explicit_project_root_inner(&state.host.paths, &self.user_id, &project_root)
        }
        .map_err(|error| error.to_string())?;

        config
            .removed_project_roots
            .retain(|removed| removed != &canonical_path);
        config.default_project_root = Some(canonical_path.clone());
        save_agent_config_inner(&state.host.paths, &self.user_id, &config)
            .map_err(|error| error.to_string())?;
        // Clear the device-local tombstone last. If registration or ordinary
        // config persistence fails, the project remains hidden.
        if restoring {
            save_removed_project_roots_inner(
                &state.host.paths,
                &self.user_id,
                &config.removed_project_roots,
            )
            .map_err(|error| error.to_string())?;
        }

        Ok(AgentProjectRootRegistration {
            project_root: canonical_path,
            roots,
            config,
        })
    }

    pub async fn remove_project_root(
        &self,
        path: String,
        fallback_path: Option<String>,
    ) -> Result<AgentConfig, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;

        let path = path.trim().to_string();
        if !structurally_valid_project_root(&path) {
            return Err("Project path must be an absolute folder path".to_string());
        }
        let fallback_path = fallback_path
            .map(|fallback| fallback.trim().to_string())
            .filter(|fallback| !fallback.is_empty());
        if let Some(fallback) = fallback_path.as_deref()
            && (fallback == path || !structurally_valid_project_root(fallback))
        {
            return Err("Project fallback must be a different absolute folder path".to_string());
        }

        let (session_manager, active_session_ids) = {
            let runtime = state.inner.lock().await;
            match runtime.as_ref() {
                Some(current) => {
                    ensure_runtime_account(current, &self.account_scope)?;
                    (
                        Arc::clone(&current.session_manager),
                        current
                            .active_runs
                            .values()
                            .map(|run| run.session_id.clone())
                            .collect::<HashSet<_>>(),
                    )
                }
                None => (
                    account_session_manager(&state.host.paths, &self.user_id)?,
                    HashSet::new(),
                ),
            }
        };
        let sessions = session_manager
            .list_all_sessions()
            .await
            .map_err(|error| format!("Failed to inspect Agent tasks: {error}"))?;
        let session_roots = sessions
            .iter()
            .map(|session| (session.id.clone(), path_string(&session.working_dir)))
            .collect::<HashMap<_, _>>();
        if project_has_active_session_run(&session_roots, &active_session_ids, &path) {
            return Err("Stop the running agent before removing this project".to_string());
        }

        let mut config = load_agent_config_inner(&state.host.paths, &self.user_id)
            .map_err(|error| error.to_string())?;
        apply_project_root_removal(&mut config, &path, fallback_path.as_deref())?;
        {
            let runtime = state.inner.lock().await;
            if let Some(current) = runtime.as_ref() {
                for (session_id, installed) in &current.session_tool_contexts {
                    if session_roots.get(session_id) == Some(&path) {
                        installed.retire_python(
                            "Python state was retired when this project was removed",
                        );
                    }
                }
            }
        }
        // The tombstone is the only persistent removal state. Saving the fallback
        // into roaming config would let this device's removal alter another
        // device. Runtime/UI use the fallback immediately; startup filters the
        // stale hidden default before selecting any project.
        save_removed_project_roots_inner(
            &state.host.paths,
            &self.user_id,
            &config.removed_project_roots,
        )
        .map_err(|error| error.to_string())?;

        let mut runtime = state.inner.lock().await;
        if let Some(current) = runtime.as_mut() {
            update_runtime_project_root_after_removal(
                &mut current.project_root,
                &path,
                fallback_path.as_deref(),
            );
        }

        Ok(config)
    }

    pub async fn get_project_trust(&self, path: String) -> Result<AgentProjectTrustStatus, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let requested = Path::new(path.trim());
        if !requested.is_dir() {
            return Ok(AgentProjectTrustStatus {
                path: path_string(requested),
                decision: None,
                available: false,
                protected_features: Vec::new(),
            });
        }
        let project_root = normalize_project_root(requested)?;
        let config =
            load_agent_config_inner(&state.host.paths, &self.user_id).map_err(|e| e.to_string())?;
        Ok(project_trust_status(&config, &project_root, true))
    }

    pub async fn set_project_trust(
        &self,
        path: String,
        trusted: bool,
    ) -> Result<AgentProjectTrustStatus, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let project_root = normalize_project_root(Path::new(&path))?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        let (agent_manager, session_manager, active_session_ids, leased_session_ids) = {
            let runtime = state.inner.lock().await;
            let Some(current) = runtime.as_ref() else {
                let mut config = load_agent_config_inner(&state.host.paths, &self.user_id)
                    .map_err(|error| error.to_string())?;
                apply_project_trust(&mut config, &project_root, trusted);
                save_agent_config_inner(&state.host.paths, &self.user_id, &config)
                    .map_err(|error| error.to_string())?;
                return Ok(project_trust_status(&config, &project_root, true));
            };
            ensure_runtime_account(current, &self.account_scope)?;
            (
                Arc::clone(&current.agent_manager),
                Arc::clone(&current.session_manager),
                current
                    .active_runs
                    .values()
                    .map(|run| run.session_id.clone())
                    .collect::<HashSet<_>>(),
                current
                    .session_tool_contexts
                    .iter()
                    .filter(|(_, installed)| installed.owner == AgentToolContextOwner::Leased)
                    .map(|(session_id, _)| session_id.clone())
                    .collect::<HashSet<_>>(),
            )
        };
        let project_session_ids = session_manager
            .list_all_sessions()
            .await
            .map_err(|error| format!("Failed to inspect Agent tasks: {error}"))?
            .into_iter()
            .filter(|session| session.working_dir == project_root)
            .map(|session| session.id)
            .collect::<Vec<_>>();
        if project_session_ids
            .iter()
            .any(|session_id| active_session_ids.contains(session_id))
        {
            return Err(
                "Stop running agents in this project before changing project trust".to_string(),
            );
        }
        if project_session_ids
            .iter()
            .any(|session_id| leased_session_ids.contains(session_id))
        {
            return Err(
                "Close externally controlled Agent tasks in this project before changing project trust"
                    .to_string(),
            );
        }
        let mut config =
            load_agent_config_inner(&state.host.paths, &self.user_id).map_err(|e| e.to_string())?;
        apply_project_trust(&mut config, &project_root, trusted);
        save_agent_config_inner(&state.host.paths, &self.user_id, &config)
            .map_err(|e| e.to_string())?;
        for session_id in project_session_ids {
            agent_manager
                .remove_session_if_loaded(&session_id)
                .await
                .map_err(|error| {
                    format!(
                        "Project trust was saved, but Maple could not refresh Agent task {session_id}: {error}"
                    )
                })?;
        }
        Ok(project_trust_status(&config, &project_root, true))
    }

    /// Save a project trust decision for an exact externally leased task and
    /// evict its cached Goose agent so the next prompt rebuilds project-scoped
    /// capabilities from that decision. The lease-owned tool context remains
    /// installed; only the derived Agent/tool catalog is refreshed.
    pub(crate) async fn set_project_trust_for_surface_session(
        &self,
        path: String,
        trusted: bool,
        access: AgentToolContextAccess,
    ) -> Result<AgentProjectTrustStatus, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let project_root = normalize_project_root(Path::new(&path))?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        let (agent_manager, session_manager) = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, &self.account_scope)?;
            if has_active_session_run(&current.active_runs, access.session_id.as_ref()) {
                return Err(
                    "Project trust cannot change while this Agent task is running".to_string(),
                );
            }
            if !matching_leased_tool_context(
                &current.session_tool_contexts,
                access.session_id.as_ref(),
                access.installation_id,
                &access.context,
            ) {
                return Err(AGENT_TOOL_CONTEXT_INACTIVE_ERROR.to_string());
            }
            (
                Arc::clone(&current.agent_manager),
                Arc::clone(&current.session_manager),
            )
        };
        let session = session_manager
            .get_session(access.session_id.as_ref(), true)
            .await
            .map_err(|error| format!("Failed to load Agent task: {error}"))?;
        if session.working_dir != project_root {
            return Err("Project trust path does not match this Agent task".to_string());
        }
        let mut config =
            load_agent_config_inner(&state.host.paths, &self.user_id).map_err(|e| e.to_string())?;
        apply_project_trust(&mut config, &project_root, trusted);
        save_agent_config_inner(&state.host.paths, &self.user_id, &config)
            .map_err(|e| e.to_string())?;
        agent_manager
            .remove_session_if_loaded(access.session_id.as_ref())
            .await
            .map_err(|error| {
                format!(
                    "Project trust was saved, but Maple could not refresh this Agent task: {error}"
                )
            })?;
        Ok(project_trust_status(&config, &project_root, true))
    }

    pub async fn save_project_root_order(
        &self,
        paths: Vec<String>,
    ) -> Result<Vec<RecentProjectRoot>, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        save_project_root_order_inner(&state.host.paths, &self.user_id, paths)
            .map_err(|e| e.to_string())
    }
}

impl AgentRuntimeHandle {
    pub async fn create_session(
        &self,
        request: Option<AgentCreateSessionRequest>,
    ) -> Result<AgentSessionDetail, String> {
        Ok(self
            .create_session_with_tool_context(request, None, AgentHostEventPolicy::Publish)
            .await?
            .detail)
    }

    pub(crate) async fn create_session_with_tool_context(
        &self,
        request: Option<AgentCreateSessionRequest>,
        tool_context: Option<AgentToolContextSpec>,
        host_events: AgentHostEventPolicy,
    ) -> Result<CreatedAgentSession, String> {
        self.create_session_with_surface_context(
            request,
            tool_context,
            Vec::new(),
            CancellationToken::new(),
            host_events,
        )
        .await
    }

    pub(crate) async fn create_session_with_surface_context(
        &self,
        request: Option<AgentCreateSessionRequest>,
        tool_context: Option<AgentToolContextSpec>,
        transient_mcp_servers: Vec<AgentTransientMcpServer>,
        setup_cancel: CancellationToken,
        host_events: AgentHostEventPolicy,
    ) -> Result<CreatedAgentSession, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        let has_external_tool_context = tool_context.is_some();
        if !has_external_tool_context && !transient_mcp_servers.is_empty() {
            return Err("Transient MCP servers require a leased Agent surface".to_string());
        }
        let transient_mcp_servers = normalize_transient_mcp_servers(transient_mcp_servers)?;
        let tool_context = SharedAgentToolContext::new(
            tool_context.unwrap_or_else(|| state.host.default_tool_context.clone()),
        );
        let mut tool_context_installation =
            PendingAgentToolContextInstallation::new(tool_context.clone());
        let runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let request = request.unwrap_or(AgentCreateSessionRequest {
            project_root: None,
            title: None,
            model: None,
            context_limit: None,
            mode: None,
            mcp_server_names: None,
            system_prompt: None,
        });
        let (
            agent_manager,
            session_manager,
            maple_api_session,
            permission_modes,
            web_tool_state,
            runtime_lifetime,
            runtime_project_root,
            runtime_model,
            runtime_mode,
        ) = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, account_scope)?;
            (
                Arc::clone(&current.agent_manager),
                Arc::clone(&current.session_manager),
                Arc::clone(&current.maple_api_session),
                Arc::clone(&current.permission_modes),
                Arc::clone(&current.web_tool_state),
                // Stop and logout cancel this token, which ends a stalled MCP
                // startup that no run entry covers yet.
                current.lifetime.clone(),
                current.project_root.clone(),
                current.model.clone(),
                current.mode.clone(),
            )
        };

        let config = load_agent_config_inner(&state.host.paths, user_id)
            .map_err(|error| error.to_string())?;
        let root = match request.project_root.as_deref() {
            Some(path) if !path.trim().is_empty() => normalize_project_root(Path::new(path))?,
            _ => runtime_project_root,
        };
        ensure_session_project_root_is_visible(&root, &config.removed_project_roots)?;
        let title = request
            .title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_AGENT_SESSION_TITLE.to_string());
        let mode = request.mode.unwrap_or(runtime_mode);
        let permission_mode = parse_user_permission_mode(&mode)?;
        if has_external_tool_context && !is_caller_mediated_mode(permission_mode) {
            return Err(
                "External Agent surfaces support only caller-mediated permission modes".to_string(),
            );
        }
        let model = request.model.unwrap_or(runtime_model);
        let session_type = if has_external_tool_context {
            SessionType::Acp
        } else {
            SessionType::User
        };
        let stored_integrations = stored_integrations_for_read(&state.host.paths, user_id);
        let cua_state = cua_state_for_new_session(
            &stored_integrations,
            request.mcp_server_names.as_deref(),
            session_type == SessionType::User,
        )?;
        let requested_mcp_names = requested_mcp_names_without_embedded_cua(
            request.mcp_server_names.as_deref(),
            cua_state,
        );
        let configured_mcp = effective_mcp_servers(&stored_integrations, config.mcp_servers)?;
        let selected_mcp = select_mcp_servers(&configured_mcp, requested_mcp_names.as_deref())?;
        let selected_extensions = selected_mcp
            .iter()
            .map(mcp_server_to_extension)
            .collect::<Result<Vec<_>, _>>()?;
        let selected_extension_keys = mcp_extension_keys(&selected_extensions);
        ensure_extension_sets_do_not_conflict(&selected_extensions, &transient_mcp_servers)?;
        let session = session_manager
            .create_session(root.clone(), title, session_type, permission_mode)
            .await
            .map_err(|e| format!("Failed to create Agent task: {e}"))?;
        let session = seed_empty_extension_state(&session_manager, session, cua_state).await?;
        let expected_provisional_session = session.clone();
        // The Goose agent is built below; the caller's prompt must be
        // stored first so the fresh agent picks it up.
        store_session_system_prompt(&session.id, request.system_prompt.clone());
        let installation_id = next_tool_context_installation_id();
        let tool_context_access = AgentToolContextAccess {
            account_scope: Arc::clone(&self.account_scope),
            session_id: Arc::from(session.id.as_str()),
            installation_id,
            context: tool_context.clone(),
        };
        // Fence the newly durable row before releasing Maple's lifecycle
        // lock. Desktop operations now see an authoritative lease rather than
        // a runnable/deletable task during the caller-controlled MCP handshake.
        let reservation_error = {
            let mut runtime = state.inner.lock().await;
            match runtime.as_mut() {
                Some(current) if current.account_scope == account_scope => {
                    match current.session_tool_contexts.entry(session.id.clone()) {
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(InstalledAgentToolContext {
                                installation_id,
                                context: tool_context.clone(),
                                owner: AgentToolContextOwner::Leased,
                                python: None,
                            });
                            None
                        }
                        std::collections::hash_map::Entry::Occupied(_) => {
                            Some("Agent task context already existed during creation".to_string())
                        }
                    }
                }
                _ => Some("Agent runtime changed during session creation".to_string()),
            }
        };
        if let Some(error) = reservation_error {
            tool_context.revoke();
            if let Err(cleanup_error) = session_manager.delete_session(&session.id).await {
                log::warn!(
                    "Failed to remove unreserved Agent task {} after setup error: {cleanup_error}",
                    session.id
                );
            }
            return Err(error);
        }
        tool_context_installation.arm_created_cleanup(
            state.clone(),
            tool_context_access.clone(),
            Arc::clone(&agent_manager),
            Arc::clone(&session_manager),
            expected_provisional_session.clone(),
        );
        // A caller-controlled MCP endpoint must not hold Maple's global
        // runtime lifecycle fence while it initializes.
        drop(runtime_lifecycle_guard);

        let setup_result: Result<(Session, Vec<AgentMcpConnectionError>), String> =
            run_external_surface_setup(
                &setup_cancel,
                &tool_context,
                "Agent surface closed during session setup",
                async {
                    install_transient_mcp_router(
                        &tool_context,
                        &session,
                        transient_mcp_servers,
                        &setup_cancel,
                    )
                    .await?;
                    // The fence is released again around MCP startup below, so
                    // it is held in a rebindable slot rather than a plain guard.
                    let mut setup_runtime_guard = Some(state.runtime_lifecycle.lock().await);
                    self.verify_generation().await?;
                    self.ensure_accepting_new_work()?;
                    {
                        let runtime = state.inner.lock().await;
                        let current = runtime
                            .as_ref()
                            .ok_or_else(|| "Agent runtime is not running".to_string())?;
                        ensure_runtime_account(current, account_scope)?;
                        if !Arc::ptr_eq(&current.agent_manager, &agent_manager)
                            || !Arc::ptr_eq(&current.session_manager, &session_manager)
                            || !Arc::ptr_eq(&current.maple_api_session, &maple_api_session)
                        {
                            return Err("Agent runtime changed during session setup".to_string());
                        }
                        if has_active_session_run(&current.active_runs, &session.id)
                            || !matching_leased_tool_context(
                                &current.session_tool_contexts,
                                &session.id,
                                installation_id,
                                &tool_context,
                            )
                        {
                            return Err(
                                "Agent task ownership changed during session setup".to_string()
                            );
                        }
                    }
                    let session = session_manager
                        .get_session(&session.id, true)
                        .await
                        .map_err(|error| format!("Failed to reload Agent task: {error}"))?;
                    if session.working_dir != root || session.session_type != session_type {
                        return Err("Agent task changed during session setup".to_string());
                    }
                    let (agent, mut mcp_errors) = configure_session_agent(
                        AgentSkillsScope {
                            paths: &state.host.paths,
                            user_id,
                        },
                        &agent_manager,
                        &session_manager,
                        &maple_api_session,
                        &state.host.harness_instructions(),
                        SessionAgentConfiguration {
                            web_tool_state: &web_tool_state,
                            session: &session,
                            model: &model,
                            context_limit: request.context_limit,
                            mode: &mode,
                            primary_model_supports_vision: false,
                            tool_context: &tool_context,
                            python_binding: None,
                            allow_embedded_cua: !has_external_tool_context,
                        },
                    )
                    .await?;
                    if !selected_extensions.is_empty() {
                        // Resolve every fallible part of restoring Maple's transient Skills client before
                        // Goose persists the MCP mutation. Reattachment after this point is infallible.
                        let skills_client = prepare_transient_skills_client(
                            &state.host.paths,
                            user_id,
                            &agent,
                            &session,
                        )?;
                        // Starting the selected MCP servers spawns stdio
                        // processes and performs remote handshakes, each
                        // bounded only by a per-server timeout that defaults to
                        // DEFAULT_MCP_TIMEOUT_SECONDS. Release the runtime
                        // lifecycle fence across it so Stop, status, and task
                        // switching stay responsive, and end the wait when the
                        // runtime is stopped rather than sitting out the timeout.
                        drop(setup_runtime_guard.take());
                        detach_transient_skills_client(&agent).await;
                        let extension_result = tokio::select! {
                            biased;
                            _ = runtime_lifetime.cancelled() => None,
                            result = agent.add_extensions_bulk(selected_extensions, &session.id) => {
                                Some(result)
                            }
                        };
                        // Maple's Skills client is reattached on every path:
                        // the caller's cleanup still walks this Agent.
                        attach_prepared_skills_client(&agent, skills_client).await;
                        setup_runtime_guard = Some(state.runtime_lifecycle.lock().await);
                        let Some(extension_result) = extension_result else {
                            return Err(MCP_STARTUP_CANCELLED_ERROR.to_string());
                        };
                        // The runtime can be stopped, replaced, or switched to
                        // another account while the fence is down. Bail before
                        // this session is published into an Agent whose manager
                        // a concurrent stop is already tearing down.
                        self.verify_generation().await?;
                        self.ensure_accepting_new_work()?;
                        {
                            let runtime = state.inner.lock().await;
                            let current = runtime
                                .as_ref()
                                .ok_or_else(|| "Agent runtime is not running".to_string())?;
                            ensure_runtime_account(current, account_scope)?;
                            if !Arc::ptr_eq(&current.agent_manager, &agent_manager)
                                || !Arc::ptr_eq(&current.session_manager, &session_manager)
                                || !Arc::ptr_eq(&current.maple_api_session, &maple_api_session)
                            {
                                return Err(
                                    "Agent runtime changed during session setup".to_string()
                                );
                            }
                        }
                        match extension_result {
                            Ok(results) => mcp_errors
                                .extend(mcp_connection_errors(results, &selected_extension_keys)),
                            Err(error) => mcp_errors.push(AgentMcpConnectionError {
                                name: "MCP servers".to_string(),
                                error: error.to_string(),
                            }),
                        }
                    }
                    {
                        let mut runtime = state.inner.lock().await;
                        let current = runtime
                            .as_mut()
                            .ok_or_else(|| "Agent runtime is not running".to_string())?;
                        ensure_runtime_account(current, account_scope)?;
                        if has_active_session_run(&current.active_runs, &session.id)
                            || !matching_leased_tool_context(
                                &current.session_tool_contexts,
                                &session.id,
                                installation_id,
                                &tool_context,
                            )
                        {
                            return Err(
                                "Agent task ownership changed during session setup".to_string()
                            );
                        }
                        let mut modes = permission_modes.lock().await;
                        current
                            .session_tool_contexts
                            .get_mut(&session.id)
                            .expect("the exact provisional context was just verified")
                            .owner = if has_external_tool_context {
                            AgentToolContextOwner::Leased
                        } else {
                            AgentToolContextOwner::Maple
                        };
                        modes.insert(session.id.clone(), permission_mode);
                    }
                    let session = session_manager
                        .get_session(&session.id, true)
                        .await
                        .map_err(|error| {
                            format!("Failed to load configured Agent task: {error}")
                        })?;
                    if setup_cancel.is_cancelled() {
                        return Err("Agent surface closed during session setup".to_string());
                    }
                    tool_context_installation.commit();
                    // Publishing the created session is the last fenced step.
                    drop(setup_runtime_guard);
                    Ok((session, mcp_errors))
                },
            )
            .await;
        let (session, mcp_errors) = match setup_result {
            Ok(result) => result,
            Err(error) => {
                tool_context.revoke();
                let cleanup = cleanup_provisional_created_session(
                    state.clone(),
                    tool_context_access.clone(),
                    Arc::clone(&agent_manager),
                    Arc::clone(&session_manager),
                    expected_provisional_session,
                );
                let cleaned = tokio::select! {
                    biased;
                    _ = setup_cancel.cancelled() => false,
                    _ = cleanup => true,
                };
                if cleaned {
                    tool_context_installation.commit();
                }
                return Err(error);
            }
        };
        let summary = session_summary(&session);
        // Session creation must not mutate project order. Only explicit folder-add and reorder
        // commands may change the persisted project list.
        let detail = AgentSessionDetail {
            session: summary.clone(),
            timeline: Vec::new(),
            mcp_errors,
            queue: empty_desktop_queue_snapshot(),
        };
        let tool_context_lease = has_external_tool_context.then(|| AgentToolContextLease {
            service: state.clone(),
            access: tool_context_access,
            created_cleanup: Some(CreatedAgentSessionCleanup {
                agent_manager: Arc::clone(&agent_manager),
                session_manager: Arc::clone(&session_manager),
                expected: expected_provisional_session.clone(),
            }),
            discard_created_on_drop: false,
            cleanup_started: false,
        });

        // The context was published only after every fallible setup await and
        // committed without yielding while the lifecycle fence was held.
        if host_events.publishes() {
            emit_agent_event(
                &state.host.events,
                AgentServiceEvent::SessionCreated(summary),
            );
        }
        Ok(CreatedAgentSession {
            detail,
            tool_context_lease,
        })
    }

    /// Attach an external Agent surface to an existing durable task.
    ///
    /// This is intentionally distinct from the Desktop-facing `load_session`:
    /// it atomically acquires the task's external tool/MCP lease, rejects live
    /// or already-leased tasks, and returns persisted history without Desktop
    /// overlays or actionable permission routing.
    pub(crate) async fn attach_session_with_surface_context(
        &self,
        session_id: String,
        project_root: String,
        tool_context: AgentToolContextSpec,
        transient_mcp_servers: Vec<AgentTransientMcpServer>,
        setup_cancel: CancellationToken,
    ) -> Result<CreatedAgentSession, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        let session_id = session_id.trim().to_string();
        if session_id.is_empty() {
            return Err("Agent task ID cannot be empty".to_string());
        }
        let tool_context = SharedAgentToolContext::new(tool_context);
        let mut tool_context_installation =
            PendingAgentToolContextInstallation::new(tool_context.clone());
        let transient_mcp_servers = normalize_transient_mcp_servers(transient_mcp_servers)?;
        let runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let root = normalize_project_root(Path::new(&project_root))?;
        let config = load_agent_config_inner(&state.host.paths, user_id)
            .map_err(|error| error.to_string())?;
        ensure_session_project_root_is_visible(&root, &config.removed_project_roots)?;
        let session_lifecycle_guard = state.session_lifecycle.lock().await;
        let (
            agent_manager,
            session_manager,
            maple_api_session,
            permission_modes,
            web_tool_state,
            runtime_model,
        ) = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, account_scope)?;
            if has_active_session_run(&current.active_runs, &session_id) {
                return Err("This Agent task is already running".to_string());
            }
            if current
                .session_tool_contexts
                .get(&session_id)
                .is_some_and(|installed| installed.owner == AgentToolContextOwner::Leased)
            {
                return Err("This Agent task is controlled by another Agent surface".to_string());
            }
            (
                Arc::clone(&current.agent_manager),
                Arc::clone(&current.session_manager),
                Arc::clone(&current.maple_api_session),
                Arc::clone(&current.permission_modes),
                Arc::clone(&current.web_tool_state),
                current.model.clone(),
            )
        };
        let session = session_manager
            .get_session(session_id.trim(), true)
            .await
            .map_err(|error| format!("Failed to load Agent task: {error}"))?;
        if session.working_dir != root {
            return Err("ACP session cwd does not match the persisted Agent task".to_string());
        }
        ensure_external_surface_loadable_session(&session)?;
        let transient_keys = transient_mcp_servers
            .iter()
            .map(|server| goose::config::extensions::name_to_key(&server.name))
            .collect::<HashSet<_>>();
        if let Some(conflict) = session_mcp_extension_keys(&session)
            .into_iter()
            .find(|key| transient_keys.contains(key))
        {
            return Err(format!(
                "Transient MCP server '{conflict}' conflicts with this task's persisted MCP configuration"
            ));
        }
        let installation_id = next_tool_context_installation_id();
        let tool_context_access = AgentToolContextAccess {
            account_scope: Arc::clone(&self.account_scope),
            session_id: Arc::from(session_id.as_str()),
            installation_id,
            context: tool_context.clone(),
        };
        {
            let mut runtime = state.inner.lock().await;
            let current = runtime
                .as_mut()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, account_scope)?;
            if has_active_session_run(&current.active_runs, &session_id)
                || current
                    .session_tool_contexts
                    .get(&session_id)
                    .is_some_and(|installed| installed.owner == AgentToolContextOwner::Leased)
            {
                return Err("This Agent task became active while ACP was attaching".to_string());
            }
            if let Some(replaced) = current.session_tool_contexts.insert(
                session_id.clone(),
                InstalledAgentToolContext {
                    installation_id,
                    context: tool_context.clone(),
                    owner: AgentToolContextOwner::Leased,
                    python: None,
                },
            ) {
                replaced.revoke("Python task ownership changed");
            }
        }
        tool_context_installation.arm_lease_cleanup(state.clone(), tool_context_access.clone());
        // The endpoint can be caller-controlled. Release both global fences
        // while its bounded, cancellation-linked MCP handshake runs. The
        // exact provisional lease remains authoritative for Desktop callers.
        drop(session_lifecycle_guard);
        drop(runtime_lifecycle_guard);
        let setup_result: Result<AgentSessionDetail, String> = run_external_surface_setup(
            &setup_cancel,
            &tool_context,
            "ACP session closed while attaching",
            async {
                install_transient_mcp_router(
                    &tool_context,
                    &session,
                    transient_mcp_servers,
                    &setup_cancel,
                )
                .await?;
                let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
                self.verify_generation().await?;
                self.ensure_accepting_new_work()?;
                let _session_lifecycle_guard = state.session_lifecycle.lock().await;
                {
                    let runtime = state.inner.lock().await;
                    let current = runtime
                        .as_ref()
                        .ok_or_else(|| "Agent runtime is not running".to_string())?;
                    ensure_runtime_account(current, account_scope)?;
                    if !Arc::ptr_eq(&current.agent_manager, &agent_manager)
                        || !Arc::ptr_eq(&current.session_manager, &session_manager)
                        || !Arc::ptr_eq(&current.maple_api_session, &maple_api_session)
                    {
                        return Err("Agent runtime changed while ACP was attaching".to_string());
                    }
                    if has_active_session_run(&current.active_runs, &session_id)
                        || !matching_leased_tool_context(
                            &current.session_tool_contexts,
                            &session_id,
                            installation_id,
                            &tool_context,
                        )
                    {
                        return Err(
                            "Agent task ownership changed while ACP was attaching".to_string()
                        );
                    }
                }
                let session = session_manager
                    .get_session(&session_id, true)
                    .await
                    .map_err(|error| format!("Failed to reload Agent task: {error}"))?;
                if session.working_dir != root {
                    return Err(
                        "ACP session cwd does not match the persisted Agent task".to_string()
                    );
                }
                ensure_external_surface_loadable_session(&session)?;
                if let Some(conflict) = session_mcp_extension_keys(&session)
                    .into_iter()
                    .find(|key| transient_keys.contains(key))
                {
                    return Err(format!(
                        "Transient MCP server '{conflict}' conflicts with this task's persisted MCP configuration"
                    ));
                }
                let model = session
                    .model_config
                    .as_ref()
                    .map(|model| model.model_name.clone())
                    .unwrap_or(runtime_model);
                let mode = session.goose_mode.to_string();
                let (agent, mcp_errors) = configure_session_agent(
                    AgentSkillsScope {
                        paths: &state.host.paths,
                        user_id,
                    },
                    &agent_manager,
                    &session_manager,
                    &maple_api_session,
                    &state.host.harness_instructions(),
                    SessionAgentConfiguration {
                        web_tool_state: &web_tool_state,
                        session: &session,
                        model: &model,
                        context_limit: session
                            .model_config
                            .as_ref()
                            .and_then(|model| model.context_limit),
                        mode: &mode,
                        primary_model_supports_vision: false,
                        tool_context: &tool_context,
                        python_binding: None,
                        allow_embedded_cua: false,
                    },
                )
                .await?;
                drop(agent);

                let session = session_manager
                    .get_session(&session_id, true)
                    .await
                    .map_err(|error| format!("Failed to load configured Agent task: {error}"))?;
                let detail = AgentSessionDetail {
                    session: session_summary(&session),
                    timeline: session
                        .conversation
                        .as_ref()
                        .map(conversation_to_timeline_items)
                        .unwrap_or_default(),
                    mcp_errors,
                    queue: empty_desktop_queue_snapshot(),
                };
                {
                    let mut runtime = state.inner.lock().await;
                    let current = runtime
                        .as_mut()
                        .ok_or_else(|| "Agent runtime is not running".to_string())?;
                    ensure_runtime_account(current, account_scope)?;
                    if has_active_session_run(&current.active_runs, &detail.session.id)
                        || !matching_leased_tool_context(
                            &current.session_tool_contexts,
                            &detail.session.id,
                            installation_id,
                            &tool_context,
                        )
                    {
                        return Err(
                            "Agent task ownership changed while ACP was attaching".to_string()
                        );
                    }
                    permission_modes
                        .lock()
                        .await
                        .insert(detail.session.id.clone(), session.goose_mode);
                }
                if setup_cancel.is_cancelled() {
                    return Err("ACP session closed while attaching".to_string());
                }
                tool_context_installation.commit();
                Ok(detail)
            },
        )
        .await;
        let detail = match setup_result {
            Ok(detail) => detail,
            Err(error) => {
                tool_context.revoke();
                let cleanup =
                    release_tool_context_lease(state.clone(), tool_context_access.clone());
                let cleaned = tokio::select! {
                    biased;
                    _ = setup_cancel.cancelled() => false,
                    _ = cleanup => true,
                };
                if cleaned {
                    tool_context_installation.commit();
                }
                return Err(error);
            }
        };
        let lease = AgentToolContextLease {
            service: state.clone(),
            access: tool_context_access,
            created_cleanup: None,
            discard_created_on_drop: false,
            cleanup_started: false,
        };
        Ok(CreatedAgentSession {
            detail,
            tool_context_lease: Some(lease),
        })
    }

    pub async fn list_sessions(
        &self,
        project_root: Option<String>,
    ) -> Result<Vec<AgentSessionSummary>, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let (session_manager, filter_root) = {
            let runtime = state.inner.lock().await;
            let session_manager = match runtime.as_ref() {
                Some(current) => {
                    ensure_runtime_account(current, account_scope)?;
                    Arc::clone(&current.session_manager)
                }
                None => account_session_manager(&state.host.paths, user_id)?,
            };
            let filter_root = project_root
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(|path| normalize_project_root(Path::new(path)))
                .transpose()?;
            (session_manager, filter_root)
        };

        let mut sessions = session_manager
            .list_all_sessions()
            .await
            .map_err(|e| format!("Failed to list Agent tasks: {e}"))?
            .into_iter()
            .filter(|session| {
                is_user_facing_session(session)
                    && !is_unprompted_acp_session(session)
                    && if let Some(root) = filter_root.as_ref() {
                        session.working_dir == *root
                    } else {
                        true
                    }
            })
            .map(|session| session_summary(&session))
            .collect::<Vec<_>>();
        sort_sessions_newest_first(&mut sessions);
        Ok(sessions)
    }

    pub async fn available_model_ids(&self) -> Result<Vec<String>, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let (maple_api_session, default_model) = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, self.account_scope.as_ref())?;
            (
                Arc::clone(&current.maple_api_session),
                current.model.clone(),
            )
        };
        drop(_runtime_lifecycle_guard);

        let mut models = match cached_model_catalog_fetch(
            &state.host.paths,
            &self.user_id,
            &maple_api_session,
            self.generation,
        )
        .await
        {
            CatalogFetch::Fresh(models) | CatalogFetch::Stale(models) => models,
            CatalogFetch::Failed => Vec::new(),
        };
        // A catalog request can outlive logout or runtime replacement. Recheck
        // the exact account generation and transport before publishing a
        // response that may have come from the former signed-in account.
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, self.account_scope.as_ref())?;
            if !Arc::ptr_eq(&current.maple_api_session, &maple_api_session) {
                return Err("Agent runtime changed while refreshing models".to_string());
            }
        }
        models.retain(|model| selectable_agent_model_id(model));
        models.retain(|model| !model.trim().is_empty());
        let mut seen = HashSet::new();
        models.retain(|model| seen.insert(model.clone()));
        models.retain(|model| model != &default_model);
        models.insert(0, default_model.clone());
        Ok(models)
    }

    /// Resolve the context limit (tokens) for a model id against the live
    /// model catalog. Follows Maple's frontend rules: an alias resolves to
    /// its target model; `context_window` and `max_context_tokens` must be
    /// present-and-equal when both appear, else the one present value wins;
    /// absent or disagreeing metadata means unknown.
    pub async fn context_limit_for_model(&self, model_id: &str) -> Result<Option<i64>, String> {
        if model_id.trim().is_empty() {
            return Ok(None);
        }
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let maple_api_session = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, self.account_scope.as_ref())?;
            Arc::clone(&current.maple_api_session)
        };
        drop(_runtime_lifecycle_guard);

        let catalog = match maple_api_session.model_catalog().await {
            Ok(catalog) => catalog,
            Err(error) => {
                log::warn!("Failed to fetch Maple Agent model catalog: {error}");
                return Ok(None);
            }
        };

        let mut concrete_id = model_id.to_string();
        for alias in &catalog.aliases {
            if alias.id == model_id {
                if let Some(target) = alias.target_model.as_deref()
                    && !target.trim().is_empty()
                {
                    concrete_id = target.to_string();
                }
                break;
            }
        }
        let model = catalog.data.iter().find(|model| model.id == concrete_id);
        let Some(model) = model else {
            return Ok(None);
        };
        Ok(reconcile_context_limit(
            model.context_window,
            model.max_context_tokens,
        ))
    }

    /// The live Maple API session for a one-off request outside a run.
    async fn maple_api_session(&self) -> Result<Arc<MapleApiSession>, String> {
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let runtime = state.inner.lock().await;
        let current = runtime
            .as_ref()
            .ok_or_else(|| "Agent runtime is not running".to_string())?;
        ensure_runtime_account(current, self.account_scope.as_ref())?;
        Ok(Arc::clone(&current.maple_api_session))
    }

    /// Which voice endpoints the account's model list offers. Follows the
    /// Maple web app: a `whisper` model enables transcription, a `tts`
    /// model enables speech.
    pub async fn audio_capabilities(&self) -> Result<AudioCapabilities, String> {
        let models = self.maple_api_session().await?.model_ids().await?;
        let has = |marker: &str| {
            models
                .iter()
                .any(|model| model.to_ascii_lowercase().contains(marker))
        };
        Ok(AudioCapabilities {
            transcription: has("whisper"),
            speech: has("tts"),
        })
    }

    /// Turn `text` into WAV audio with Maple's text-to-speech model.
    pub async fn synthesize_speech(
        &self,
        text: &str,
        voice: &str,
        speed: f32,
    ) -> Result<Vec<u8>, String> {
        let body = serde_json::to_vec(&serde_json::json!({
            "input": text,
            "model": TTS_MODEL,
            "voice": voice,
            "speed": speed,
        }))
        .map_err(|error| error.to_string())?;
        let started = std::time::Instant::now();
        let response = self
            .maple_api_session()
            .await?
            .audio_request("/v1/audio/speech", "audio/wav", body)
            .await?;
        log::info!(
            "text-to-speech: HTTP {} ({}, {} bytes) after {:?} for {} chars",
            response.status,
            response.content_type,
            response.body.len(),
            started.elapsed(),
            text.chars().count()
        );
        if let Some(message) = audio_error_message(&response) {
            log::warn!("text-to-speech failed: {message}");
            return Err(message);
        }
        speech_audio_from_body(response.body)
    }

    /// Transcribe WAV audio with Maple's Whisper model.
    pub async fn transcribe_audio(&self, wav: Vec<u8>) -> Result<String, String> {
        use base64::Engine;

        let body = serde_json::to_vec(&serde_json::json!({
            "file": base64::engine::general_purpose::STANDARD.encode(&wav),
            "filename": "recording.wav",
            "content_type": "audio/wav",
            "model": TRANSCRIPTION_MODEL,
        }))
        .map_err(|error| error.to_string())?;
        let started = std::time::Instant::now();
        let response = self
            .maple_api_session()
            .await?
            .audio_request("/v1/audio/transcriptions", "application/json", body)
            .await?;
        log::info!(
            "transcription: HTTP {} ({} bytes) after {:?}",
            response.status,
            response.body.len(),
            started.elapsed()
        );
        if let Some(message) = audio_error_message(&response) {
            log::warn!("transcription failed: {message}");
            return Err(message);
        }
        let parsed: serde_json::Value = serde_json::from_slice(&response.body)
            .map_err(|error| format!("Transcription returned invalid JSON: {error}"))?;
        Ok(parsed
            .get("text")
            .and_then(|text| text.as_str())
            .unwrap_or_default()
            .trim()
            .to_string())
    }

    /// Whether the catalog marks a model (or the target of an alias) as
    /// vision capable. None when the catalog is unavailable or lacks it.
    pub async fn model_supports_vision(&self, model_id: &str) -> Result<Option<bool>, String> {
        if model_id.trim().is_empty() {
            return Ok(None);
        }
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let maple_api_session = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, self.account_scope.as_ref())?;
            Arc::clone(&current.maple_api_session)
        };
        drop(_runtime_lifecycle_guard);

        let catalog = match maple_api_session.model_catalog().await {
            Ok(catalog) => catalog,
            Err(error) => {
                log::warn!("Failed to fetch Maple Agent model catalog: {error}");
                return Ok(None);
            }
        };
        Ok(catalog_supports_vision(&catalog, model_id))
    }

    pub async fn load_session(&self, session_id: String) -> Result<AgentSessionDetail, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let session_manager = {
            let runtime = state.inner.lock().await;
            match runtime.as_ref() {
                Some(current) => {
                    ensure_runtime_account(current, account_scope)?;
                    Arc::clone(&current.session_manager)
                }
                None => account_session_manager(&state.host.paths, user_id)?,
            }
        };
        let session = session_manager
            .get_session(&session_id, true)
            .await
            .map_err(|e| format!("Failed to load Agent task: {e}"))?;
        let conversation = session
            .conversation
            .as_ref()
            .ok_or_else(|| "Agent task history was not loaded".to_string())?;
        let timeline = conversation_to_timeline_items(conversation);
        let mut timeline = overlay_live_timeline(
            &state.live_timelines,
            &session_id,
            AgentPermissionRouting::Desktop,
            conversation,
            timeline,
        )
        .await;
        // Goose can persist an action-required row before Maple has registered
        // its responder. Reconcile the final Desktop projection against the
        // actual surface owner so another caller's request can never acquire
        // actionable Desktop buttons during that gap or from stale history.
        let calling_surface_active = {
            let runtime = state.inner.lock().await;
            runtime.as_ref().is_some_and(|current| {
                current.account_scope == account_scope
                    && current.active_runs.values().any(|run| {
                        run.session_id == session_id
                            && run.permission_routing == AgentPermissionRouting::CallingSurface
                    })
            })
        };
        let pending_routes = state
            .pending_permissions
            .lock()
            .await
            .iter()
            .filter(|((pending_session_id, _), _)| pending_session_id == &session_id)
            .map(|((_, request_id), pending)| (request_id.clone(), pending.routing))
            .collect::<HashMap<_, _>>();
        reconcile_desktop_permission_items(&mut timeline, &pending_routes, calling_surface_active);

        Ok(AgentSessionDetail {
            session: session_summary(&session),
            timeline,
            mcp_errors: Vec::new(),
            queue: snapshot_desktop_queue(state, account_scope, &session_id).await,
        })
    }

    pub async fn rename_session(
        &self,
        maple_api_session: Arc<MapleApiSession>,
        request: AgentRenameSessionRequest,
    ) -> Result<AgentSessionSummary, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        ensure_account_scope(maple_api_session.account_scope(), account_scope).map_err(|_| {
            "Maple API authentication belongs to a different signed-in account".to_string()
        })?;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;

        let session_id = request.session_id.trim().to_string();
        if session_id.is_empty() {
            return Err("Agent task rename requires a task ID".to_string());
        }
        let title = normalize_user_provided_session_title(&request.title)?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        let session_title_lifecycle = resolve_session_title_lifecycle(
            &state.session_title_lifecycles,
            account_scope,
            &session_id,
        )
        .await;
        let _auth_lease = maple_api_session.active_lease().await?;
        let session_manager = {
            let runtime = state.inner.lock().await;
            match runtime.as_ref() {
                Some(current) => {
                    ensure_runtime_account(current, account_scope)?;
                    Arc::clone(&current.session_manager)
                }
                None => account_session_manager(&state.host.paths, user_id)?,
            }
        };
        let _session_title_lifecycle_guard = session_title_lifecycle.lock().await;
        let current_session = session_manager
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to load Agent task before renaming: {error}"))?;
        if current_session.name == title {
            return Ok(session_summary(&current_session));
        }

        session_manager
            .update(&session_id)
            .user_provided_name(title)
            .apply()
            .await
            .map_err(|error| format!("Failed to rename Agent task: {error}"))?;
        let session = session_manager
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to load renamed Agent task: {error}"))?;
        let summary = session_summary(&session);
        emit_agent_event(
            &state.host.events,
            AgentServiceEvent::SessionUpdated {
                session_id,
                run_id: None,
                session: summary.clone(),
            },
        );
        Ok(summary)
    }

    /// A small asynchronous snapshot for the existing task menu. Execution and
    /// cleanup remain task-owned; polling this never resolves or starts Python.
    pub async fn python_status(&self, session_id: String) -> Result<AgentPythonStatus, String> {
        let state = &self.service;
        let _runtime_lifecycle = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let _session_lifecycle = state.session_lifecycle.lock().await;
        let (session_manager, resettable) = {
            let runtime = state.inner.lock().await;
            match runtime.as_ref() {
                Some(current) => {
                    ensure_runtime_account(current, &self.account_scope)?;
                    let resettable = current
                        .session_tool_contexts
                        .get(&session_id)
                        .filter(|installed| installed.owner == AgentToolContextOwner::Maple)
                        .and_then(|installed| installed.python.as_ref())
                        .is_some_and(|binding| binding.status().generation.is_some());
                    (Arc::clone(&current.session_manager), resettable)
                }
                None => (
                    account_session_manager(&state.host.paths, &self.user_id)?,
                    false,
                ),
            }
        };
        session_manager
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to find Agent task: {error}"))?;
        Ok(AgentPythonStatus { resettable })
    }

    /// Reset the task's current Python state, not a menu-time generation.
    /// Success confirms cleanup; all process waits happen outside lifecycle locks.
    pub async fn reset_python(&self, session_id: String) -> Result<(), String> {
        let cleanup = {
            let state = &self.service;
            let _runtime_lifecycle = state.runtime_lifecycle.lock().await;
            self.verify_generation().await?;
            self.ensure_accepting_new_work()?;
            let _session_lifecycle = state.session_lifecycle.lock().await;
            let (session_manager, agent_manager) = {
                let runtime = state.inner.lock().await;
                match runtime.as_ref() {
                    Some(current) => {
                        ensure_runtime_account(current, &self.account_scope)?;
                        if has_active_session_run(&current.active_runs, &session_id) {
                            return Err(
                                "Stop the running agent before resetting Python".to_string()
                            );
                        }
                        if current
                            .session_tool_contexts
                            .get(&session_id)
                            .is_some_and(|installed| {
                                installed.owner == AgentToolContextOwner::Leased
                            })
                        {
                            return Err("This task is controlled by another Agent surface; close its session or reset Python through that surface".to_string());
                        }
                        (
                            Arc::clone(&current.session_manager),
                            Some(Arc::clone(&current.agent_manager)),
                        )
                    }
                    None => (
                        account_session_manager(&state.host.paths, &self.user_id)?,
                        None,
                    ),
                }
            };
            session_manager
                .get_session(&session_id, false)
                .await
                .map_err(|error| format!("Failed to find Agent task: {error}"))?;
            // Goose claims this token before Maple releases its locks for MCP
            // preparation, earlier than insertion into current.active_runs.
            if let Some(manager) = agent_manager
                && manager.is_session_busy(&session_id).await
            {
                return Err(
                    "Stop or finish the preparing agent run before resetting Python".to_string(),
                );
            }
            let runtime = state.inner.lock().await;
            runtime
                .as_ref()
                .and_then(|current| current.session_tool_contexts.get(&session_id))
                .and_then(|installed| installed.python.as_ref())
                .map(|binding| {
                    if binding.is_closed() {
                        binding.retire("Python was reset by the user")
                    } else {
                        binding.reset("Python was reset by the user")
                    }
                })
        };
        if let Some(cleanup) = cleanup {
            cleanup.await.map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Archive or restore a task. Archived tasks keep their history and
    /// stay listed with `archived` set, so the UI can show them apart.
    pub async fn set_session_archived(
        &self,
        session_id: String,
        archived: bool,
    ) -> Result<AgentSessionSummary, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let session_id = session_id.trim().to_string();
        if session_id.is_empty() {
            return Err("Agent task ID cannot be empty".to_string());
        }
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        let session_manager = {
            let runtime = state.inner.lock().await;
            match runtime.as_ref() {
                Some(current) => {
                    ensure_runtime_account(current, account_scope)?;
                    if archived && has_active_session_run(&current.active_runs, &session_id) {
                        return Err("Stop the running agent before archiving this task".to_string());
                    }
                    Arc::clone(&current.session_manager)
                }
                None => account_session_manager(&state.host.paths, user_id)?,
            }
        };
        let current_session = session_manager
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to load Agent task before archiving: {error}"))?;
        if archived {
            let runtime = state.inner.lock().await;
            if let Some(installed) = runtime
                .as_ref()
                .and_then(|current| current.session_tool_contexts.get(&session_id))
            {
                // Preparation releases these lifecycle locks before becoming
                // an active run. Retire its captured Python capability now,
                // even if the following metadata write fails or is cancelled.
                installed.retire_python("Python state was retired when this task was archived");
            }
        }
        if current_session.archived_at.is_some() == archived {
            return Ok(session_summary(&current_session));
        }
        session_manager
            .update(&session_id)
            .archived_at(archived.then(chrono::Utc::now))
            .apply()
            .await
            .map_err(|error| format!("Failed to archive Agent task: {error}"))?;
        let session = session_manager
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to load archived Agent task: {error}"))?;
        let summary = session_summary(&session);
        emit_agent_event(
            &state.host.events,
            AgentServiceEvent::SessionUpdated {
                session_id,
                run_id: None,
                session: summary.clone(),
            },
        );
        Ok(summary)
    }

    /// Raw bytes of a stored image attachment, for display in the app.
    pub async fn read_image_attachment(
        &self,
        session_id: String,
        attachment_id: String,
    ) -> Result<Vec<u8>, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        account_session_manager(&state.host.paths, user_id)?
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to find Agent task {session_id}: {error}"))?;
        let store = account_attachment_store(&state.host.paths, user_id)?;
        tokio::task::spawn_blocking(move || store.read(&session_id, &attachment_id))
            .await
            .map_err(|error| format!("Agent image attachment task failed: {error}"))?
    }

    pub async fn list_session_mcp_servers(
        &self,
        session_id: String,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let session_manager = {
            let runtime = state.inner.lock().await;
            match runtime.as_ref() {
                Some(current) => {
                    ensure_runtime_account(current, account_scope)?;
                    Arc::clone(&current.session_manager)
                }
                None => account_session_manager(&state.host.paths, user_id)?,
            }
        };
        let session = session_manager
            .get_session(session_id.trim(), false)
            .await
            .map_err(|error| format!("Failed to load Agent task: {error}"))?;
        let stored_integrations = stored_integrations_for_read(&state.host.paths, user_id);
        let configured = effective_mcp_servers(
            &stored_integrations,
            load_agent_config_inner(&state.host.paths, user_id)
                .map_err(|error| format!("Failed to load MCP servers: {error}"))?
                .mcp_servers,
        )?;
        project_session_mcp_servers(&stored_integrations, &configured, &session)
    }

    pub async fn set_session_mcp_server_enabled(
        &self,
        request: AgentSetSessionMcpServerRequest,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        let session_id = request.session_id.trim().to_string();
        let requested_key = goose::config::extensions::name_to_key(request.name.trim());
        if session_id.is_empty() {
            return Err("Agent task ID cannot be empty".to_string());
        }
        if requested_key.is_empty() || maple_reserved_extension_key(&requested_key) {
            return Err("That MCP server cannot be changed".to_string());
        }

        let (agent_manager, session_manager, maple_api_session) = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, account_scope)?;
            if has_active_session_run(&current.active_runs, &session_id) {
                return Err("Stop the running agent before changing MCP servers".to_string());
            }
            if current
                .session_tool_contexts
                .get(&session_id)
                .is_some_and(|installed| installed.owner == AgentToolContextOwner::Leased)
            {
                return Err(
                    "Disconnect the external Agent surface before changing MCP servers".to_string(),
                );
            }
            (
                Arc::clone(&current.agent_manager),
                Arc::clone(&current.session_manager),
                Arc::clone(&current.maple_api_session),
            )
        };
        let stored_integrations = stored_integrations_for_read(&state.host.paths, user_id);
        let configured = effective_mcp_servers(
            &stored_integrations,
            load_agent_config_inner(&state.host.paths, user_id)
                .map_err(|error| format!("Failed to load MCP servers: {error}"))?
                .mcp_servers,
        )?;
        let session = session_manager
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to load Agent task: {error}"))?;
        let session_mcp_keys = session_mcp_extension_keys(&session);
        let manager_result = get_or_create_session_agent(
            &agent_manager,
            &maple_api_session,
            &session,
            &state.host.harness_instructions(),
            RuntimeContext::default(),
        )
        .await
        .map_err(|error| format!("Failed to load Goose agent: {error}"))?;
        for error in mcp_connection_errors(manager_result.extension_results, &session_mcp_keys) {
            log::warn!(
                "Failed to restore MCP server {}: {}",
                error.name,
                error.error
            );
        }
        let agent = manager_result.agent;
        if requested_key == CUA_DRIVER_MCP_NAME
            && session_cua_backend(&stored_integrations, &session)
                == Some(AgentIntegrationBackend::Embedded)
        {
            if session.session_type != SessionType::User {
                return Err(
                    "Built-in CUA is available only to tasks running in the Maple desktop app"
                        .to_string(),
                );
            }
            if request.enabled {
                // Settings does not select a model and cannot run tools. Use
                // the fail-closed text-model projection for this idle client;
                // every actual run replaces it using the authoritative model
                // capability from the catalog.
                attach_embedded_cua_client(
                    &agent,
                    &session,
                    maple_api_session.account_scope(),
                    false,
                )
                .await
                .map_err(|error| format!("Failed to start built-in CUA: {error}"))?;
                if let Err(error) = persist_session_cua_state(
                    session_manager.as_ref(),
                    &session_id,
                    CuaSessionState {
                        backend: AgentIntegrationBackend::Embedded,
                        enabled: true,
                    },
                )
                .await
                {
                    let _ = agent
                        .extension_manager
                        .remove_extension(CUA_DRIVER_MCP_NAME)
                        .await;
                    return Err(error);
                }
            } else {
                // Persist first: if storage fails, the still-live client and
                // durable state continue to agree that CUA is enabled.
                persist_session_cua_state(
                    session_manager.as_ref(),
                    &session_id,
                    CuaSessionState {
                        backend: AgentIntegrationBackend::Embedded,
                        enabled: false,
                    },
                )
                .await?;
                let _ = agent
                    .extension_manager
                    .remove_extension(CUA_DRIVER_MCP_NAME)
                    .await;
            }
            let refreshed = session_manager
                .get_session(&session_id, false)
                .await
                .map_err(|error| format!("Failed to reload Agent task: {error}"))?;
            return project_session_mcp_servers(&stored_integrations, &configured, &refreshed);
        }
        // Preflight Skills restoration before detaching the working client or changing persisted MCP
        // state. Reattaching this prepared client after the mutation cannot fail.
        let skills_client =
            prepare_transient_skills_client(&state.host.paths, user_id, &agent, &session)?;
        detach_transient_skills_client(&agent).await;
        let active = agent.get_extension_configs().await;
        let active_config = active
            .iter()
            .find(|config| mcp_transport_label(config).is_some() && config.key() == requested_key);

        let mutation_result: Result<(), String> = async {
            if request.enabled {
                if active_config.is_none() {
                    let server = configured
                        .iter()
                        .find(|server| {
                            goose::config::extensions::name_to_key(&server.name) == requested_key
                        })
                        .ok_or_else(|| {
                            format!(
                                "MCP server '{}' is no longer configured and cannot be enabled",
                                request.name.trim()
                            )
                        })?;
                    let extension = mcp_server_to_extension(server)?;
                    agent
                        .add_extension(extension, &session_id)
                        .await
                        .map_err(|error| {
                            format!("Failed to connect MCP server '{}': {error}", server.name)
                        })?;
                }
            } else if let Some(config) = active_config {
                agent
                    .remove_extension(&config.name(), &session_id)
                    .await
                    .map_err(|error| {
                        format!(
                            "Failed to disconnect MCP server '{}': {error}",
                            request.name.trim()
                        )
                    })?;
            } else {
                // A failed cold restore may already have removed the server from the
                // live manager. Persist that authoritative state so the UI still gets
                // a successful, durable disable operation.
                agent
                    .persist_extension_state(&session_id)
                    .await
                    .map_err(|error| format!("Failed to save task MCP settings: {error}"))?;
            }
            Ok(())
        }
        .await;
        attach_prepared_skills_client(&agent, skills_client).await;
        mutation_result?;

        if requested_key == CUA_DRIVER_MCP_NAME {
            persist_session_cua_state(
                session_manager.as_ref(),
                &session_id,
                CuaSessionState {
                    backend: AgentIntegrationBackend::External,
                    enabled: request.enabled,
                },
            )
            .await?;
        }

        let refreshed = session_manager
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to reload Agent task: {error}"))?;
        project_session_mcp_servers(&stored_integrations, &configured, &refreshed)
    }

    pub async fn delete_session(&self, session_id: String) -> Result<(), String> {
        self.delete_session_inner(session_id, true).await
    }

    async fn delete_session_inner(
        &self,
        session_id: String,
        require_admission: bool,
    ) -> Result<(), String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        if require_admission {
            self.ensure_accepting_new_work()?;
        }
        let session_id = session_id.trim().to_string();
        if session_id.is_empty() {
            return Err("Agent task ID cannot be empty".to_string());
        }

        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        let (agent_manager, session_manager, permission_modes, web_tool_state, title_task) = {
            let mut runtime = state.inner.lock().await;
            match runtime.as_mut() {
                Some(current) => {
                    ensure_runtime_account(current, account_scope)?;
                    if has_active_session_run(&current.active_runs, &session_id) {
                        return Err("Stop the running agent before deleting this task".to_string());
                    }
                    if current
                        .session_tool_contexts
                        .get(&session_id)
                        .is_some_and(|installed| installed.owner == AgentToolContextOwner::Leased)
                    {
                        return Err(
                            "Disconnect the external Agent surface before deleting this task"
                                .to_string(),
                        );
                    }
                    if let Some(installed) = current.session_tool_contexts.get(&session_id) {
                        installed
                            .retire_python("Python state was retired when this task was deleted");
                    }
                    (
                        Some(Arc::clone(&current.agent_manager)),
                        Arc::clone(&current.session_manager),
                        Some(Arc::clone(&current.permission_modes)),
                        Some(Arc::clone(&current.web_tool_state)),
                        current.session_title_tasks.remove(&session_id),
                    )
                }
                None => (
                    None,
                    account_session_manager(&state.host.paths, user_id)?,
                    None,
                    None,
                    None,
                ),
            }
        };

        if let Some(title_task) = title_task {
            title_task.token.cancel();
            join_agent_tasks(vec![title_task.task_handle], RUN_SHUTDOWN_TIMEOUT).await;
        }

        delete_persisted_agent_session(
            session_manager.as_ref(),
            &state.pending_permissions,
            &state.live_timelines,
            &state.subagents,
            web_tool_state.as_deref(),
            &session_id,
        )
        .await?;
        match account_attachment_store(&state.host.paths, user_id)
            .and_then(|store| store.delete_session(&session_id))
        {
            Ok(()) => {}
            Err(error) => {
                log::warn!("Deleted Agent task {session_id}, but failed to clear images: {error}")
            }
        }
        if let Some(agent_manager) = agent_manager
            && let Err(error) = agent_manager.remove_session_if_loaded(&session_id).await
        {
            log::warn!(
                "Deleted Goose session {session_id}, but failed to unload its agent: {error}"
            );
        }
        if let Some(permission_modes) = permission_modes {
            permission_modes.lock().await.remove(&session_id);
        }
        let _ = clear_desktop_queue(state, account_scope, &session_id).await;
        let removed_tool_context = {
            let mut runtime = state.inner.lock().await;
            if let Some(current) = runtime.as_mut() {
                ensure_runtime_account(current, account_scope)?;
                current.session_tool_contexts.remove(&session_id)
            } else {
                None
            }
        };
        if let Some(installed) = removed_tool_context {
            installed.revoke("Python task ownership ended");
        }

        Ok(())
    }
}

async fn delete_persisted_agent_session(
    session_manager: &SessionManager,
    pending_permissions: &PendingPermissions,
    live_timelines: &LiveTimelines,
    subagents: &SessionSubagents,
    web_tool_state: Option<&WebToolState>,
    session_id: &str,
) -> Result<(), String> {
    session_manager
        .get_session(session_id, false)
        .await
        .map_err(|e| format!("Failed to find Agent task {session_id}: {e}"))?;
    session_manager
        .delete_session(session_id)
        .await
        .map_err(|e| format!("Failed to delete Agent task {session_id}: {e}"))?;

    // The caller-owned prompt is keyed by task id; a deleted task never
    // needs it again, and the map would otherwise grow for the process life.
    store_session_system_prompt(session_id, None);
    live_timelines.lock().await.remove(session_id);
    subagents.lock().await.remove(session_id);
    pending_permissions
        .lock()
        .await
        .retain(|(pending_session_id, _), _| pending_session_id != session_id);
    if let Some(web_tool_state) = web_tool_state {
        web_tool_state.clear_session(session_id).await;
    }

    Ok(())
}

async fn finalize_cancelled_agent_turn(
    session_manager: &SessionManager,
    live_timelines: &LiveTimelines,
    web_tool_state: &WebToolState,
    session_id: &str,
    routing: AgentPermissionRouting,
    user_message: &Message,
    cancelled_permission_ids: &HashSet<String>,
) -> Result<(), String> {
    let mut session = session_manager
        .get_session(session_id, true)
        .await
        .map_err(|error| format!("Failed to inspect stopped Agent task: {error}"))?;
    let user_message_is_persisted = session.conversation.as_ref().is_some_and(|conversation| {
        conversation
            .messages()
            .iter()
            .any(|message| message.id == user_message.id)
    });
    if !user_message_is_persisted {
        session_manager
            .add_message(session_id, user_message)
            .await
            .map_err(|error| format!("Failed to retain stopped Agent prompt: {error}"))?;
    } else if let Some(conversation) = session.conversation.take()
        && let Some(repaired) =
            repair_cancelled_turn(&conversation, user_message, cancelled_permission_ids)
    {
        session_manager
            .replace_conversation(session_id, &repaired)
            .await
            .map_err(|error| format!("Failed to repair stopped Agent tool history: {error}"))?;
    }

    let stopped_notice = Message::assistant()
        .with_system_notification(SystemNotificationType::InlineMessage, "Stopped by user")
        .with_visibility(true, false)
        .with_generated_id();
    session_manager
        .add_message(session_id, &stopped_notice)
        .await
        .map_err(|error| format!("Failed to record stopped Agent turn: {error}"))?;

    // Goose's persisted conversation is the committed cancellation boundary.
    // Drop Maple's speculative event suffix so reloads project only that history.
    {
        let mut timelines = live_timelines.lock().await;
        remove_live_timeline_for_routing(&mut timelines, session_id, routing);
    }
    // Search provenance is an in-memory Maple permission convenience, not
    // Goose history. Reset it rather than letting a discarded search result
    // authorize a later open_url call. A cold session already starts empty.
    web_tool_state.clear_session(session_id).await;

    Ok(())
}

fn repair_cancelled_turn(
    conversation: &Conversation,
    user_message: &Message,
    cancelled_permission_ids: &HashSet<String>,
) -> Option<Conversation> {
    let messages = conversation.messages();
    let turn_start = messages
        .iter()
        .position(|message| message.id == user_message.id)?;
    let turn_messages = &messages[turn_start..];
    let cancelled_decline_ids: HashSet<String> = turn_messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            MessageContent::ToolResponse(response)
                if cancelled_permission_ids.contains(&response.id)
                    && is_goose_declined_tool_response(response) =>
            {
                Some(response.id.clone())
            }
            _ => None,
        })
        .collect();

    let mut repair_input = turn_messages.to_vec();
    if !cancelled_decline_ids.is_empty() {
        for message in &mut repair_input {
            message.content.retain(|content| match content {
                MessageContent::ToolRequest(request) => {
                    !cancelled_decline_ids.contains(&request.id)
                }
                MessageContent::ToolResponse(response) => {
                    !cancelled_decline_ids.contains(&response.id)
                }
                MessageContent::ActionRequired(action) => match &action.data {
                    ActionRequiredData::ToolConfirmation { id, .. } => {
                        !cancelled_decline_ids.contains(id)
                    }
                    _ => true,
                },
                _ => true,
            });
        }
        repair_input.retain(|message| !message.content.is_empty());
    }
    let removed_cancelled_decline = repair_input.as_slice() != turn_messages;

    let completed_tool_ids: HashSet<String> = repair_input
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            MessageContent::ToolResponse(response) => Some(response.id.clone()),
            _ => None,
        })
        .collect();
    let has_unmatched_request = repair_input
        .iter()
        .flat_map(|message| message.content.iter())
        .any(|content| match content {
            MessageContent::ToolRequest(request) => !completed_tool_ids.contains(&request.id),
            _ => false,
        });
    if !has_unmatched_request {
        if !removed_cancelled_decline {
            return None;
        }
        return Some(Conversation::new_unvalidated(
            messages[..turn_start]
                .iter()
                .cloned()
                .chain(repair_input)
                .collect::<Vec<_>>(),
        ));
    }

    // fix_conversation is Goose's canonical orphan-pair repair. It expects a
    // provider-ready conversation ending in a user message, while a stopped
    // turn may legitimately end in completed assistant content. Add a valid
    // temporary pair to protect that tail, then strip the pair after repair.
    let sentinel_id = format!(
        "maple-cancel-repair-{}",
        user_message.id.as_deref().unwrap_or("turn")
    );
    repair_input.push(
        Message::assistant()
            .with_tool_request(
                sentinel_id.clone(),
                Ok(rmcp::model::CallToolRequestParams::new(
                    "maple_cancel_repair_sentinel".to_string(),
                )),
            )
            .with_generated_id(),
    );
    repair_input.push(
        Message::user()
            .with_tool_response(
                sentinel_id.clone(),
                Ok(rmcp::model::CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text("cancel repair sentinel"),
                ])),
            )
            .with_generated_id(),
    );
    let (mut repaired_turn, _) = fix_conversation(Conversation::new_unvalidated(repair_input));
    for message in repaired_turn.messages_mut() {
        message.content.retain(|content| match content {
            MessageContent::ToolRequest(request) => request.id != sentinel_id,
            MessageContent::ToolResponse(response) => response.id != sentinel_id,
            _ => true,
        });
    }
    repaired_turn
        .messages_mut()
        .retain(|message| !message.content.is_empty());

    let repaired = Conversation::new_unvalidated(
        messages[..turn_start]
            .iter()
            .cloned()
            .chain(repaired_turn)
            .collect::<Vec<_>>(),
    );
    (repaired != *conversation).then_some(repaired)
}

fn is_goose_declined_tool_response(response: &goose::conversation::message::ToolResponse) -> bool {
    const DECLINED_PREFIX: &str = "The user has declined to run this tool.";
    response.tool_result.as_ref().is_ok_and(|result| {
        result.is_error == Some(true)
            && result.content.iter().any(|content| {
                content
                    .as_text()
                    .is_some_and(|text| text.text.starts_with(DECLINED_PREFIX))
            })
    })
}

/// How a model-catalog read resolved.
#[derive(Debug)]
enum CatalogFetch {
    /// A live fetch, or a cache entry still inside its TTL.
    Fresh(Vec<String>),
    /// The fetch failed and an expired cache answered instead.
    Stale(Vec<String>),
    /// No usable source; callers fall back to the default model.
    Failed,
}

#[derive(Serialize, Deserialize)]
struct CachedModelCatalog {
    fetched_at_ms: u64,
    models: Vec<String>,
}

/// A cached catalog answers session/new for this long without a network
/// round trip. The ACP process is short-lived, so the cache lives on disk
/// next to the account config and survives restarts.
const MODEL_CATALOG_TTL_MS: u64 = 10 * 60 * 1000;

/// Guards the background revalidation so concurrent readers in one process
/// trigger at most one refresh.
static MODEL_CATALOG_REFRESH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Resolve the catalog's vision flag without inventing one when metadata is absent.
/// An explicit alias capability (including false) overrides its target model.
fn catalog_supports_vision(
    catalog: &maple_sdk::ModelCatalogResponse,
    model_id: &str,
) -> Option<bool> {
    let mut concrete_id = model_id;
    for alias in &catalog.aliases {
        if alias.id == model_id {
            if let Some(vision) = alias.capabilities.as_ref().map(|c| c.vision) {
                return Some(vision);
            }
            if let Some(target) = alias.target_model.as_deref()
                && !target.trim().is_empty()
            {
                concrete_id = target;
            }
            break;
        }
    }
    catalog
        .data
        .iter()
        .find(|model| model.id == concrete_id)
        .and_then(|model| model.capabilities.as_ref())
        .map(|capabilities| capabilities.vision)
}

fn model_catalog_cache_path(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, String> {
    Ok(agent_config_dir(paths, user_id)
        .map_err(|e| e.to_string())?
        .join("model-catalog.json"))
}

fn model_catalog_is_fresh(fetched_at_ms: u64) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0);
    now.saturating_sub(fetched_at_ms) < MODEL_CATALOG_TTL_MS
}

async fn cached_model_catalog_fetch(
    paths: &AgentPathLayout,
    user_id: &str,
    maple_api_session: &Arc<MapleApiSession>,
    generation: u64,
) -> CatalogFetch {
    let cache_path = match model_catalog_cache_path(paths, user_id) {
        Ok(path) => path,
        Err(error) => {
            log::warn!("Failed to place the model catalog cache: {error}");
            PathBuf::new()
        }
    };
    let cached = if cache_path.as_os_str().is_empty() {
        None
    } else {
        let path = cache_path.clone();
        tokio::task::spawn_blocking(move || {
            fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<CachedModelCatalog>(&text).ok())
        })
        .await
        .ok()
        .flatten()
    };
    if let Some(record) = cached
        .as_ref()
        .filter(|record| model_catalog_is_fresh(record.fetched_at_ms))
    {
        log::debug!("Serving the cached Maple Agent model catalog");
        return CatalogFetch::Fresh(record.models.clone());
    }
    if let Some(record) = cached {
        // Stale-while-revalidate: answer from the cache without a network
        // round trip and refresh the file in the background, so a session
        // request never blocks on the catalog after the first fetch. A
        // catalog change therefore appears one connection later.
        if MODEL_CATALOG_REFRESH_IN_FLIGHT
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            let session = Arc::clone(maple_api_session);
            let path = cache_path.clone();
            tokio::spawn(async move {
                refresh_model_catalog(&path, &session).await;
                MODEL_CATALOG_REFRESH_IN_FLIGHT.store(false, Ordering::Relaxed);
            });
        }
        return CatalogFetch::Stale(record.models);
    }
    // No cache at all: fetch inline so a fresh install still gets a real
    // model list for its first session.
    let _ = generation;
    match maple_api_session.model_ids().await {
        Ok(models) => {
            write_model_catalog(&cache_path, &models).await;
            CatalogFetch::Fresh(models)
        }
        Err(error) => {
            log::warn!("Failed to refresh Maple Agent model catalog: {error}");
            CatalogFetch::Failed
        }
    }
}

/// Fetch the catalog once and persist it. Shared by the inline first fetch
/// and the background revalidation.
async fn refresh_model_catalog(path: &Path, maple_api_session: &Arc<MapleApiSession>) {
    match maple_api_session.model_ids().await {
        Ok(models) => write_model_catalog(path, &models).await,
        Err(error) => log::warn!("Background model catalog refresh failed: {error}"),
    }
}

async fn write_model_catalog(path: &Path, models: &[String]) {
    if path.as_os_str().is_empty() {
        return;
    }
    let record = CachedModelCatalog {
        fetched_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis() as u64)
            .unwrap_or(0),
        models: models.to_vec(),
    };
    let Ok(text) = serde_json::to_string(&record) else {
        return;
    };
    let path = path.to_path_buf();
    let written = tokio::task::spawn_blocking(move || write_model_catalog_file(&path, &text))
        .await
        .unwrap_or_else(|error| {
            log::warn!("Model catalog cache task failed: {error}");
            Err(std::io::Error::other("model catalog cache task failed"))
        });
    if written.is_err() {
        log::warn!("Failed to write the model catalog cache");
    }
}

fn write_model_catalog_file(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)
}

impl AgentRuntimeHandle {
    pub async fn send_message(
        &self,
        request: AgentSendMessageRequest,
    ) -> Result<AgentRunHandle, String> {
        self.send_message_inner(
            request,
            None,
            None,
            AgentHostEventPolicy::Publish,
            AgentPermissionRouting::Desktop,
            DesktopSendDisposition::StageOrStart,
        )
        .await
    }

    pub(crate) async fn send_message_with_tool_context(
        &self,
        request: AgentSendMessageRequest,
        access: AgentToolContextAccess,
        surface_lifetime: CancellationToken,
        host_events: AgentHostEventPolicy,
    ) -> Result<AgentRunHandle, String> {
        self.send_message_inner(
            request,
            Some(access),
            Some(surface_lifetime),
            host_events,
            AgentPermissionRouting::CallingSurface,
            DesktopSendDisposition::StartOnly,
        )
        .await
    }

    async fn send_message_inner(
        &self,
        request: AgentSendMessageRequest,
        tool_context_access: Option<AgentToolContextAccess>,
        surface_lifetime: Option<CancellationToken>,
        host_events: AgentHostEventPolicy,
        permission_routing: AgentPermissionRouting,
        desktop_send: DesktopSendDisposition,
    ) -> Result<AgentRunHandle, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        // Both fences are released again while this task's MCP servers start,
        // so they are held in rebindable slots rather than plain guards.
        let mut runtime_lifecycle_guard = Some(state.runtime_lifecycle.lock().await);
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let text = request.text.trim().to_string();
        if text.is_empty()
            && request.attachments.is_empty()
            && desktop_send == DesktopSendDisposition::StartOnly
        {
            return Err("Prompt cannot be empty".to_string());
        }
        // Reject this combination before the uploads reach the attachment
        // store; take_desktop_steer_plan repeats the check after the files
        // would already be on disk.
        if request.queue_id.is_some() && !request.attachments.is_empty() {
            return Err(QUEUED_MESSAGE_ATTACHMENTS_ERROR.to_string());
        }

        let mut session_lifecycle_guard = Some(state.session_lifecycle.lock().await);
        let prepared_images = if request.attachments.is_empty() {
            Vec::new()
        } else {
            let session_manager = account_session_manager(&state.host.paths, user_id)?;
            session_manager
                .get_session(&request.session_id, false)
                .await
                .map_err(|error| {
                    format!("Failed to find Agent task {}: {error}", request.session_id)
                })?;
            let store = account_attachment_store(&state.host.paths, user_id)?;
            let session_id = request.session_id.clone();
            let uploads = request.attachments.clone();
            tokio::task::spawn_blocking(move || store.store_uploads(&session_id, &uploads))
                .await
                .map_err(|error| format!("Agent image attachment task failed: {error}"))??
        };
        let draft_message = (!text.is_empty() || !prepared_images.is_empty())
            .then(|| user_message_with_images(&text, &prepared_images, request.vision_capable));
        let (launch_messages, mut started_queue, consume_queue_ids) = match desktop_send {
            DesktopSendDisposition::StageOrStart => {
                match self.take_desktop_send_plan(&request, draft_message).await? {
                    DesktopSendPlan::Staged {
                        run_id,
                        queued,
                        queue,
                    } => {
                        return Ok(staged_run_handle(run_id, *queued, queue));
                    }
                    DesktopSendPlan::Steered { run_id, queue } => {
                        return Ok(steered_run_handle(run_id, queue));
                    }
                    DesktopSendPlan::Start {
                        launch_messages,
                        queue,
                        consume_queue_ids,
                    } => (launch_messages, queue, consume_queue_ids),
                }
            }
            DesktopSendDisposition::StartOnly => {
                // The draft carries prompt images the way desktop sends do:
                // embedded for vision models, referenced through read_image
                // for everyone else. It is None only for an empty prompt.
                let Some(launch_message) = draft_message else {
                    return Err("Prompt cannot be empty".to_string());
                };
                reject_foreign_surface_session(state, account_scope, &request.session_id).await?;
                (
                    vec![launch_message],
                    empty_desktop_queue_snapshot(),
                    Vec::new(),
                )
            }
        };
        let session_title_lifecycle = resolve_session_title_lifecycle(
            &state.session_title_lifecycles,
            account_scope,
            &request.session_id,
        )
        .await;
        let run_id = next_run_id();
        let (run_events, run_events_rx) = AgentRunEventPublisher::new(
            state.host.events.clone(),
            request.session_id.clone(),
            run_id.clone(),
            host_events,
        );
        let cancel_token = surface_lifetime
            .as_ref()
            .map(CancellationToken::child_token)
            .unwrap_or_default();
        if cancel_token.is_cancelled() {
            return Err("Agent surface closed before the run could start".to_string());
        }
        let user_message = launch_messages
            .first()
            .cloned()
            .ok_or_else(|| "Prompt cannot be empty".to_string())?;
        let launch_text = user_message.as_concat_text();
        let prompt_title = session_title_from_prompt(&launch_text);
        let session_title_prompt = launch_text.clone();
        let (
            agent_manager,
            session_manager,
            maple_api_session,
            permission_modes,
            web_tool_state,
            runtime_lifetime,
            model,
            mode,
        ) = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, account_scope)?;
            (
                Arc::clone(&current.agent_manager),
                Arc::clone(&current.session_manager),
                Arc::clone(&current.maple_api_session),
                Arc::clone(&current.permission_modes),
                Arc::clone(&current.web_tool_state),
                // Stop and logout cancel this token, which ends a stalled MCP
                // startup that no run entry covers yet.
                current.lifetime.clone(),
                request
                    .model
                    .clone()
                    .unwrap_or_else(|| current.model.clone()),
                request.mode.clone().unwrap_or_else(|| current.mode.clone()),
            )
        };
        let requested_permission_mode = parse_user_permission_mode(&mode)?;
        // A calling surface may run its tasks in Maple's Auto policy (an ACP
        // "Approve all" mode). Every other mode stays caller-mediated: the
        // surface, not Maple, owns each unresolved interactive decision.
        if permission_routing == AgentPermissionRouting::CallingSurface
            && !is_caller_mediated_mode(requested_permission_mode)
            && requested_permission_mode != GooseMode::Auto
        {
            return Err(
                "External Agent surfaces support only caller-mediated permission modes".to_string(),
            );
        }

        if message_to_timeline_items(&user_message, false)
            .into_iter()
            .next()
            .is_none()
        {
            return Err("Failed to create user timeline item".to_string());
        }
        let live_timelines = Arc::clone(&state.live_timelines);
        let task_subagents = Arc::clone(&state.subagents);
        let task_subagent_host = state.clone();

        // Claim the session before changing its title, provider, mode, or
        // extensions. A duplicate send must not mutate an Agent that is already
        // serving another run.
        agent_manager
            .try_register_cancel_token(&request.session_id, cancel_token.clone())
            .await
            .map_err(|e| format!("Agent task is already running: {e}"))?;

        // A rejected or delayed send must not be able to change a live policy that
        // the mode command already made authoritative. Seed only sessions that do
        // not yet have runtime policy state, after Goose grants this run its
        // claim. A calling surface is itself authoritative: every prompt carries
        // its current mode, so a mode switch applies on the next turn. On failure
        // the previous entry is restored rather than cleared.
        let (permission_mode, mode_rollback) = {
            let mut modes = permission_modes.lock().await;
            if permission_routing == AgentPermissionRouting::CallingSurface {
                (
                    requested_permission_mode,
                    ModeRollback::Restore(
                        request.session_id.clone(),
                        modes.insert(request.session_id.clone(), requested_permission_mode),
                    ),
                )
            } else {
                select_session_permission_mode(
                    &mut modes,
                    &request.session_id,
                    requested_permission_mode,
                )
            }
        };
        let effective_mode = permission_mode.to_string();

        let mut fallback_title_applied = false;
        let setup_result: Result<AgentRunSetup, String> = async {
            // External surfaces present an opaque exact-match capability. Check
            // it before any persisted-session work so deletion that won the
            // session lifecycle race is reported as an expired surface task.
            let external_tool_context = if let Some(access) = tool_context_access.as_ref() {
                let mut runtime = state.inner.lock().await;
                let current = runtime
                    .as_mut()
                    .ok_or_else(|| "Agent runtime is not running".to_string())?;
                ensure_runtime_account(current, account_scope)?;
                Some(resolve_session_tool_context(
                    &mut current.session_tool_contexts,
                    account_scope,
                    &request.session_id,
                    Some(access),
                    &state.host.default_tool_context,
                )?)
            } else {
                None
            };
            let mut session = session_manager
                .get_session(&request.session_id, true)
                .await
                .map_err(|e| format!("Failed to load Agent task: {e}"))?;
            let usage_before = AgentRunUsage::from_accumulated_session(&session);
            validate_session_model_lock(
                session.message_count,
                session
                    .model_config
                    .as_ref()
                    .map(|model| model.model_name.as_str()),
                &model,
            )?;
            let tool_context = match external_tool_context {
                Some(context) => context,
                None => {
                    let mut runtime = state.inner.lock().await;
                    let current = runtime
                        .as_mut()
                        .ok_or_else(|| "Agent runtime is not running".to_string())?;
                    ensure_runtime_account(current, account_scope)?;
                    resolve_session_tool_context(
                        &mut current.session_tool_contexts,
                        account_scope,
                        &request.session_id,
                        None,
                        &state.host.default_tool_context,
                    )?
                }
            };
            let python_binding = {
                let mut runtime = state.inner.lock().await;
                let current = runtime
                    .as_mut()
                    .ok_or_else(|| "Agent runtime is not running".to_string())?;
                ensure_runtime_account(current, account_scope)?;
                resolve_session_python(state, current, &session, &tool_context, permission_routing)?
            };
            let should_name_from_prompt = should_name_session_from_prompt(&session);
            if should_name_from_prompt {
                session_manager
                    .update(&session.id)
                    .system_generated_name(prompt_title.clone())
                    .apply()
                    .await
                    .map_err(|e| format!("Failed to name Agent task: {e}"))?;
                fallback_title_applied = true;
                session = session_manager
                    .get_session(&session.id, false)
                    .await
                    .map_err(|e| format!("Failed to load named Agent task: {e}"))?;
                run_events
                    .publish(AgentRunEvent::SessionUpdated(session_summary(&session)))
                    .await;
            }
            // Restoring this task's persisted MCP servers spawns stdio
            // processes and performs remote handshakes, each bounded only by a
            // per-server timeout that defaults to DEFAULT_MCP_TIMEOUT_SECONDS.
            // Holding Maple's global fences across that would block Stop,
            // status, and task switching for minutes, so release both fences
            // and make the wait cancellation-selectable.
            let prepared = {
                let harness_instructions = state.host.harness_instructions();
                drop(session_lifecycle_guard.take());
                drop(runtime_lifecycle_guard.take());
                let outcome = await_mcp_startup(
                    &cancel_token,
                    &runtime_lifetime,
                    prepare_session_agent(
                        &agent_manager,
                        &maple_api_session,
                        &session,
                        &harness_instructions,
                    ),
                )
                .await;
                // Re-fence before touching shared runtime state again,
                // whatever the outcome, so the cleanup this function performs
                // on failure keeps the ordering the success path has.
                runtime_lifecycle_guard = Some(state.runtime_lifecycle.lock().await);
                session_lifecycle_guard = Some(state.session_lifecycle.lock().await);
                outcome?
            };
            // The runtime can be stopped, replaced, or switched to another
            // account while the fences are down. Never install Maple's
            // built-in tools into an Agent whose manager a concurrent stop is
            // already tearing down.
            self.verify_generation().await?;
            self.ensure_accepting_new_work()?;
            {
                let runtime = state.inner.lock().await;
                let current = runtime
                    .as_ref()
                    .ok_or_else(|| "Agent runtime is not running".to_string())?;
                ensure_runtime_account(current, account_scope)?;
                if !Arc::ptr_eq(&current.agent_manager, &agent_manager)
                    || !Arc::ptr_eq(&current.session_manager, &session_manager)
                    || !Arc::ptr_eq(&current.maple_api_session, &maple_api_session)
                {
                    return Err("Agent runtime changed while starting MCP servers".to_string());
                }
            }
            if cancel_token.is_cancelled() {
                return Err(MCP_STARTUP_CANCELLED_ERROR.to_string());
            }
            if python_binding.is_closed() || tool_context.is_revoked() {
                return Err("Agent task ownership changed while preparing Python tools".to_string());
            }
            let (agent, mcp_errors) = finish_session_agent(
                prepared,
                AgentSkillsScope {
                    paths: &state.host.paths,
                    user_id,
                },
                &session_manager,
                &maple_api_session,
                SessionAgentConfiguration {
                    web_tool_state: &web_tool_state,
                    session: &session,
                    model: &model,
                    context_limit: request.context_limit,
                    mode: &effective_mode,
                    primary_model_supports_vision: request.vision_capable,
                    tool_context: &tool_context,
                    python_binding: Some(&python_binding),
                    allow_embedded_cua: permission_routing == AgentPermissionRouting::Desktop,
                },
            )
            .await?;
            Ok((
                agent,
                mcp_errors,
                tool_context,
                should_name_from_prompt,
                usage_before,
            ))
        }
        .await;
        let (agent, mcp_errors, tool_context, generate_session_title, usage_before) =
            match setup_result {
                Ok(setup) => setup,
                Err(error) => {
                    if fallback_title_applied {
                        match restore_unused_agent_session_fallback_under_lifecycle(
                            session_manager.as_ref(),
                            &request.session_id,
                            &prompt_title,
                        )
                        .await
                        {
                            Ok(Some(session)) => {
                                run_events
                                    .publish(AgentRunEvent::SessionUpdated(session))
                                    .await;
                            }
                            Ok(None) => {}
                            Err(restore_error) => log::warn!("{restore_error}"),
                        }
                    }
                    mode_rollback.undo(&permission_modes).await;
                    agent_manager
                        .unregister_cancel_token(&request.session_id)
                        .await;
                    return Err(error);
                }
            };
        if !mcp_errors.is_empty() {
            run_events
                .publish(AgentRunEvent::SetupWarning(format_mcp_connection_errors(
                    &mcp_errors,
                )))
                .await;
        }
        if cancel_token.is_cancelled() {
            if generate_session_title {
                match restore_unused_agent_session_fallback_under_lifecycle(
                    session_manager.as_ref(),
                    &request.session_id,
                    &prompt_title,
                )
                .await
                {
                    Ok(Some(session)) => {
                        run_events
                            .publish(AgentRunEvent::SessionUpdated(session))
                            .await;
                    }
                    Ok(None) => {}
                    Err(error) => log::warn!("{error}"),
                }
            }
            mode_rollback.undo(&permission_modes).await;
            agent_manager
                .unregister_cancel_token(&request.session_id)
                .await;
            return Err("Agent surface closed before the run could start".to_string());
        }

        let task_events = run_events.clone();
        let state_inner = Arc::clone(&state.inner);
        let session_lifecycle = Arc::clone(&state.session_lifecycle);
        let task_pending_permissions = Arc::clone(&state.pending_permissions);
        let session_id = request.session_id.clone();
        let task_run_id = run_id.clone();
        let task_agent_manager = Arc::clone(&agent_manager);
        let task_session_manager = Arc::clone(&session_manager);
        let task_permission_modes = Arc::clone(&permission_modes);
        let task_web_tool_state = Arc::clone(&web_tool_state);
        let task_user_messages = launch_messages.clone();
        let task_cancel_token = cancel_token.clone();
        let task_agent = Arc::clone(&agent);
        let active_agent = Arc::clone(&agent);
        let cancelled_permission_ids = Arc::new(Mutex::new(HashSet::new()));
        let task_cancelled_permission_ids = Arc::clone(&cancelled_permission_ids);
        let accepting_queue = Arc::new(AtomicBool::new(true));
        let steered_unacked = Arc::new(Mutex::new(Vec::new()));
        let task_steered_unacked = Arc::clone(&steered_unacked);
        let task_accepting_queue = Arc::clone(&accepting_queue);
        let task_desktop_queues = Arc::clone(&state.desktop_queues);
        let task_account_scope = account_scope.to_string();
        let task_issued_permission_ids = Arc::new(Mutex::new(HashSet::new()));
        let (
            session_title_start,
            session_title_settled,
            session_title_registered,
            session_title_task,
        ) = if generate_session_title {
            // Once the first user message is persisted, title generation is a
            // session/account concern rather than part of the reply stream.
            // Stop/delete/logout own this token through the runtime registry.
            let title_cancel_token = CancellationToken::new();
            let (title_start_tx, title_start_rx) = oneshot::channel();
            let (title_settled_tx, title_settled_rx) = oneshot::channel();
            let (title_registered_tx, title_registered_rx) = oneshot::channel();
            let title_state_inner = Arc::clone(&state_inner);
            let title_session_manager = Arc::clone(&session_manager);
            let title_agent = Arc::clone(&agent);
            let title_session_lifecycle = Arc::clone(&session_lifecycle);
            let title_session_title_lifecycle = Arc::clone(&session_title_lifecycle);
            let title_session_id = session_id.clone();
            let title_first_prompt = session_title_prompt.clone();
            let title_expected_fallback = prompt_title.clone();
            let title_run_id = run_id.clone();
            let title_account_scope = account_scope.to_string();
            let title_dispatcher = state.host.events.clone();
            let title_task_cancel_token = title_cancel_token.clone();
            let title_cleanup_cancel_token = title_cancel_token.clone();
            let title_cleanup_session_id = title_session_id.clone();
            let title_cleanup_run_id = title_run_id.clone();
            let title_task_handle = tokio::spawn(async move {
                // Do not finish or self-remove before the owning runtime has
                // atomically registered this handle for Stop/delete.
                if title_registered_rx.await.is_err() {
                    return;
                }
                run_agent_session_title_task(AgentSessionTitleJob {
                    start: title_start_rx,
                    settled: title_settled_rx,
                    session_manager: title_session_manager,
                    agent: title_agent,
                    session_lifecycle: title_session_lifecycle,
                    session_title_lifecycle: title_session_title_lifecycle,
                    session_id: title_session_id,
                    first_prompt: title_first_prompt,
                    expected_fallback_title: title_expected_fallback,
                    cancel_token: title_task_cancel_token,
                    dispatcher: title_dispatcher,
                    host_events,
                })
                .await;
                remove_agent_session_title_task(
                    &title_state_inner,
                    &title_account_scope,
                    &title_cleanup_session_id,
                    &title_cleanup_run_id,
                )
                .await;
            });
            (
                Some(title_start_tx),
                Some(title_settled_tx),
                Some(title_registered_tx),
                Some(ActiveAgentSessionTitleTask {
                    run_id: title_run_id,
                    token: title_cleanup_cancel_token,
                    task_handle: title_task_handle,
                }),
            )
        } else {
            (None, None, None, None)
        };
        let (start_tx, start_rx) = oneshot::channel();
        let (terminal_tx, terminal_rx) = watch::channel(None);
        let (usage_tx, usage_rx) = watch::channel(None);
        let task = tokio::spawn(async move {
            let should_run = tokio::select! {
                biased;
                _ = task_cancel_token.cancelled() => false,
                start = start_rx => start.is_ok(),
            };
            let mut pending_user_messages = task_user_messages;
            let mut current_user_message = pending_user_messages
                .last()
                .cloned()
                .expect("a Desktop or ACP start always has at least one user message");
            let mut session_title_start = session_title_start;
            let mut result = Ok(AgentPromptOutcome::default());
            if should_run {
                loop {
                    if let Err((persisted, error)) = persist_leading_user_messages(
                        task_session_manager.as_ref(),
                        &session_id,
                        &pending_user_messages,
                    )
                    .await
                    {
                        // Same terminal path as a failed reply: the queue was
                        // already drained and the chips emitted as promoted, so
                        // give the unpersisted ones back as chips and tell the
                        // surface why the run stopped.
                        let _promote_guard = session_lifecycle.lock().await;
                        if permission_routing == AgentPermissionRouting::Desktop {
                            let snapshot = restore_unpersisted_desktop_queue_messages(
                                &task_desktop_queues,
                                &task_account_scope,
                                &session_id,
                                &pending_user_messages[persisted..],
                            )
                            .await;
                            task_events
                                .publish(AgentRunEvent::QueueChanged(snapshot))
                                .await;
                        }
                        let item = error_item(error.clone());
                        {
                            let mut timelines = live_timelines.lock().await;
                            apply_failed_prompt_outcome(
                                &mut timelines,
                                &session_id,
                                permission_routing,
                                item.clone(),
                            );
                        }
                        task_events.publish(AgentRunEvent::Error(item)).await;
                        task_accepting_queue.store(false, Ordering::Release);
                        result = Err(error);
                        break;
                    }
                    current_user_message = pending_user_messages
                        .last()
                        .cloned()
                        .expect("a promoted batch is never empty");
                    result = provider::with_run_cancellation(
                        task_cancel_token.clone(),
                        run_agent_prompt(AgentPromptRun {
                            events: task_events.clone(),
                            agent: Arc::clone(&task_agent),
                            session_manager: Arc::clone(&task_session_manager),
                            session_title_lifecycle: Arc::clone(&session_title_lifecycle),
                            live_timelines: live_timelines.clone(),
                            subagents: Arc::clone(&task_subagents),
                            subagent_host: Some((
                                task_subagent_host.clone(),
                                Arc::from(task_account_scope.as_str()),
                            )),
                            session_id: session_id.clone(),
                            user_message: current_user_message.clone(),
                            permission_modes: Arc::clone(&task_permission_modes),
                            web_tool_state: Arc::clone(&task_web_tool_state),
                            web_permission_context: WebPermissionContext::from_user_prompt(
                                &pending_user_messages
                                    .iter()
                                    .map(|message| message.as_concat_text())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            ),
                            cancel_token: task_cancel_token.clone(),
                            session_title_start: session_title_start.take(),
                            pending_permissions: Arc::clone(&task_pending_permissions),
                            issued_permission_ids: Arc::clone(&task_issued_permission_ids),
                            cancelled_permission_ids: Arc::clone(&task_cancelled_permission_ids),
                            run_id: task_run_id.clone(),
                            permission_routing,
                            steered_unacked: Arc::clone(&task_steered_unacked),
                        }),
                    )
                    .await;
                    if task_cancel_token.is_cancelled() {
                        break;
                    }
                    let should_continue = {
                        let _promote_guard = session_lifecycle.lock().await;
                        if task_cancel_token.is_cancelled() {
                            false
                        } else if let Ok(outcome) = &result {
                            let mut timelines = live_timelines.lock().await;
                            apply_successful_prompt_outcome(
                                &mut timelines,
                                &session_id,
                                permission_routing,
                                outcome,
                            );
                            drop(timelines);
                            // ACP still needs outcome cleanup and Error events.
                            // Only Desktop may promote leftover chips. Goose
                            // merges consecutive user roles for the provider
                            // request, so the model still sees one user turn.
                            if permission_routing != AgentPermissionRouting::Desktop {
                                task_accepting_queue.store(false, Ordering::Release);
                                false
                            } else {
                                match take_all_desktop_queue_items_from_map(
                                    &task_desktop_queues,
                                    &task_account_scope,
                                    &session_id,
                                )
                                .await
                                {
                                    Some((queued, snapshot)) => {
                                        pending_user_messages =
                                            queued.iter().map(queued_user_message).collect();
                                        current_user_message = pending_user_messages
                                            .last()
                                            .cloned()
                                            .expect("take_all returns at least one item");
                                        emit_promoted_queue_items(
                                            &task_events,
                                            &live_timelines,
                                            &session_id,
                                            permission_routing,
                                            &queued,
                                            snapshot,
                                        )
                                        .await;
                                        true
                                    }
                                    None => {
                                        task_accepting_queue.store(false, Ordering::Release);
                                        false
                                    }
                                }
                            }
                        } else {
                            if let Err(error) = &result {
                                let item = error_item(error.clone());
                                {
                                    let mut timelines = live_timelines.lock().await;
                                    apply_failed_prompt_outcome(
                                        &mut timelines,
                                        &session_id,
                                        permission_routing,
                                        item.clone(),
                                    );
                                }
                                task_events.publish(AgentRunEvent::Error(item)).await;
                            }
                            task_accepting_queue.store(false, Ordering::Release);
                            false
                        }
                    };
                    if !should_continue {
                        break;
                    }
                }
            }

            // Keep deletion serialized until every terminal write and event for
            // this run has completed. The active-run entry stays visible while
            // the cleanup is in progress, so deletion continues to reject it.
            let _session_lifecycle_guard = session_lifecycle.lock().await;
            // Completion and cancellation linearize under the same lock used by
            // agent_cancel_run. Whichever side acquires it first owns the terminal
            // result, so Stop cannot succeed against an already-settled run.
            task_accepting_queue.store(false, Ordering::Release);
            // Stop already ran this pair (mem::take makes a second persist a
            // no-op). Natural completion and provider errors must do the same
            // so a steer Goose never drained is not dropped or leaked into
            // the next reply on this pooled session agent.
            persist_unacked_steers(
                task_session_manager.as_ref(),
                &session_id,
                &task_steered_unacked,
            )
            .await;
            task_agent.discard_pending_steers(&session_id).await;
            let run_was_cancelled = !should_run || task_cancel_token.is_cancelled();
            let terminal_permissions = cancel_pending_permissions_for_runs(
                &task_pending_permissions,
                std::slice::from_ref(&task_run_id),
                &HashMap::from([(task_run_id.clone(), Arc::clone(&task_agent))]),
            )
            .await;
            if !terminal_permissions.is_empty() {
                task_cancelled_permission_ids.lock().await.extend(
                    terminal_permissions
                        .iter()
                        .map(|((_, request_id), _)| request_id.clone()),
                );
                for ((permission_session_id, request_id), _) in terminal_permissions {
                    if let Some(item) = update_live_permission_status(
                        &live_timelines,
                        &permission_session_id,
                        permission_routing,
                        &request_id,
                        "cancelled",
                    )
                    .await
                    {
                        task_events.publish(AgentRunEvent::TimelineItem(item)).await;
                    }
                }
            }
            let cancelled_permission_ids = task_cancelled_permission_ids.lock().await.clone();
            let result = if run_was_cancelled {
                finalize_cancelled_agent_turn(
                    task_session_manager.as_ref(),
                    &live_timelines,
                    task_web_tool_state.as_ref(),
                    &session_id,
                    permission_routing,
                    &current_user_message,
                    &cancelled_permission_ids,
                )
                .await
                .map(|_| AgentPromptOutcome::default())
            } else {
                result
            };
            if let Some(settled) = session_title_settled {
                // If the normal post-persistence start signal was never sent,
                // the title task may inspect the session only after cancellation
                // repair or reply failure has reached its durable boundary.
                let _ = settled.send(());
            }
            task_agent_manager
                .unregister_cancel_token(&session_id)
                .await;

            let (status, message) = match result {
                Ok(_) if run_was_cancelled => ("cancelled", None),
                Ok(_) => ("completed", None),
                Err(error) => ("failed", Some(error)),
            };
            if run_was_cancelled && let Some(error) = message.as_ref() {
                let item = error_item(error.clone());
                {
                    let mut timelines = live_timelines.lock().await;
                    apply_failed_prompt_outcome(
                        &mut timelines,
                        &session_id,
                        permission_routing,
                        item.clone(),
                    );
                }
                task_events.publish(AgentRunEvent::Error(item)).await;
            }
            // This retained per-run signal is authoritative for non-UI consumers.
            // It is deliberately published after runFinished so a receiver that
            // can still drain the broadcast stream observes all timeline chunks
            // before settling, while a lagged receiver can never miss completion.
            let terminal = match status {
                "cancelled" => AgentRunTerminal::Cancelled,
                "failed" => AgentRunTerminal::Failed,
                _ => AgentRunTerminal::Completed,
            };
            let usage = task_session_manager
                .get_session(&session_id, false)
                .await
                .map(|session| {
                    AgentRunUsage::from_accumulated_session(&session).saturating_delta(usage_before)
                })
                .unwrap_or_default();
            let _ = usage_tx.send(Some(usage));
            // Drop the subagents that ended with the run, before the
            // caller reads the snapshot this event sends it back for.
            end_run_subagents(&task_subagents, &session_id).await;
            task_events.publish(AgentRunEvent::Finished(terminal)).await;
            let _ = terminal_tx.send(Some(terminal));
            // Remove the stored JoinHandle only after the final externally visible
            // side effect. Stop may otherwise miss this task and return while its
            // runFinished event is still pending.
            let mut runtime = state_inner.lock().await;
            if let Some(current) = runtime.as_mut() {
                current.active_runs.remove(&task_run_id);
            }
        });

        let mut task = Some(task);
        let mut session_title_task = session_title_task;
        let insertion_error = {
            let mut runtime = state.inner.lock().await;
            match runtime.as_mut() {
                None => Some("Agent runtime is not running".to_string()),
                Some(current) => match ensure_runtime_account(current, account_scope) {
                    Err(error) => Some(error),
                    Ok(())
                        if session_title_task.is_some()
                            && current
                                .session_title_tasks
                                .contains_key(&request.session_id) =>
                    {
                        Some("Agent task title generation is already running".to_string())
                    }
                    Ok(()) => {
                        current.active_runs.insert(
                            run_id.clone(),
                            ActiveAgentRun {
                                agent: active_agent,
                                permission_routing,
                                token: cancel_token.clone(),
                                tool_context: tool_context.clone(),
                                session_id: request.session_id.clone(),
                                events: run_events.clone(),
                                cancelled_permission_ids: Arc::clone(&cancelled_permission_ids),
                                accepting_queue: Arc::clone(&accepting_queue),
                                steered_unacked: Arc::clone(&steered_unacked),
                                task_handle: task.take().expect("task handle must be available"),
                            },
                        );
                        if let Some(title_task) = session_title_task.take() {
                            current
                                .session_title_tasks
                                .insert(request.session_id.clone(), title_task);
                        }
                        None
                    }
                },
            }
        };
        if let Some(error) = insertion_error {
            let task = task.expect("failed insertion must retain task handle");
            task.abort();
            let _ = task.await;
            if let Some(title_task) = session_title_task {
                title_task.token.cancel();
                title_task.task_handle.abort();
                let _ = title_task.task_handle.await;
            }
            if generate_session_title {
                match restore_unused_agent_session_fallback_under_lifecycle(
                    session_manager.as_ref(),
                    &request.session_id,
                    &prompt_title,
                )
                .await
                {
                    Ok(Some(session)) => {
                        run_events
                            .publish(AgentRunEvent::SessionUpdated(session))
                            .await;
                    }
                    Ok(None) => {}
                    Err(restore_error) => log::warn!("{restore_error}"),
                }
            }
            // Mirror the setup-error path: a mode this send seeded must not
            // outlive the run that never started.
            mode_rollback.undo(&permission_modes).await;
            agent_manager
                .unregister_cancel_token(&request.session_id)
                .await;
            return Err(error);
        }
        if !consume_queue_ids.is_empty() {
            let mut consume_error = None;
            for queue_id in &consume_queue_ids {
                match remove_desktop_queue_item(state, account_scope, &request.session_id, queue_id)
                    .await
                {
                    Ok((_, snapshot)) => started_queue = snapshot,
                    Err(error) => {
                        consume_error = Some(error);
                        break;
                    }
                }
            }
            if consume_error.is_none() {
                publish_desktop_queue_changed(
                    state,
                    account_scope,
                    &request.session_id,
                    started_queue.clone(),
                )
                .await;
            }
        }
        if let Some(registered) = session_title_registered {
            let _ = registered.send(());
        }
        run_events.publish(AgentRunEvent::Started).await;

        for launch_message in &launch_messages {
            if let Some(item) = message_to_timeline_items(launch_message, false)
                .into_iter()
                .next()
            {
                record_and_emit_timeline_item(
                    &run_events,
                    &state.live_timelines,
                    &request.session_id,
                    permission_routing,
                    item,
                )
                .await;
            }
        }
        let _ = start_tx.send(());
        // Keep the session claimed until the optimistic timeline item and start
        // signal are ordered. A cancellation cleanup must not finish and then be
        // followed by this send path re-appending the cancelled prompt.
        drop(session_lifecycle_guard.take());

        let permission_responder =
            matches!(permission_routing, AgentPermissionRouting::CallingSurface).then(|| {
                AgentRunPermissionResponder {
                    agent: self.clone(),
                    session_id: Arc::from(request.session_id.as_str()),
                    run_id: Arc::from(run_id.as_str()),
                }
            });
        let cancellation = matches!(permission_routing, AgentPermissionRouting::CallingSurface)
            .then(|| AgentRunCancellation {
                agent: self.clone(),
                session_id: Arc::from(request.session_id.as_str()),
                run_id: Arc::from(run_id.as_str()),
                routing: permission_routing,
            });
        Ok(AgentRunHandle {
            run_id,
            events: run_events_rx,
            terminal: terminal_rx,
            usage: usage_rx,
            event_overflowed: run_events.overflow_flag(),
            permission_responder,
            cancellation,
            queued: None,
            queue: started_queue,
        })
    }

    async fn take_desktop_send_plan(
        &self,
        request: &AgentSendMessageRequest,
        draft_message: Option<Message>,
    ) -> Result<DesktopSendPlan, String> {
        let state = &self.service;
        let account_scope = self.account_scope.as_ref();
        reject_foreign_surface_session(state, account_scope, &request.session_id).await?;
        let text = request.text.trim();
        if request.steer {
            return take_desktop_steer_plan(
                state,
                account_scope,
                request,
                text,
                draft_message.as_ref(),
            )
            .await;
        }
        if let Some(run_id) =
            desktop_run_id_for_session(state, account_scope, &request.session_id).await?
        {
            let Some(draft_message) = draft_message else {
                return Err("Prompt cannot be empty".to_string());
            };
            let (queued, snapshot) = enqueue_desktop_queue_message(
                state,
                account_scope,
                &request.session_id,
                text,
                draft_message,
            )
            .await?;
            publish_desktop_queue_changed(
                state,
                account_scope,
                &request.session_id,
                snapshot.clone(),
            )
            .await;
            return Ok(DesktopSendPlan::Staged {
                run_id,
                queue: snapshot,
                queued: Box::new(queued),
            });
        }

        let launch = prepare_desktop_launch_message(
            state,
            account_scope,
            &request.session_id,
            draft_message,
        )
        .await?;
        Ok(DesktopSendPlan::Start {
            launch_messages: launch.launch_messages,
            queue: launch.queue,
            consume_queue_ids: launch.consume_queue_ids,
        })
    }

    pub async fn cancel_queued_message(
        &self,
        request: AgentQueueControlRequest,
    ) -> Result<AgentDesktopQueueSnapshot, String> {
        let state = &self.service;
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        let (_, snapshot) =
            remove_desktop_queue_item(state, account_scope, &request.session_id, &request.queue_id)
                .await?;
        publish_desktop_queue_changed(state, account_scope, &request.session_id, snapshot.clone())
            .await;
        Ok(snapshot)
    }

    #[cfg(test)]
    pub async fn unqueue_message_for_edit(
        &self,
        request: AgentQueueControlRequest,
    ) -> Result<AgentQueuedMessage, String> {
        let state = &self.service;
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        let (removed, snapshot) =
            remove_desktop_queue_item(state, account_scope, &request.session_id, &request.queue_id)
                .await?;
        publish_desktop_queue_changed(state, account_scope, &request.session_id, snapshot).await;
        Ok(removed)
    }

    pub async fn begin_queued_message_edit(
        &self,
        request: AgentQueueControlRequest,
    ) -> Result<(), String> {
        let state = &self.service;
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        begin_desktop_queue_edit(state, account_scope, &request.session_id, &request.queue_id).await
    }

    pub async fn end_queued_message_edit(
        &self,
        request: AgentQueueControlRequest,
    ) -> Result<(), String> {
        let state = &self.service;
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        end_desktop_queue_edit(state, account_scope, &request.session_id, &request.queue_id).await
    }

    pub async fn cancel_desktop_run(&self, run_id: String) -> Result<(), String> {
        self.cancel_run_scoped(&run_id, None, AgentPermissionRouting::Desktop)
            .await
    }

    async fn cancel_run_scoped(
        &self,
        run_id: &str,
        expected_session_id: Option<&str>,
        expected_routing: AgentPermissionRouting,
    ) -> Result<(), String> {
        let state = &self.service;
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        // Order permission updates before the worker's authoritative reload and
        // terminal event. If the worker settled first, its active-run entry will
        // already be gone by the time this command inspects it.
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        let (
            agent,
            session_id,
            session_manager,
            steered_unacked,
            cancel_token,
            tool_context,
            run_events,
            cancelled_permission_ids,
            accepting_queue,
        ) = {
            let runtime = state.inner.lock().await;
            let Some(current) = runtime.as_ref() else {
                return Ok(());
            };
            ensure_runtime_account(current, account_scope)?;
            let Some(active_run) = current.active_runs.get(run_id) else {
                return Ok(());
            };
            validate_run_cancellation_scope(
                active_run.session_id.as_str(),
                active_run.permission_routing,
                expected_session_id,
                expected_routing,
            )?;
            (
                Arc::clone(&active_run.agent),
                active_run.session_id.clone(),
                Arc::clone(&current.session_manager),
                Arc::clone(&active_run.steered_unacked),
                active_run.token.clone(),
                active_run.tool_context.clone(),
                active_run.events.clone(),
                Arc::clone(&active_run.cancelled_permission_ids),
                Arc::clone(&active_run.accepting_queue),
            )
        };
        persist_unacked_steers(session_manager.as_ref(), &session_id, &steered_unacked).await;
        agent.discard_pending_steers(&session_id).await;
        accepting_queue.store(false, Ordering::Release);
        tool_context.cancel_run(&cancel_token);
        let run_id = run_id.to_string();
        let cancelled_permissions = cancel_pending_permissions_for_runs(
            &state.pending_permissions,
            std::slice::from_ref(&run_id),
            &HashMap::from([(run_id.clone(), agent)]),
        )
        .await;
        cancelled_permission_ids.lock().await.extend(
            cancelled_permissions
                .iter()
                .map(|((_, request_id), _)| request_id.clone()),
        );
        for ((session_id, request_id), _) in cancelled_permissions {
            if let Some(item) = update_live_permission_status(
                &state.live_timelines,
                &session_id,
                expected_routing,
                &request_id,
                "cancelled",
            )
            .await
            {
                run_events.publish(AgentRunEvent::TimelineItem(item)).await;
            }
        }
        Ok(())
    }

    /// Turn the web tools on or off for one task. Takes effect on the next
    /// turn: the developer client is rebuilt per run from the session.
    pub async fn set_session_web_enabled(
        &self,
        request: AgentSetSessionWebRequest,
    ) -> Result<AgentSessionSummary, String> {
        let state = &self.service;
        let user_id = self.user_id.as_ref();
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let session_id = request.session_id.trim().to_string();
        if session_id.is_empty() {
            return Err("Agent task ID cannot be empty".to_string());
        }
        let session_manager = {
            let runtime = state.inner.lock().await;
            match runtime.as_ref() {
                Some(current) => {
                    ensure_runtime_account(current, account_scope)?;
                    Arc::clone(&current.session_manager)
                }
                None => account_session_manager(&state.host.paths, user_id)?,
            }
        };
        let session = session_manager
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to load Agent task: {error}"))?;
        let mut extension_data = session.extension_data.clone();
        extension_data.set_extension_state(
            MAPLE_WEB_STATE_KEY,
            MAPLE_WEB_STATE_VERSION,
            json!({ "enabled": request.enabled }),
        );
        session_manager
            .update(&session_id)
            .extension_data(extension_data)
            .apply()
            .await
            .map_err(|error| format!("Failed to persist web setting: {error}"))?;
        let session = session_manager
            .get_session(&session_id, false)
            .await
            .map_err(|error| format!("Failed to load Agent task: {error}"))?;
        Ok(session_summary(&session))
    }

    /// Persist a caller-selected permission mode to the task row. Runtime
    /// policy follows on the next prompt regardless, which carries the
    /// live mode; this only decides what a later connection loads.
    pub async fn persist_session_permission_mode(
        &self,
        session_id: &str,
        mode: &str,
    ) -> Result<(), String> {
        let goose_mode = parse_user_permission_mode(mode)?;
        let state = &self.service;
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        let session_manager = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, self.account_scope.as_ref())?;
            Arc::clone(&current.session_manager)
        };
        session_manager
            .update(session_id)
            .goose_mode(goose_mode)
            .apply()
            .await
            .map_err(|error| format!("Failed to save the Agent task's mode: {error}"))
    }

    pub async fn set_permission_mode(
        &self,
        request: AgentPermissionModeRequest,
    ) -> Result<(), String> {
        let state = &self.service;
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;

        let session_id = request.session_id.trim().to_string();
        if session_id.is_empty() {
            return Err("Agent permission mode update requires a task ID".to_string());
        }
        let session_title_lifecycle = resolve_session_title_lifecycle(
            &state.session_title_lifecycles,
            account_scope,
            &session_id,
        )
        .await;
        let goose_mode = parse_user_permission_mode(&request.mode)?;
        let (agent_manager, session_manager, maple_api_session, permission_modes, active_agent) = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, account_scope)?;
            if current.active_runs.values().any(|run| {
                run.session_id == session_id
                    && run.permission_routing == AgentPermissionRouting::CallingSurface
            }) || current
                .session_tool_contexts
                .get(&session_id)
                .is_some_and(|installed| installed.owner == AgentToolContextOwner::Leased)
            {
                return Err("This Agent task is controlled by another Agent surface".to_string());
            }
            (
                Arc::clone(&current.agent_manager),
                Arc::clone(&current.session_manager),
                Arc::clone(&current.maple_api_session),
                Arc::clone(&current.permission_modes),
                current
                    .active_runs
                    .values()
                    .find(|run| {
                        run.session_id == session_id
                            && run.permission_routing == AgentPermissionRouting::Desktop
                    })
                    .map(|run| Arc::clone(&run.agent)),
            )
        };

        // Restrictive transitions take effect before any fallible Goose or disk
        // work. Otherwise the selector could say Read only while a still-live Auto
        // policy approves the next write. If setup fails, restore the previous
        // policy so the command and optimistic UI can roll back consistently.
        let previous_restrictive_mode = if goose_mode == GooseMode::SmartApprove {
            permission_modes
                .lock()
                .await
                .insert(session_id.clone(), goose_mode)
        } else {
            None
        };
        let update_result: Result<Arc<Agent>, String> = async {
            let session = session_manager
                .get_session(&session_id, false)
                .await
                .map_err(|error| format!("Failed to load Agent task: {error}"))?;
            let agent = match active_agent {
                Some(agent) => agent,
                None => {
                    get_or_create_session_agent(
                        &agent_manager,
                        &maple_api_session,
                        &session,
                        &state.host.harness_instructions(),
                        RuntimeContext::default(),
                    )
                    .await
                    .map_err(|error| {
                        format!("Failed to resolve Goose agent for mode update: {error}")
                    })?
                    .agent
                }
            };
            agent
                .update_goose_mode(GOOSE_PERMISSION_ROUTING_MODE, &session_id)
                .await
                .map_err(|error| format!("Failed to update Goose mode: {error}"))?;
            // update_goose_mode already persists SmartApprove, which is both our
            // internal Goose routing mode and the user-facing Read-only mode. Auto
            // is Maple-owned, so only that case needs a second persistence step.
            // Keeping Read-only to one write avoids a failed duplicate write
            // leaving the persisted session stricter than the live Maple policy.
            if goose_mode == GooseMode::Auto {
                session_manager
                    .update(&session_id)
                    .goose_mode(goose_mode)
                    .apply()
                    .await
                    .map_err(|error| format!("Failed to persist Agent permission mode: {error}"))?;
            }
            Ok(agent)
        }
        .await;
        let agent = match update_result {
            Ok(agent) => agent,
            Err(error) => {
                if goose_mode == GooseMode::SmartApprove {
                    let mut modes = permission_modes.lock().await;
                    match previous_restrictive_mode {
                        Some(previous) => {
                            modes.insert(session_id.clone(), previous);
                        }
                        None => {
                            modes.remove(&session_id);
                        }
                    }
                }
                return Err(error);
            }
        };
        if goose_mode == GooseMode::Auto {
            permission_modes
                .lock()
                .await
                .insert(session_id.clone(), goose_mode);
        }
        // The runtime-wide mode is the default for sends that omit one. It is
        // fixed at start; one task's choice must not leak into other tasks.

        if goose_mode == GooseMode::Auto {
            let request_ids = {
                let mut pending = state.pending_permissions.lock().await;
                let request_ids = pending
                    .iter()
                    .filter(|((pending_session_id, _), request)| {
                        pending_session_id == &session_id
                            && request.routing == AgentPermissionRouting::Desktop
                    })
                    .map(|((_, request_id), _)| request_id.clone())
                    .collect::<Vec<_>>();
                for request_id in &request_ids {
                    pending.remove(&(session_id.clone(), request_id.clone()));
                }
                request_ids
            };
            for request_id in request_ids {
                deliver_tool_permission(&agent, request_id.clone(), Permission::AllowOnce).await;
                if let Some(item) = update_live_permission_status(
                    &state.live_timelines,
                    &session_id,
                    AgentPermissionRouting::Desktop,
                    &request_id,
                    "allow_once",
                )
                .await
                {
                    emit_agent_event(
                        &state.host.events,
                        AgentServiceEvent::TimelineItem {
                            session_id: session_id.clone(),
                            run_id: None,
                            item,
                        },
                    );
                }
            }
        }

        // The policy is already committed at this point. A best-effort refresh
        // must not report failure to the selector and make it roll back to a mode
        // that is no longer authoritative.
        let _session_title_lifecycle_guard = session_title_lifecycle.lock().await;
        match session_manager.get_session(&session_id, false).await {
            Ok(session) => emit_agent_event(
                &state.host.events,
                AgentServiceEvent::SessionUpdated {
                    session_id,
                    run_id: None,
                    session: session_summary(&session),
                },
            ),
            Err(error) => log::warn!(
                "Agent permission mode was updated, but the refreshed session could not be loaded: {error}"
            ),
        }
        Ok(())
    }

    pub async fn permission_respond(
        &self,
        response: AgentPermissionResponse,
    ) -> Result<(), String> {
        let decision = permission_decision_from_str(&response.decision)?;
        let display_status = response.decision.clone();
        self.resolve_permission(
            response.session_id,
            response.request_id,
            decision,
            AgentPermissionResponseScope::Desktop,
            Some(display_status),
        )
        .await
    }

    async fn permission_respond_for_run(
        &self,
        session_id: &str,
        run_id: &str,
        request_id: String,
        decision: AgentPermissionDecision,
    ) -> Result<(), String> {
        self.resolve_permission(
            session_id.to_string(),
            request_id,
            decision,
            AgentPermissionResponseScope::CallingSurface {
                run_id: run_id.to_string(),
            },
            None,
        )
        .await
    }

    async fn resolve_permission(
        &self,
        session_id: String,
        request_id: String,
        decision: AgentPermissionDecision,
        scope: AgentPermissionResponseScope,
        display_status: Option<String>,
    ) -> Result<(), String> {
        let state = &self.service;
        let account_scope = self.account_scope.as_ref();
        let _runtime_lifecycle_guard = state.runtime_lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let _session_lifecycle_guard = state.session_lifecycle.lock().await;
        let session_id = session_id.trim().to_string();
        if session_id.is_empty() {
            return Err("Agent permission response requires a task ID".to_string());
        }
        if request_id.trim().is_empty() {
            return Err("Agent permission response requires a request ID".to_string());
        }
        let (agent, run_id, expected_routing, run_events, cancelled_permission_ids) = {
            let runtime = state.inner.lock().await;
            let current = runtime
                .as_ref()
                .ok_or_else(|| "Agent runtime is not running".to_string())?;
            ensure_runtime_account(current, account_scope)?;
            let (run_id, expected_routing, active_run) = match &scope {
                AgentPermissionResponseScope::Desktop => {
                    let (run_id, active_run) = current
                        .active_runs
                        .iter()
                        .find(|(_, run)| run.session_id == session_id)
                        .ok_or_else(|| {
                            format!(
                                "No running Agent task found for permission request {request_id}"
                            )
                        })?;
                    (run_id.clone(), AgentPermissionRouting::Desktop, active_run)
                }
                AgentPermissionResponseScope::CallingSurface { run_id } => {
                    let active_run = current.active_runs.get(run_id).ok_or_else(|| {
                        format!("No running Agent task found for permission request {request_id}")
                    })?;
                    if active_run.session_id != session_id {
                        return Err("Agent permission responder does not own this task".to_string());
                    }
                    (
                        run_id.clone(),
                        AgentPermissionRouting::CallingSurface,
                        active_run,
                    )
                }
            };
            if active_run.token.is_cancelled() {
                return Err("Agent permission request is already cancelled".to_string());
            }
            (
                Arc::clone(&active_run.agent),
                run_id,
                expected_routing,
                active_run.events.clone(),
                Arc::clone(&active_run.cancelled_permission_ids),
            )
        };
        let key = (session_id.clone(), request_id.clone());
        {
            let mut pending = state.pending_permissions.lock().await;
            let Some(request) = pending.get(&key) else {
                return Err(format!(
                    "No pending Agent Mode permission request found for {request_id} in task {session_id}"
                ));
            };
            if request.run_id != run_id || request.routing != expected_routing {
                return Err("Agent permission responder does not own this request".to_string());
            }
            pending.remove(&key);
        }
        if decision == AgentPermissionDecision::Cancel {
            cancelled_permission_ids
                .lock()
                .await
                .insert(request_id.clone());
        }
        agent
            .handle_confirmation(
                request_id.clone(),
                PermissionConfirmation {
                    principal_type: PrincipalType::Tool,
                    permission: decision.goose_permission(),
                },
            )
            .await;
        if let Some(item) = update_live_permission_status(
            &state.live_timelines,
            &session_id,
            expected_routing,
            &request_id,
            display_status
                .as_deref()
                .unwrap_or_else(|| decision.status()),
        )
        .await
        {
            match scope {
                AgentPermissionResponseScope::Desktop => emit_agent_event(
                    &state.host.events,
                    AgentServiceEvent::TimelineItem {
                        session_id,
                        run_id: None,
                        item,
                    },
                ),
                AgentPermissionResponseScope::CallingSurface { .. } => {
                    run_events.publish(AgentRunEvent::TimelineItem(item)).await;
                }
            }
        }
        Ok(())
    }
}

fn validate_run_cancellation_scope(
    actual_session_id: &str,
    actual_routing: AgentPermissionRouting,
    expected_session_id: Option<&str>,
    expected_routing: AgentPermissionRouting,
) -> Result<(), String> {
    if actual_routing != expected_routing {
        return Err("Agent run is controlled by another Agent surface".to_string());
    }
    if expected_session_id.is_some_and(|session_id| session_id != actual_session_id) {
        return Err("Agent run cancellation capability does not own this task".to_string());
    }
    Ok(())
}

struct AgentPromptRun {
    events: AgentRunEventPublisher,
    agent: Arc<Agent>,
    session_manager: Arc<SessionManager>,
    session_title_lifecycle: Arc<Mutex<()>>,
    live_timelines: LiveTimelines,
    subagents: SessionSubagents,
    /// The service and account a background subagent watcher reports to.
    /// A watcher outlives this run, so it cannot borrow the run's state.
    subagent_host: Option<(MapleAgentService, Arc<str>)>,
    session_id: String,
    user_message: Message,
    permission_modes: SessionPermissionModes,
    web_tool_state: Arc<WebToolState>,
    web_permission_context: WebPermissionContext,
    cancel_token: CancellationToken,
    session_title_start: Option<oneshot::Sender<()>>,
    pending_permissions: PendingPermissions,
    issued_permission_ids: IssuedPermissionIds,
    cancelled_permission_ids: CancelledPermissionIds,
    run_id: String,
    permission_routing: AgentPermissionRouting,
    steered_unacked: Arc<Mutex<Vec<Message>>>,
}

#[derive(Default)]
struct AgentPromptOutcome {
    terminal_message: Option<LiveMessageCandidate>,
}

#[derive(Clone, Debug, PartialEq)]
struct LiveMessageCandidate {
    id: Option<String>,
    role: String,
    created: i64,
    items: Vec<AgentTimelineItem>,
}

fn apply_successful_prompt_outcome(
    timelines: &mut HashMap<String, LiveTimelineEntry>,
    session_id: &str,
    routing: AgentPermissionRouting,
    outcome: &AgentPromptOutcome,
) {
    if routing == AgentPermissionRouting::CallingSurface {
        remove_live_timeline_for_routing(timelines, session_id, routing);
        return;
    }
    match outcome.terminal_message.as_ref() {
        Some(candidate) => {
            timelines.insert(
                session_id.to_string(),
                LiveTimelineEntry {
                    routing,
                    timeline: LiveTimeline::Completed(candidate.clone()),
                },
            );
        }
        None => {
            remove_live_timeline_for_routing(timelines, session_id, routing);
        }
    }
}

fn apply_failed_prompt_outcome(
    timelines: &mut HashMap<String, LiveTimelineEntry>,
    session_id: &str,
    routing: AgentPermissionRouting,
    item: AgentTimelineItem,
) {
    if routing == AgentPermissionRouting::CallingSurface {
        remove_live_timeline_for_routing(timelines, session_id, routing);
        return;
    }
    timelines.insert(
        session_id.to_string(),
        LiveTimelineEntry {
            routing,
            timeline: LiveTimeline::Failed(vec![item]),
        },
    );
}

fn remove_live_timeline_for_routing(
    timelines: &mut HashMap<String, LiveTimelineEntry>,
    session_id: &str,
    routing: AgentPermissionRouting,
) -> Option<LiveTimelineEntry> {
    timelines
        .get(session_id)
        .is_some_and(|entry| entry.routing == routing)
        .then(|| {
            timelines
                .remove(session_id)
                .expect("matching live timeline must still exist")
        })
}

async fn selected_permission_mode(
    permission_modes: &SessionPermissionModes,
    session_id: &str,
) -> GooseMode {
    permission_modes
        .lock()
        .await
        .get(session_id)
        .copied()
        .unwrap_or(GOOSE_PERMISSION_ROUTING_MODE)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModeRollback {
    None,
    Remove(String),
    Restore(String, Option<GooseMode>),
}

impl ModeRollback {
    async fn undo(self, permission_modes: &SessionPermissionModes) {
        match self {
            Self::None => {}
            Self::Remove(session_id) => {
                permission_modes.lock().await.remove(&session_id);
            }
            Self::Restore(session_id, previous) => {
                let mut modes = permission_modes.lock().await;
                match previous {
                    Some(mode) => {
                        modes.insert(session_id, mode);
                    }
                    None => {
                        modes.remove(&session_id);
                    }
                }
            }
        }
    }
}

fn select_session_permission_mode(
    permission_modes: &mut HashMap<String, GooseMode>,
    session_id: &str,
    requested_mode: GooseMode,
) -> (GooseMode, ModeRollback) {
    if let Some(mode) = permission_modes.get(session_id).copied() {
        (mode, ModeRollback::None)
    } else {
        permission_modes.insert(session_id.to_string(), requested_mode);
        (requested_mode, ModeRollback::Remove(session_id.to_string()))
    }
}

async fn deliver_tool_permission(agent: &Agent, request_id: String, permission: Permission) {
    agent
        .handle_confirmation(
            request_id,
            PermissionConfirmation {
                principal_type: PrincipalType::Tool,
                permission,
            },
        )
        .await;
}

async fn deliver_tool_permission_if_auto(
    agent: &Agent,
    session_id: &str,
    permission_modes: &SessionPermissionModes,
    request_id: &str,
    cancel_token: &CancellationToken,
) -> bool {
    // Keep the policy lock through confirmation delivery. This is the
    // linearization point for Auto -> Read only: once the restrictive mode
    // command returns, no permission decision based on an older Auto snapshot
    // can still be delivered.
    let modes = permission_modes.lock().await;
    if modes
        .get(session_id)
        .copied()
        .unwrap_or(GOOSE_PERMISSION_ROUTING_MODE)
        != GooseMode::Auto
    {
        return false;
    }
    let permission = if cancel_token.is_cancelled() {
        Permission::Cancel
    } else {
        Permission::AllowOnce
    };
    deliver_tool_permission(agent, request_id.to_string(), permission).await;
    drop(modes);
    true
}

async fn claim_pending_permission_if_auto(
    agent: &Agent,
    session_id: &str,
    permission_modes: &SessionPermissionModes,
    pending_permissions: &PendingPermissions,
    request_id: &str,
    cancel_token: &CancellationToken,
) -> bool {
    // This is the same Auto -> Read only linearization boundary as the direct
    // path above, with the pending request claimed while the policy is locked.
    let modes = permission_modes.lock().await;
    if modes
        .get(session_id)
        .copied()
        .unwrap_or(GOOSE_PERMISSION_ROUTING_MODE)
        != GooseMode::Auto
    {
        return false;
    }
    let claimed = pending_permissions
        .lock()
        .await
        .remove(&(session_id.to_string(), request_id.to_string()))
        .is_some();
    if claimed {
        let permission = if cancel_token.is_cancelled() {
            Permission::Cancel
        } else {
            Permission::AllowOnce
        };
        deliver_tool_permission(agent, request_id.to_string(), permission).await;
    }
    drop(modes);
    true
}

struct PermissionAutomationContext<'a> {
    permission_modes: &'a SessionPermissionModes,
    web_tool_state: &'a WebToolState,
    web_permission_context: &'a WebPermissionContext,
    working_dir: &'a Path,
    cancel_token: &'a CancellationToken,
}

async fn automatically_handle_permissions(
    agent: &Agent,
    session_id: &str,
    message: &Message,
    context: PermissionAutomationContext<'_>,
) -> HashSet<String> {
    let PermissionAutomationContext {
        permission_modes,
        web_tool_state,
        web_permission_context,
        working_dir,
        cancel_token,
    } = context;
    let shell_classifier = ShellPermissionClassifier;
    let web_classifier = WebPermissionClassifier;
    let mut handled = HashSet::new();

    for content in &message.content {
        let MessageContent::ActionRequired(action) = content else {
            continue;
        };
        let tool_request_id = match &action.data {
            ActionRequiredData::ToolConfirmation { id, .. } => Some(id.clone()),
            _ => None,
        };
        if let Some(request_id) = tool_request_id.as_ref()
            && deliver_tool_permission_if_auto(
                agent,
                session_id,
                permission_modes,
                request_id,
                cancel_token,
            )
            .await
        {
            let request_id = request_id.clone();
            handled.insert(request_id);
            continue;
        }
        let current_mode = selected_permission_mode(permission_modes, session_id)
            .await
            .to_string();
        if let Some(request) =
            OpenUrlPermissionRequest::from_action(&current_mode, action, web_permission_context)
        {
            let request_id = request.request_id().to_string();
            let outcome = if cancel_token.is_cancelled() {
                WebPermissionOutcome::Cancelled
            } else if web_tool_state
                .contains_search_url(session_id, request.url())
                .await
            {
                log::info!("Auto-approved search-derived Agent Mode URL request {request_id}");
                WebPermissionOutcome::AllowOnce
            } else {
                web_classifier
                    .classify(agent, session_id, &request, cancel_token)
                    .await
            };
            if deliver_tool_permission_if_auto(
                agent,
                session_id,
                permission_modes,
                &request_id,
                cancel_token,
            )
            .await
            {
                handled.insert(request_id);
                continue;
            }
            let permission = if cancel_token.is_cancelled() {
                Permission::Cancel
            } else {
                match outcome {
                    WebPermissionOutcome::AllowOnce => Permission::AllowOnce,
                    WebPermissionOutcome::Cancelled => Permission::Cancel,
                    WebPermissionOutcome::RequiresApproval => continue,
                }
            };
            deliver_tool_permission(agent, request_id.clone(), permission).await;
            handled.insert(request_id);
            continue;
        }
        if let Some(request_id) = local_read_request_id(&current_mode, action)
            .or_else(|| local_read_image_request_id(&current_mode, action))
            .map(str::to_string)
        {
            let permission = if cancel_token.is_cancelled() {
                Permission::Cancel
            } else {
                log::info!("Auto-approved local Agent Mode file read request {request_id}");
                Permission::AllowOnce
            };
            deliver_tool_permission(agent, request_id.clone(), permission).await;
            handled.insert(request_id);
            continue;
        }
        let Some(request) = ShellPermissionRequest::from_action(&current_mode, working_dir, action)
        else {
            if let Some(request_id) = tool_request_id
                && deliver_tool_permission_if_auto(
                    agent,
                    session_id,
                    permission_modes,
                    &request_id,
                    cancel_token,
                )
                .await
            {
                handled.insert(request_id);
            }
            continue;
        };
        let request_id = request.request_id().to_string();
        let outcome = shell_classifier
            .classify(agent, session_id, &request, cancel_token)
            .await;
        if deliver_tool_permission_if_auto(
            agent,
            session_id,
            permission_modes,
            &request_id,
            cancel_token,
        )
        .await
        {
            handled.insert(request_id);
            continue;
        }
        let permission = if cancel_token.is_cancelled() {
            Permission::Cancel
        } else {
            match outcome {
                ShellPermissionOutcome::ReadOnly => {
                    log::info!("Auto-approved read-only Agent Mode shell request {request_id}");
                    Permission::AllowOnce
                }
                ShellPermissionOutcome::Cancelled => Permission::Cancel,
                ShellPermissionOutcome::RequiresApproval => continue,
            }
        };

        deliver_tool_permission(agent, request_id.clone(), permission).await;
        handled.insert(request_id);
    }

    handled
}

struct ExtractedToolPermissionRequests {
    requests: HashMap<String, AgentPermissionRequest>,
    conflicting_ids: HashSet<String>,
}

fn tool_permission_requests(message: &Message) -> ExtractedToolPermissionRequests {
    let mut requests = HashMap::new();
    let mut conflicting_ids = HashSet::new();
    for content in &message.content {
        let MessageContent::ActionRequired(action) = content else {
            continue;
        };
        let ActionRequiredData::ToolConfirmation {
            id,
            tool_name,
            arguments,
            prompt,
        } = &action.data
        else {
            continue;
        };
        if id.trim().is_empty() {
            conflicting_ids.insert(id.clone());
            continue;
        }
        let request = AgentPermissionRequest {
            request_id: id.clone(),
            tool_name: tool_name.clone(),
            arguments: arguments.clone(),
            prompt: prompt.clone(),
        };
        if conflicting_ids.contains(id) {
            continue;
        }
        match requests.get(id) {
            Some(_) => {
                // A request ID is a one-shot capability. Even byte-for-byte
                // duplicate entries in the same Goose message are ambiguous:
                // registering one and suppressing the other can accidentally
                // suppress the only caller-visible prompt. Fail closed instead.
                requests.remove(id);
                conflicting_ids.insert(id.clone());
            }
            None => {
                requests.insert(id.clone(), request);
            }
        }
    }
    ExtractedToolPermissionRequests {
        requests,
        conflicting_ids,
    }
}

/// The `delegate` calls of one run, so the desktop can show which
/// subagents are working now.
///
/// Everything is keyed by the request ID of the `delegate` call, which is
/// also the ID of its timeline row.
#[derive(Default)]
struct SubagentTracker {
    /// Subagents that have not reported a result yet.
    running: HashMap<String, RunningSubagent>,
    /// Background task ID -> the `delegate` request that started it. An
    /// async `delegate` returns at once while its subagent keeps working.
    background: HashMap<String, String>,
    /// `load` request ID -> the background task it collects.
    loads: HashMap<String, LoadedSubagent>,
    /// Background tasks that have started but have no watcher yet, as
    /// `(delegate request ID, Goose task ID)`.
    unwatched: Vec<(String, String)>,
}

struct RunningSubagent {
    /// What the subagent was asked to do.
    task: String,
    background: bool,
    started: std::time::Instant,
    /// The tool it called most recently.
    activity: Option<String>,
}

struct LoadedSubagent {
    delegate_id: String,
    /// `peek` reads progress only; the subagent keeps working.
    peek: bool,
}

impl SubagentTracker {
    /// The task asked for a subagent, or asked for the result of one.
    fn tool_request(
        &mut self,
        id: &str,
        name: &str,
        arguments: Option<&JsonObject>,
    ) -> Option<AgentRunEvent> {
        match name {
            SUBAGENT_DELEGATE_TOOL => {
                // Goose repeats a split tool-request message; the first
                // one owns the row.
                if self.running.contains_key(id) {
                    return None;
                }
                let background = subagent_flag(arguments, "async");
                let task = subagent_task_label(arguments);
                self.running.insert(
                    id.to_string(),
                    RunningSubagent {
                        task: task.clone(),
                        background,
                        started: std::time::Instant::now(),
                        activity: None,
                    },
                );
                Some(AgentRunEvent::SubagentStarted {
                    id: id.to_string(),
                    task,
                    background,
                })
            }
            SUBAGENT_LOAD_TOOL => {
                let source = subagent_argument(arguments, "source")?;
                let delegate_id = self.background.get(source)?.clone();
                self.loads.insert(
                    id.to_string(),
                    LoadedSubagent {
                        delegate_id,
                        peek: subagent_flag(arguments, "peek"),
                    },
                );
                None
            }
            _ => None,
        }
    }

    /// A tool call returned. `result` is `None` when the call failed.
    fn tool_response(
        &mut self,
        id: &str,
        result: Option<&CallToolResult>,
    ) -> Option<AgentRunEvent> {
        if let Some(load) = self.loads.remove(id) {
            if load.peek {
                return None;
            }
            return self.finish(&load.delegate_id);
        }
        let running = self.running.get(id)?;
        // An async `delegate` returns while its subagent works on. Keep
        // the row and follow the task by the ID Goose reports back.
        if running.background
            && let Some(task_id) = result.and_then(background_task_id)
        {
            self.background.insert(task_id.clone(), id.to_string());
            self.unwatched.push((id.to_string(), task_id));
            return None;
        }
        self.finish(id)
    }

    /// The subagent of `request_id` called a tool. A background subagent
    /// reports through the `load` call that is waiting for it.
    fn notification(
        &mut self,
        request_id: &str,
        notification: &ServerNotification,
    ) -> Option<AgentRunEvent> {
        let id = match self.loads.get(request_id) {
            Some(load) => load.delegate_id.clone(),
            None if self.running.contains_key(request_id) => request_id.to_string(),
            None => return None,
        };
        let tool = subagent_notification_tool(notification)?;
        if let Some(running) = self.running.get_mut(&id) {
            running.activity = Some(tool.clone());
        }
        Some(AgentRunEvent::SubagentActivity { id, tool })
    }

    /// The subagents of this task that are still working, for a caller
    /// that comes to the task after their run ended.
    fn snapshot(&self) -> Vec<AgentSubagent> {
        let now = std::time::Instant::now();
        let mut subagents = self
            .running
            .iter()
            .map(|(id, running)| AgentSubagent {
                id: id.clone(),
                task: running.task.clone(),
                background: running.background,
                elapsed_ms: now
                    .saturating_duration_since(running.started)
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
                activity: running.activity.clone(),
            })
            .collect::<Vec<_>>();
        // The map has no order of its own; the oldest subagent reads first.
        subagents.sort_by_key(|subagent| std::cmp::Reverse(subagent.elapsed_ms));
        subagents
    }

    /// A run keeps no subagent past its own end, but a background task
    /// works on and is collected by a later run of the same task.
    fn retain_background(&mut self) {
        self.running.retain(|_, running| running.background);
        self.loads.clear();
        self.background
            .retain(|_, delegate_id| self.running.contains_key(delegate_id));
    }

    /// Background tasks that started since the last call. Nothing pushes
    /// their end back, so each one needs a watcher.
    fn take_unwatched(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.unwatched)
    }

    /// Whether this task still shows the subagent of `delegate_id`.
    fn is_watching(&self, delegate_id: &str) -> bool {
        self.running.contains_key(delegate_id)
    }

    fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    fn finish(&mut self, id: &str) -> Option<AgentRunEvent> {
        self.running.remove(id)?;
        self.background.retain(|_, delegate_id| delegate_id != id);
        Some(AgentRunEvent::SubagentFinished { id: id.to_string() })
    }
}

/// How often Maple asks Goose whether a background subagent has ended.
const SUBAGENT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Everything one background subagent watcher needs to report an end.
struct BackgroundSubagentWatch {
    service: MapleAgentService,
    account_scope: Arc<str>,
    /// Weak, so a watcher cannot keep a stopped runtime's Agent alive.
    agent: std::sync::Weak<Agent>,
    session_manager: Arc<SessionManager>,
    subagents: SessionSubagents,
    events: AgentEventDispatcher,
    session_id: String,
    run_id: String,
    /// Request ID of the `delegate` call, which is the card's row ID.
    delegate_id: String,
    /// The ID Goose gave the background task.
    task_id: String,
    working_dir: PathBuf,
    /// Whether this surface's events reach Maple Desktop. An ACP caller
    /// keeps its run stream to itself; the desktop reads the same end
    /// from the persisted history when it opens the task.
    host_events: AgentHostEventPolicy,
}

/// Tell the task when its background subagent ends.
///
/// A background `delegate` returns at once and Goose pushes nothing back
/// when the subagent finishes, so the model would learn of it only if the
/// user asked again. Poll `load(peek)`, which reports status and never
/// consumes the result, then report the end into the task: steered into
/// the turn that is running, or left in the history for the next one.
async fn watch_background_subagent(watch: BackgroundSubagentWatch) {
    let status = loop {
        tokio::time::sleep(SUBAGENT_POLL_INTERVAL).await;
        // A stopped runtime drops its agents, and with them the subagent.
        let Some(agent) = watch.agent.upgrade() else {
            return;
        };
        // The model may have collected the result itself, or the task may
        // be gone. Either way the row is closed and nobody needs a notice.
        if !subagent_is_watched(&watch.subagents, &watch.session_id, &watch.delegate_id).await {
            return;
        }
        match background_subagent_status(&agent, &watch).await {
            Some(status) => break status,
            None => continue,
        }
    };

    if !finish_watched_subagent(&watch.subagents, &watch.session_id, &watch.delegate_id).await {
        // A `load` in a turn beat the watcher to it; the model has the
        // result already.
        return;
    }
    if watch.host_events.publishes() {
        emit_agent_event(
            &watch.events,
            AgentServiceEvent::Run {
                session_id: watch.session_id.clone(),
                run_id: watch.run_id.clone(),
                event: AgentRunEvent::SubagentFinished {
                    id: watch.delegate_id.clone(),
                },
            },
        );
    }
    report_background_subagent_end(&watch, &status).await;
}

/// The status Goose reports for a background task, or `None` while it
/// still runs. A task Goose no longer knows reads as ended.
async fn background_subagent_status(
    agent: &Arc<Agent>,
    watch: &BackgroundSubagentWatch,
) -> Option<String> {
    let context = ToolCallContext::new(
        watch.session_id.clone(),
        Some(watch.working_dir.clone()),
        None,
    );
    let call = rmcp::model::CallToolRequestParams::new(SUBAGENT_LOAD_TOOL.to_string())
        .with_arguments(rmcp::object!({ "source": watch.task_id.clone(), "peek": true }));
    let dispatched = agent
        .extension_manager
        .dispatch_tool_call(&context, call, CancellationToken::new())
        .await;
    let Ok(dispatched) = dispatched else {
        return Some("gone".to_string());
    };
    let Ok(result) = dispatched.result.await else {
        return Some("gone".to_string());
    };
    let status = result
        .meta
        .as_ref()
        .and_then(|meta| meta.0.get("task_status"))
        .and_then(Value::as_str)?;
    (status != "running").then(|| status.to_string())
}

/// Put the end of a background subagent where the model will read it, and
/// leave a row in the transcript for the user.
async fn report_background_subagent_end(watch: &BackgroundSubagentWatch, status: &str) {
    let outcome = match status {
        "completed" => "finished",
        "gone" => "is no longer available",
        _ => "failed",
    };
    let for_model = Message::user()
        .with_text(format!(
            "The background subagent of task {} {outcome}. Call load(source: \"{}\") to read its result, then carry on.",
            watch.task_id, watch.task_id
        ))
        // The user did not write this; it belongs to the model's view of
        // the conversation only.
        .with_visibility(false, true)
        .with_generated_id();
    // A turn that is still running can act on this at its next step. With
    // no turn to steer, the history carries it into the next one.
    if steer_into_desktop_run(
        &watch.service,
        &watch.account_scope,
        &watch.session_id,
        &for_model,
    )
    .await
    .is_err()
        && let Err(error) = watch
            .session_manager
            .add_message(&watch.session_id, &for_model)
            .await
    {
        log::warn!("Failed to record the end of a background subagent: {error}");
        return;
    }

    let notice = Message::assistant()
        .with_system_notification(
            SystemNotificationType::InlineMessage,
            format!("Background subagent {outcome}"),
        )
        .with_visibility(true, false)
        .with_generated_id();
    if let Err(error) = watch
        .session_manager
        .add_message(&watch.session_id, &notice)
        .await
    {
        log::warn!("Failed to record a background subagent notice: {error}");
        return;
    }
    if !watch.host_events.publishes() {
        return;
    }
    for item in message_to_timeline_items(&notice, false) {
        emit_agent_event(
            &watch.events,
            AgentServiceEvent::TimelineItem {
                session_id: watch.session_id.clone(),
                run_id: None,
                item,
            },
        );
    }
}

async fn subagent_is_watched(
    subagents: &SessionSubagents,
    session_id: &str,
    delegate_id: &str,
) -> bool {
    subagents
        .lock()
        .await
        .get(session_id)
        .is_some_and(|tracker| tracker.is_watching(delegate_id))
}

/// Close a watched row. `false` when something else closed it first.
async fn finish_watched_subagent(
    subagents: &SessionSubagents,
    session_id: &str,
    delegate_id: &str,
) -> bool {
    let mut trackers = subagents.lock().await;
    let Some(tracker) = trackers.get_mut(session_id) else {
        return false;
    };
    let finished = tracker.finish(delegate_id).is_some();
    if tracker.is_empty() {
        trackers.remove(session_id);
    }
    finished
}

/// Apply `work` to this task's subagent tracker.
///
/// The map holds an entry only while a task has a subagent, so a task
/// that never delegates costs one lookup.
async fn track_subagents<T>(
    subagents: &SessionSubagents,
    session_id: &str,
    work: impl FnOnce(&mut SubagentTracker) -> T,
) -> T {
    let mut trackers = subagents.lock().await;
    let tracker = trackers.entry(session_id.to_string()).or_default();
    let result = work(tracker);
    if tracker.is_empty() {
        trackers.remove(session_id);
    }
    result
}

/// End every subagent of a run that stopped. A background subagent works
/// on, and a later run of the same task collects it with `load`.
async fn end_run_subagents(subagents: &SessionSubagents, session_id: &str) {
    let mut trackers = subagents.lock().await;
    let Some(tracker) = trackers.get_mut(session_id) else {
        return;
    };
    tracker.retain_background();
    if tracker.is_empty() {
        trackers.remove(session_id);
    }
}

/// Read one Goose message for subagent starts and ends.
fn subagent_events(tracker: &mut SubagentTracker, message: &Message) -> Vec<AgentRunEvent> {
    let mut events = Vec::new();
    for content in &message.content {
        let event = match content {
            MessageContent::ToolRequest(request) => match &request.tool_call {
                Ok(call) => {
                    tracker.tool_request(&request.id, call.name.as_ref(), call.arguments.as_ref())
                }
                Err(_) => None,
            },
            MessageContent::ToolResponse(response) => {
                tracker.tool_response(&response.id, response.tool_result.as_ref().ok())
            }
            _ => None,
        };
        events.extend(event);
    }
    events
}

fn subagent_argument<'a>(arguments: Option<&'a JsonObject>, key: &str) -> Option<&'a str> {
    arguments?
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn subagent_flag(arguments: Option<&JsonObject>, key: &str) -> bool {
    arguments
        .and_then(|arguments| arguments.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// What the subagent was asked to do, for the card above the composer.
fn subagent_task_label(arguments: Option<&JsonObject>) -> String {
    let detail = subagent_argument(arguments, "source")
        .or_else(|| subagent_argument(arguments, "instructions"));
    let Some(detail) = detail else {
        return "Delegated task".to_string();
    };
    let first_line = detail.lines().next().unwrap_or(detail).trim();
    bounded_timeline_text(first_line, MAX_AGENT_SESSION_TITLE_CHARS)
}

/// The ID Goose gives a background subagent, from the result of the
/// `delegate` call that started it.
fn background_task_id(result: &CallToolResult) -> Option<String> {
    if result.is_error.unwrap_or(false) {
        return None;
    }
    Some(
        result
            .meta
            .as_ref()?
            .0
            .get("subagent_session_id")?
            .as_str()?
            .to_string(),
    )
}

/// Whether an MCP notification carries a subagent tool call. This is the
/// cheap shape check the run loop applies before it takes the tracker
/// lock; `subagent_notification_tool` does the full read.
#[allow(deprecated)]
fn is_subagent_notification(notification: &ServerNotification) -> bool {
    let ServerNotification::LoggingMessageNotification(message) = notification else {
        return false;
    };
    message
        .params
        .data
        .as_object()
        .and_then(|data| data.get("type"))
        .and_then(Value::as_str)
        == Some(SUBAGENT_TOOL_REQUEST_TYPE)
}

/// The tool a subagent called, from the notification Goose forwards while
/// the parent waits.
#[allow(deprecated)]
fn subagent_notification_tool(notification: &ServerNotification) -> Option<String> {
    let ServerNotification::LoggingMessageNotification(message) = notification else {
        return None;
    };
    let data = message.params.data.as_object()?;
    if data.get("type").and_then(Value::as_str) != Some(SUBAGENT_TOOL_REQUEST_TYPE) {
        return None;
    }
    let call = data.get("tool_call")?.as_object()?;
    let name = call.get("name")?.as_str()?;
    let arguments = call.get("arguments").cloned().unwrap_or(Value::Null);
    Some(descriptive_tool_title(name, &arguments).unwrap_or_else(|| friendly_tool_label(name)))
}

async fn run_agent_prompt(run: AgentPromptRun) -> Result<AgentPromptOutcome, String> {
    let AgentPromptRun {
        events,
        agent,
        session_manager,
        session_title_lifecycle,
        live_timelines,
        subagents,
        subagent_host,
        session_id,
        user_message,
        permission_modes,
        web_tool_state,
        web_permission_context,
        cancel_token,
        session_title_start,
        pending_permissions,
        issued_permission_ids,
        cancelled_permission_ids,
        run_id,
        permission_routing,
        steered_unacked,
    } = run;
    let mut terminal_message = None;
    let session_config = SessionConfig {
        id: session_id.clone(),
        schedule_id: None,
        max_turns: None,
        retry_config: None,
    };
    let mut stream = agent
        .reply(user_message, session_config, Some(cancel_token.clone()))
        .await
        .map_err(|e| format!("Goose reply failed: {e}"))?;
    let session_title_lifecycle_guard = session_title_lifecycle.lock().await;
    let updated_session = session_manager
        .get_session(&session_id, false)
        .await
        .map_err(|e| format!("Failed to load updated Agent task: {e}"))?;
    let working_dir = updated_session.working_dir.clone();
    events
        .publish(AgentRunEvent::SessionUpdated(session_summary(
            &updated_session,
        )))
        .await;
    drop(session_title_lifecycle_guard);
    if let Some(start) = session_title_start {
        // Goose has persisted the first user message and Maple has published
        // the fallback snapshot. A semantic title can now advance that state,
        // but can never be followed by a stale fallback event from this run.
        let _ = start.send(());
    }

    let mut prompt_error = None;

    loop {
        let next_event = stream.next().await;
        // The Maple provider records this immediately before returning its
        // final auth/secure-connection error. Consume it before Goose's
        // synthetic assistant message can enter the task timeline.
        if let Some(error) = provider::take_terminal_run_error() {
            prompt_error = Some(error);
            break;
        }
        let Some(event) = next_event else {
            break;
        };
        match event {
            Ok(AgentEvent::Message(message)) => {
                let extracted_permissions = tool_permission_requests(&message);
                if !extracted_permissions.conflicting_ids.is_empty() {
                    for request_id in &extracted_permissions.conflicting_ids {
                        deliver_tool_permission(&agent, request_id.clone(), Permission::Cancel)
                            .await;
                    }
                    prompt_error = Some(format!(
                        "Goose emitted an empty or conflicting permission request ID: {}",
                        extracted_permissions
                            .conflicting_ids
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                    break;
                }
                let permission_requests = extracted_permissions.requests;
                let automatically_handled = automatically_handle_permissions(
                    &agent,
                    &session_id,
                    &message,
                    PermissionAutomationContext {
                        permission_modes: &permission_modes,
                        web_tool_state: &web_tool_state,
                        web_permission_context: &web_permission_context,
                        working_dir: &working_dir,
                        cancel_token: &cancel_token,
                    },
                )
                .await;
                if let Some(error) = provider::take_terminal_run_error() {
                    prompt_error = Some(error);
                    break;
                }
                if cancel_token.is_cancelled() && !automatically_handled.is_empty() {
                    cancelled_permission_ids
                        .lock()
                        .await
                        .extend(automatically_handled.iter().cloned());
                }
                // Steered user rows are emitted when queued. Goose later yields
                // the same complete message; replace it instead of appending.
                if message_role(&message) == "user" {
                    ack_steered_message(&steered_unacked, &message).await;
                }
                let live = message_role(&message) != "user";
                let mut items = message_to_timeline_items(&message, live);
                items.retain(|item| {
                    pending_permission_request_id(item)
                        .is_none_or(|request_id| !automatically_handled.contains(&request_id))
                });
                let mut newly_auto_handled = HashSet::new();
                let mut duplicate_permissions = HashSet::new();
                for item in &mut items {
                    if let Some(request_id) = pending_permission_request_id(item) {
                        let Some(request) = permission_requests.get(&request_id).cloned() else {
                            cancelled_permission_ids
                                .lock()
                                .await
                                .insert(request_id.clone());
                            deliver_tool_permission(&agent, request_id, Permission::Cancel).await;
                            item.status = Some("cancelled".to_string());
                            continue;
                        };
                        match register_pending_permission(
                            &pending_permissions,
                            &issued_permission_ids,
                            &session_id,
                            &run_id,
                            permission_routing,
                            request,
                            &cancel_token,
                        )
                        .await
                        {
                            PendingPermissionRegistration::Rejected => {
                                cancelled_permission_ids
                                    .lock()
                                    .await
                                    .insert(request_id.clone());
                                deliver_tool_permission(&agent, request_id, Permission::Cancel)
                                    .await;
                                item.status = Some("cancelled".to_string());
                            }
                            PendingPermissionRegistration::Existing => {
                                duplicate_permissions.insert(request_id);
                            }
                            PendingPermissionRegistration::Registered => {
                                if claim_pending_permission_if_auto(
                                    &agent,
                                    &session_id,
                                    &permission_modes,
                                    &pending_permissions,
                                    &request_id,
                                    &cancel_token,
                                )
                                .await
                                {
                                    if cancel_token.is_cancelled() {
                                        cancelled_permission_ids
                                            .lock()
                                            .await
                                            .insert(request_id.clone());
                                    }
                                    newly_auto_handled.insert(request_id);
                                }
                            }
                        }
                    }
                }
                items.retain(|item| {
                    pending_permission_request_id(item).is_none_or(|request_id| {
                        !newly_auto_handled.contains(&request_id)
                            && !duplicate_permissions.contains(&request_id)
                    })
                });
                // Publish a permission card while holding the same claim lock
                // used by an Allow-all transition. If that transition already
                // drained the request, suppress the now-non-actionable card; if
                // this path wins, the transition will immediately replace the
                // published card with its allowed status.
                let pending_publication_guard = if items
                    .iter()
                    .any(|item| pending_permission_request_id(item).is_some())
                {
                    Some(pending_permissions.lock().await)
                } else {
                    None
                };
                if let Some(pending) = pending_publication_guard.as_ref() {
                    items.retain(|item| {
                        pending_permission_request_id(item).is_none_or(|request_id| {
                            pending.contains_key(&(session_id.clone(), request_id))
                        })
                    });
                }
                if !items.is_empty() {
                    terminal_message = Some(update_live_message_candidate(
                        terminal_message,
                        &message,
                        &items,
                    ));
                }
                for item in items {
                    if let Some(request_id) = pending_permission_request_id(&item)
                        && let Some(request) = permission_requests.get(&request_id)
                    {
                        record_timeline_item(
                            &live_timelines,
                            &session_id,
                            permission_routing,
                            item.clone(),
                        )
                        .await;
                        events
                            .publish(AgentRunEvent::PermissionRequested {
                                request: request.clone(),
                                item,
                            })
                            .await;
                        continue;
                    }
                    record_and_emit_timeline_item(
                        &events,
                        &live_timelines,
                        &session_id,
                        permission_routing,
                        item,
                    )
                    .await;
                }
                drop(pending_publication_guard);
                // Subagent events name the `delegate` tool call that owns
                // them, so they follow the timeline row that opens it.
                // Only a message with tool traffic can move the tracker;
                // plain streamed text skips the lock.
                let has_tool_content = message.content.iter().any(|content| {
                    matches!(
                        content,
                        MessageContent::ToolRequest(_) | MessageContent::ToolResponse(_)
                    )
                });
                let (subagent_updates, started_in_background) = if has_tool_content {
                    track_subagents(&subagents, &session_id, |tracker| {
                        (subagent_events(tracker, &message), tracker.take_unwatched())
                    })
                    .await
                } else {
                    (Vec::new(), Vec::new())
                };
                for event in subagent_updates {
                    events.publish(event).await;
                }
                if let Some((service, account_scope)) = subagent_host.as_ref() {
                    for (delegate_id, task_id) in started_in_background {
                        tokio::spawn(watch_background_subagent(BackgroundSubagentWatch {
                            service: service.clone(),
                            account_scope: Arc::clone(account_scope),
                            agent: Arc::downgrade(&agent),
                            session_manager: Arc::clone(&session_manager),
                            subagents: Arc::clone(&subagents),
                            events: events.dispatcher.clone(),
                            session_id: session_id.clone(),
                            run_id: run_id.clone(),
                            delegate_id,
                            task_id,
                            working_dir: working_dir.clone(),
                            host_events: events.host_events,
                        }));
                    }
                }
            }
            // Usage ledgers remain in Goose's persisted messages for context
            // accounting, but Agent Mode does not render ephemeral token rows.
            Ok(AgentEvent::Usage(_) | AgentEvent::MessageUsage { .. }) => {}
            // Developer/MCP notifications are transport diagnostics. Tool
            // requests, results, permissions, and failures arrive as messages
            // and form the stable user-facing timeline. The one exception is
            // a subagent, whose tool calls reach the parent only here.
            Ok(AgentEvent::McpNotification((request_id, notification))) => {
                // MCP servers are chatty; check the shape before taking
                // the tracker lock, so only subagent traffic pays for it.
                if is_subagent_notification(&notification) {
                    let event = track_subagents(&subagents, &session_id, |tracker| {
                        tracker.notification(&request_id, &notification)
                    })
                    .await;
                    if let Some(event) = event {
                        events.publish(event).await;
                    }
                }
            }
            Ok(AgentEvent::HistoryReplaced(conversation)) => {
                terminal_message = None;
                reseed_live_timeline_after_history_replaced(
                    &live_timelines,
                    &session_id,
                    permission_routing,
                    &conversation,
                )
                .await;
                events.publish(AgentRunEvent::HistoryReplaced).await;
            }
            Err(error) => {
                prompt_error = Some(format!("Goose stream failed: {error}"));
                break;
            }
        }
        // Keep polling Goose after cancellation. Goose observes the same token,
        // stops provider/tool work, and then commits any complete message/tool
        // batch before its stream ends. Dropping the stream here would discard
        // that standard durable boundary even after a completed result event.
    }

    if let Some(error) = prompt_error {
        return Err(error);
    }

    Ok(AgentPromptOutcome { terminal_message })
}

fn live_message_candidate(message: &Message, items: &[AgentTimelineItem]) -> LiveMessageCandidate {
    LiveMessageCandidate {
        id: message.id.clone(),
        role: message_role(message),
        created: message.created,
        items: coalesce_timeline_items(items.to_vec()),
    }
}

fn update_live_message_candidate(
    current: Option<LiveMessageCandidate>,
    message: &Message,
    items: &[AgentTimelineItem],
) -> LiveMessageCandidate {
    let role = message_role(message);
    // Provider stream chunks have a stable ID. Id-less Goose messages are
    // complete logical events and may share the same second-resolution
    // timestamp, so combining them would conflate a reply with a later notice.
    let Some(mut current) = current.filter(|current| {
        current.id.is_some()
            && current.id == message.id
            && current.role == role
            && current.items.iter().all(|item| item.item_type != "system")
            && items.iter().all(|item| item.item_type != "system")
    }) else {
        return live_message_candidate(message, items);
    };

    for item in items {
        current.items = merge_timeline_item(current.items, item.clone());
    }
    current
}

fn timeline_item_matches(
    live: &AgentTimelineItem,
    persisted: &AgentTimelineItem,
    match_id: bool,
) -> bool {
    (!match_id || live.id == persisted.id)
        && live.item_type == persisted.item_type
        && live.role == persisted.role
        && live.title == persisted.title
        && live.text == persisted.text
        && live.status == persisted.status
        && live.input == persisted.input
        && live.output == persisted.output
}

fn terminal_message_is_persisted(
    conversation: &Conversation,
    candidate: &LiveMessageCandidate,
) -> bool {
    let messages = conversation.messages();
    let current_turn_start = messages
        .iter()
        .rposition(|message| {
            let role = message_role(message);
            is_real_user_message(message, &role)
        })
        .unwrap_or(0);
    let turn_messages = &messages[current_turn_start..];
    if let Some(id) = candidate.id.as_deref() {
        let mut persisted_items = Vec::new();
        for message in turn_messages.iter().filter(|message| {
            message_role(message) == candidate.role && message.id.as_deref() == Some(id)
        }) {
            for item in message_to_timeline_items(message, true) {
                persisted_items = merge_timeline_item(persisted_items, item);
            }
        }
        return timeline_projection_matches(&candidate.items, &persisted_items, true);
    }

    turn_messages
        .iter()
        .filter(|message| {
            message_role(message) == candidate.role && message.created == candidate.created
        })
        .any(|message| {
            let persisted_items = coalesce_timeline_items(message_to_timeline_items(message, true));
            timeline_projection_matches(&candidate.items, &persisted_items, false)
        })
}

fn timeline_projection_matches(
    live: &[AgentTimelineItem],
    persisted: &[AgentTimelineItem],
    match_id: bool,
) -> bool {
    live.len() == persisted.len()
        && live
            .iter()
            .zip(persisted)
            .all(|(live, persisted)| timeline_item_matches(live, persisted, match_id))
}

fn bounded_timeline_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

fn pending_permission_request_id(item: &AgentTimelineItem) -> Option<String> {
    if item.item_type == "permission" {
        return item
            .id
            .strip_prefix("permission-")
            .filter(|request_id| !request_id.is_empty())
            .map(ToString::to_string);
    }
    None
}

fn project_is_trusted(paths: &AgentPathLayout, user_id: &str, project_root: &Path) -> bool {
    match load_agent_config_inner(paths, user_id) {
        Ok(config) => project_trust_status(&config, project_root, true).decision == Some(true),
        Err(error) => {
            log::warn!(
                "Failed to load Agent Mode project trust; keeping project-provided capabilities disabled: {error}"
            );
            false
        }
    }
}

fn project_skills_root_is_available(project_root: &Path) -> bool {
    let Ok(canonical) = project_root.canonicalize() else {
        return false;
    };
    canonical == project_root && canonical.is_dir() && fs::read_dir(canonical).is_ok()
}

struct SkillsDiscoveryScope {
    working_dir: PathBuf,
    blocked_project_root: Option<PathBuf>,
}

fn skills_discovery_scope(
    paths: &AgentPathLayout,
    user_id: &str,
    session: &Session,
) -> Result<SkillsDiscoveryScope, String> {
    if project_is_trusted(paths, user_id, &session.working_dir) {
        if project_skills_root_is_available(&session.working_dir) {
            return Ok(SkillsDiscoveryScope {
                working_dir: session.working_dir.clone(),
                blocked_project_root: None,
            });
        }
        log::warn!(
            "Trusted project skills folder is unavailable; keeping project skills disabled: {}",
            session.working_dir.display()
        );
    }

    let root = agent_config_dir(paths, user_id)
        .map_err(|error| format!("Failed to locate Maple skills data: {error}"))?
        .join("untrusted-project-skills");
    fs::create_dir_all(&root)
        .map_err(|error| format!("Failed to create Maple skills data directory: {error}"))?;
    set_owner_only_dir_permissions(&root);
    Ok(SkillsDiscoveryScope {
        working_dir: root,
        blocked_project_root: Some(session.working_dir.clone()),
    })
}

async fn detach_transient_skills_client(agent: &Agent) {
    let _ = agent
        .extension_manager
        .remove_extension(MAPLE_SKILLS_CLIENT_KEY)
        .await;
}

fn maple_skills_extension_config() -> ExtensionConfig {
    ExtensionConfig::Platform {
        name: SKILLS_EXTENSION_NAME.to_string(),
        description: "Discover and load agent skills from the local filesystem".to_string(),
        display_name: Some("Maple Skills Extension".to_string()),
        bundled: Some(true),
        available_tools: MAPLE_SKILLS_TOOLS
            .iter()
            .map(|tool| tool.to_string())
            .collect(),
    }
}

/// Goose's `summon` extension, which owns `delegate` and `load`.
///
/// Goose builds the client itself from its platform registry, so the
/// subagent it starts inherits this task's provider and its enabled MCP
/// servers.
///
/// The pinned Goose fork forwards child ActionRequired messages and inherits
/// the parent's permission mode. Child tools are constructed by Goose's own
/// factory; the parent's injected Maple developer client and its task-bound
/// Python capability are not copied. Delegation remains ask-before in Maple.
fn maple_subagent_extension_config() -> ExtensionConfig {
    ExtensionConfig::Platform {
        name: SUMMON_EXTENSION_NAME.to_string(),
        description: "Delegate a task to a subagent that runs on its own".to_string(),
        display_name: Some("Subagents".to_string()),
        bundled: Some(true),
        available_tools: MAPLE_SUBAGENT_TOOLS
            .iter()
            .map(|tool| tool.to_string())
            .collect(),
    }
}

fn skills_client_for_working_dir(
    agent: &Arc<Agent>,
    session: &Session,
    working_dir: PathBuf,
) -> Result<SkillsClient, String> {
    let mut skills_session = session.clone();
    skills_session.working_dir = working_dir;
    let mut skills_context = agent.extension_manager.get_context().clone();
    skills_context.extension_manager = Some(Arc::downgrade(&agent.extension_manager));
    skills_context.session = Some(Arc::new(skills_session));
    SkillsClient::new(skills_context)
        .map(|client| client.with_builtin_skills(false))
        .map_err(|error| format!("Failed to create Maple skills tools: {error}"))
}

struct TrustAwareSkillsClient {
    inner: SkillsClient,
    blocked_project_root: Option<PathBuf>,
}

#[async_trait::async_trait]
impl McpClientTrait for TrustAwareSkillsClient {
    async fn list_tools(
        &self,
        session_id: &str,
        next_cursor: Option<String>,
        cancel_token: CancellationToken,
    ) -> Result<ListToolsResult, rmcp::ServiceError> {
        self.inner
            .list_tools(session_id, next_cursor, cancel_token)
            .await
    }

    async fn call_tool(
        &self,
        context: &ToolCallContext,
        name: &str,
        arguments: Option<JsonObject>,
        cancel_token: CancellationToken,
    ) -> Result<CallToolResult, rmcp::ServiceError> {
        let requested_skill = arguments
            .as_ref()
            .and_then(|arguments| arguments.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let result = self
            .inner
            .call_tool(context, name, arguments, cancel_token)
            .await?;
        let skill_missing = result.is_error == Some(true)
            && result.content.iter().any(|content| {
                content.as_text().is_some_and(|text| {
                    text.text.starts_with("Skill '") && text.text.contains(" not found")
                })
            });
        let Some(project_root) = self
            .blocked_project_root
            .as_ref()
            .filter(|_| skill_missing && !requested_skill.is_empty())
        else {
            return Ok(result);
        };
        Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "No trusted skill named '{requested_skill}' was found. Project trust is disabled for '{}', so project-provided skills are unavailable. Ask the user to trust this project or continue without the skill. Do not read project skill files directly unless the user explicitly asks.",
            project_root.display()
        ))]))
    }

    fn get_info(&self) -> Option<&InitializeResult> {
        self.inner.get_info()
    }

    fn get_instructions(&self) -> Option<String> {
        self.inner.get_instructions()
    }

    async fn subscribe(&self) -> mpsc::Receiver<ServerNotification> {
        self.inner.subscribe().await
    }
}

fn prepare_transient_skills_client(
    paths: &AgentPathLayout,
    user_id: &str,
    agent: &Arc<Agent>,
    session: &Session,
) -> Result<TrustAwareSkillsClient, String> {
    let scope = skills_discovery_scope(paths, user_id, session)?;
    Ok(TrustAwareSkillsClient {
        inner: skills_client_for_working_dir(agent, session, scope.working_dir)?,
        blocked_project_root: scope.blocked_project_root,
    })
}

async fn attach_prepared_skills_client(agent: &Arc<Agent>, skills_client: TrustAwareSkillsClient) {
    agent
        .extension_manager
        .add_client(
            MAPLE_SKILLS_CLIENT_KEY.to_string(),
            maple_skills_extension_config(),
            Arc::new(skills_client),
            None,
            None,
        )
        .await;
}

struct AgentSkillsScope<'a> {
    paths: &'a AgentPathLayout,
    user_id: &'a str,
}

struct SessionAgentConfiguration<'a> {
    web_tool_state: &'a Arc<WebToolState>,
    session: &'a Session,
    model: &'a str,
    context_limit: Option<usize>,
    mode: &'a str,
    primary_model_supports_vision: bool,
    tool_context: &'a SharedAgentToolContext,
    python_binding: Option<&'a Arc<PythonTaskBinding>>,
    /// True only while a task is being driven by Maple's desktop surface.
    /// A cached User session may later be leased by ACP, so SessionType alone
    /// is not a sufficient host-process capability check.
    allow_embedded_cua: bool,
}

fn maple_model_config(
    model: &str,
    context_limit: Option<usize>,
) -> Result<goose_providers::model::ModelConfig, String> {
    let mut model_config =
        goose::model_config::model_config_from_user_config(MAPLE_PROVIDER_NAME, model)
            .map_err(|e| format!("Failed to configure Goose model {model}: {e}"))?;
    // Maple's authoritative catalog value is per session. Explicitly clear any
    // process-global Goose context override when metadata is unavailable.
    model_config.context_limit = context_limit.filter(|limit| *limit > 0);
    Ok(model_config)
}

async fn install_maple_provider<T>(
    agent: &Arc<Agent>,
    transport: &Arc<T>,
    session: &Session,
    model: &str,
    context_limit: Option<usize>,
) -> Result<(), String>
where
    T: provider::MapleInferenceTransport + 'static,
{
    // A session snapshot that came from the authoritative catalog remains the
    // best same-model value when a later UI/catalog version omits metadata.
    // Do not preserve it across a model change, where it may describe a
    // different provider window.
    let context_limit = context_limit.filter(|limit| *limit > 0).or_else(|| {
        session
            .model_config
            .as_ref()
            .filter(|config| config.model_name == model)
            .and_then(|config| config.context_limit)
            .filter(|limit| *limit > 0)
    });
    let model_config = maple_model_config(model, context_limit)?;
    install_maple_provider_config(agent, transport, &session.id, model_config).await
}

async fn install_maple_provider_config<T>(
    agent: &Arc<Agent>,
    transport: &Arc<T>,
    session_id: &str,
    model_config: goose_providers::model::ModelConfig,
) -> Result<(), String>
where
    T: provider::MapleInferenceTransport + 'static,
{
    let provider = Arc::new(MapleProvider::new(Arc::clone(transport)));
    agent
        .update_provider(provider, model_config, session_id)
        .await
        .map_err(|e| format!("Failed to update Goose provider: {e}"))
}

/// Write an empty enabled-extensions record into a new session.
///
/// Goose loads extensions from the session row when it builds the agent,
/// and it warns when the row has no record at all. Maple adds its built-in
/// tools only after the agent exists, so seed an empty record first, as
/// Goose's own gateway does.
async fn seed_empty_extension_state(
    session_manager: &Arc<SessionManager>,
    session: Session,
    cua_state: Option<CuaSessionState>,
) -> Result<Session, String> {
    let mut extension_data = session.extension_data.clone();
    goose::session::EnabledExtensionsState::new(Vec::new())
        .to_extension_data(&mut extension_data)
        .map_err(|e| format!("Failed to seed Agent task extension state: {e}"))?;
    if let Some(cua_state) = cua_state {
        put_session_cua_state(&mut extension_data, cua_state)?;
    }
    session_manager
        .update(&session.id)
        .extension_data(extension_data)
        .apply()
        .await
        .map_err(|e| format!("Failed to seed Agent task extension state: {e}"))?;
    session_manager
        .get_session(&session.id, false)
        .await
        .map_err(|e| format!("Failed to reload Agent task: {e}"))
}

async fn get_or_create_session_agent<T>(
    agent_manager: &Arc<AgentManager>,
    transport: &Arc<T>,
    session: &Session,
    harness_instructions: &str,
    runtime_context: RuntimeContext,
) -> Result<AgentManagerGetResult, String>
where
    T: provider::MapleInferenceTransport + 'static,
{
    let manager_result = agent_manager
        .get_or_create_agent_with_runtime_context(session.id.clone(), runtime_context)
        .await
        .map_err(|e| format!("Failed to load Agent for task {}: {e}", session.id))?;

    // Freshly created agents inherit goose's stock identity prompt, which
    // names goose and AAIF even though users only ever meet Maple's Agent
    // Mode. Brand the base prompt as Maple the moment the agent is created;
    // cached agents keep the override for their whole lifetime.
    // The harness that hosts this task supplies the opening instructions:
    // the desktop is Maple; an ACP caller sends its own persona.
    if manager_result.agent_created {
        let harness_instructions =
            session_system_prompt(&session.id).unwrap_or_else(|| harness_instructions.to_string());
        manager_result
            .agent
            .override_system_prompt(system_prompt::system_prompt(&harness_instructions))
            .await;
    }

    // Goose's built-in registry cannot reconstruct Maple's caller-owned
    // provider. Its default-provider fallback uses the runtime-global model and
    // persists that model immediately, so a cold admin action could otherwise
    // overwrite this task's locked model before the next send. Restore the
    // session snapshot while the caller still holds Maple's lifecycle guard.
    if manager_result.agent_created
        && session.provider_name.as_deref() == Some(MAPLE_PROVIDER_NAME)
        && let Some(model_config) = session.model_config.as_ref()
    {
        install_maple_provider_config(
            &manager_result.agent,
            transport,
            &session.id,
            model_config.clone(),
        )
        .await?;
    }

    Ok(manager_result)
}

/// Await an MCP startup that runs with Maple's lifecycle fences released.
///
/// `cancel` is the run's own token and `runtime_lifetime` is the token Stop
/// and logout cancel, so neither has to wait out the per-server MCP timeout
/// to end a startup that is already doomed.
async fn await_mcp_startup<T>(
    cancel: &CancellationToken,
    runtime_lifetime: &CancellationToken,
    work: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(MCP_STARTUP_CANCELLED_ERROR.to_string()),
        _ = runtime_lifetime.cancelled() => Err(MCP_STARTUP_CANCELLED_ERROR.to_string()),
        result = work => result,
    }
}

/// The Goose Agent for a task plus the connection errors its persisted MCP
/// servers reported while it was built.
struct PreparedSessionAgent {
    agent: Arc<Agent>,
    mcp_errors: Vec<AgentMcpConnectionError>,
}

/// Create or fetch this task's Goose Agent and restore its persisted MCP
/// extensions.
///
/// This is the unbounded half of session Agent configuration: every persisted
/// MCP server is spawned or handshaked here, each with a per-server timeout
/// that defaults to [`DEFAULT_MCP_TIMEOUT_SECONDS`]. Callers must run it
/// outside Maple's global lifecycle fences, with a cancellation-selectable
/// await, and re-verify runtime state before calling
/// [`finish_session_agent`] with the result.
async fn prepare_session_agent(
    agent_manager: &Arc<AgentManager>,
    maple_api_session: &Arc<MapleApiSession>,
    session: &Session,
    harness_instructions: &str,
) -> Result<PreparedSessionAgent, String> {
    let session_mcp_keys = session_mcp_extension_keys(session);
    let manager_result = get_or_create_session_agent(
        agent_manager,
        maple_api_session,
        session,
        harness_instructions,
        RuntimeContext::default(),
    )
    .await?;
    let mcp_errors = mcp_connection_errors(manager_result.extension_results, &session_mcp_keys);
    Ok(PreparedSessionAgent {
        agent: manager_result.agent,
        mcp_errors,
    })
}

async fn configure_session_agent(
    skills_scope: AgentSkillsScope<'_>,
    agent_manager: &Arc<AgentManager>,
    session_manager: &Arc<SessionManager>,
    maple_api_session: &Arc<MapleApiSession>,
    harness_instructions: &str,
    configuration: SessionAgentConfiguration<'_>,
) -> Result<(Arc<Agent>, Vec<AgentMcpConnectionError>), String> {
    let prepared = prepare_session_agent(
        agent_manager,
        maple_api_session,
        configuration.session,
        harness_instructions,
    )
    .await?;
    finish_session_agent(
        prepared,
        skills_scope,
        session_manager,
        maple_api_session,
        configuration,
    )
    .await
}

#[cfg(embedded_cua)]
fn embedded_cua_extension_config() -> ExtensionConfig {
    ExtensionConfig::Builtin {
        name: CUA_DRIVER_MCP_NAME.to_string(),
        description: CUA_DRIVER_DESCRIPTION.to_string(),
        display_name: Some(CUA_DRIVER_NAME.to_string()),
        timeout: Some(DEFAULT_EXTENSION_TIMEOUT),
        bundled: Some(true),
        available_tools: Vec::new(),
    }
}

/// Make the cached Goose Agent match Maple's task metadata and the surface
/// currently driving it. The client is registered as ephemeral so Goose never
/// writes an unreconstructable host-owned Rust object into its extension
/// snapshot.
async fn reconcile_embedded_cua_client(
    agent: &Arc<Agent>,
    session: &Session,
    account_scope: &str,
    allow_embedded_cua: bool,
    primary_model_supports_vision: bool,
) -> Option<AgentMcpConnectionError> {
    let state = session_cua_state(session);
    let should_attach = session.session_type == SessionType::User
        && allow_embedded_cua
        && state.is_some_and(|state| {
            state.backend == AgentIntegrationBackend::Embedded && state.enabled
        });

    if !should_attach {
        if state.is_some_and(|state| state.backend == AgentIntegrationBackend::Embedded) {
            let _ = agent
                .extension_manager
                .remove_extension(CUA_DRIVER_MCP_NAME)
                .await;
        }
        return None;
    }

    match attach_embedded_cua_client(agent, session, account_scope, primary_model_supports_vision)
        .await
    {
        Ok(()) => None,
        Err(error) => {
            let _ = agent
                .extension_manager
                .remove_extension(CUA_DRIVER_MCP_NAME)
                .await;
            Some(AgentMcpConnectionError {
                name: CUA_DRIVER_NAME.to_string(),
                error,
            })
        }
    }
}

#[cfg(embedded_cua)]
async fn attach_embedded_cua_client(
    agent: &Arc<Agent>,
    session: &Session,
    account_scope: &str,
    primary_model_supports_vision: bool,
) -> Result<(), String> {
    // A CUA trusted authorization has absolute and idle TTLs. Renew that
    // authorization at each desktop run boundary instead of keeping a cached
    // Agent's old authority alive indefinitely. The account/task-scoped CUA
    // lifecycle itself remains stable across these adapter replacements.
    let _ = agent
        .extension_manager
        .remove_extension(CUA_DRIVER_MCP_NAME)
        .await;
    let text_model_image_context = if primary_model_supports_vision {
        None
    } else {
        let mut context = agent.extension_manager.get_context().clone();
        context.extension_manager = Some(Arc::downgrade(&agent.extension_manager));
        Some(context)
    };
    let client = cua::create_embedded_cua_client(
        account_scope,
        &session.id,
        text_model_image_context,
        Arc::clone(&agent.config.permission_manager),
    )
    .await?;
    agent
        .extension_manager
        .add_ephemeral_client(
            CUA_DRIVER_MCP_NAME.to_string(),
            embedded_cua_extension_config(),
            client,
            None,
            None,
        )
        .await;
    Ok(())
}

#[cfg(not(embedded_cua))]
async fn attach_embedded_cua_client(
    _agent: &Arc<Agent>,
    _session: &Session,
    _account_scope: &str,
    _primary_model_supports_vision: bool,
) -> Result<(), String> {
    Err("Built-in CUA is not available on this operating system yet".to_string())
}

/// Install Maple's provider, permission routing, and built-in tool clients
/// into an Agent that [`prepare_session_agent`] already built.
///
/// Everything here is bounded local work, so it runs under Maple's lifecycle
/// fences: an extension must not be installed into an Agent that a concurrent
/// stop is tearing down.
async fn finish_session_agent(
    prepared: PreparedSessionAgent,
    skills_scope: AgentSkillsScope<'_>,
    session_manager: &Arc<SessionManager>,
    maple_api_session: &Arc<MapleApiSession>,
    configuration: SessionAgentConfiguration<'_>,
) -> Result<(Arc<Agent>, Vec<AgentMcpConnectionError>), String> {
    let SessionAgentConfiguration {
        web_tool_state,
        session,
        model,
        context_limit,
        mode,
        primary_model_supports_vision,
        tool_context,
        python_binding,
        allow_embedded_cua,
    } = configuration;
    let PreparedSessionAgent {
        agent,
        mut mcp_errors,
    } = prepared;
    let skills_client =
        prepare_transient_skills_client(skills_scope.paths, skills_scope.user_id, &agent, session)?;
    install_maple_provider(&agent, maple_api_session, session, model, context_limit).await?;
    // All transient MCP operations are hidden behind Maple's one static
    // `external_mcp` tool, which is permanently ask-before in Maple's owned
    // permission file. Keeping Goose in SmartApprove preserves native behavior
    // without persisting caller-controlled tool names or a lease-only mode.
    agent
        .update_goose_mode(GOOSE_PERMISSION_ROUTING_MODE, &session.id)
        .await
        .map_err(|e| format!("Failed to configure Goose permission routing: {e}"))?;
    let developer = ExtensionConfig::Builtin {
        name: "developer".to_string(),
        description: DEFAULT_EXTENSION_DESCRIPTION.to_string(),
        display_name: Some("Developer".to_string()),
        timeout: Some(DEFAULT_EXTENSION_TIMEOUT),
        bundled: Some(true),
        // MapleDeveloperClient owns the exact native catalog and may add a
        // frozen lease-scoped MCP catalog. Persisting dynamic tool names would
        // either hide those tools or leak transient session state.
        available_tools: Vec::new(),
    };
    let mut developer_context = agent.extension_manager.get_context().clone();
    if !primary_model_supports_vision {
        developer_context.extension_manager = Some(Arc::downgrade(&agent.extension_manager));
    }
    let web_transport: Arc<dyn crate::maple_api::MapleWebTransport> = maple_api_session.clone();
    let attachment_store = Arc::new(account_attachment_store(
        skills_scope.paths,
        skills_scope.user_id,
    )?);
    let mut developer_client = MapleDeveloperClient::new(
        developer_context,
        primary_model_supports_vision,
        web_transport,
        Arc::clone(web_tool_state),
        tool_context.clone(),
    )
    .map_err(|e| format!("Failed to create Maple developer tools: {e}"))?
    .with_attachment_store(attachment_store)
    .with_web_enabled(session_web_enabled(session))
    .with_desktop_ui_tools(session.session_type != SessionType::Acp);
    if let Some(binding) = python_binding {
        developer_client = developer_client.with_python_binding(Arc::clone(binding));
    }
    agent
        .extension_manager
        .add_client(
            "developer".to_string(),
            developer,
            Arc::new(developer_client),
            None,
            None,
        )
        .await;
    // SkillsClient needs a trust-filtered working directory, but Goose would reconstruct a
    // persisted platform extension with the real session root. Detach only for the extension-state
    // write, then restore unconditionally before propagating any persistence error.
    detach_transient_skills_client(&agent).await;
    // Goose persists the extension state itself here. A task that cannot
    // offer subagents is still a usable task, so a failure is a warning.
    if let Err(error) = agent
        .add_extension(maple_subagent_extension_config(), &session.id)
        .await
    {
        log::warn!("Subagents are unavailable for this task: {error}");
    }
    let persist_result = agent.persist_extension_state(&session.id).await;
    attach_prepared_skills_client(&agent, skills_client).await;
    persist_result.map_err(|e| format!("Failed to persist Maple built-in tools: {e}"))?;
    if let Some(error) = reconcile_embedded_cua_client(
        &agent,
        session,
        maple_api_session.account_scope(),
        allow_embedded_cua,
        primary_model_supports_vision,
    )
    .await
    {
        mcp_errors.push(error);
    }
    // Persist the user-facing policy separately for session restoration and display.
    session_manager
        .update(&session.id)
        .goose_mode(parse_goose_mode(mode))
        .apply()
        .await
        .map_err(|e| format!("Failed to persist Agent permission mode: {e}"))?;
    Ok((agent, mcp_errors))
}

/// The project addendum goose appends to the system prompt of a run
/// (`Agent::load_project_instructions`, which is private). A side question
/// must repeat it or its request prefix differs from the run's.
fn project_instructions_addendum(session: &Session) -> Option<String> {
    let project_id = session.project_id.as_deref()?;
    let entry = goose::sources::read_project(project_id).ok()?;
    let mut parts = vec![format!("# Project: {}", entry.name)];
    if !entry.description.is_empty() {
        parts.push(entry.description.clone());
    }
    if !entry.content.is_empty() {
        parts.push(entry.content.clone());
    }
    Some(parts.join("\n\n"))
}

/// The message projection goose applies before every provider call
/// (`reply_parts::stream_response_from_provider`, which is crate-private).
fn messages_for_provider(messages: Vec<Message>) -> Vec<Message> {
    let visible = Conversation::new_unvalidated(messages).agent_visible_messages();
    let (fixed, _) = fix_conversation(Conversation::new_unvalidated(visible));
    goose::conversation::merge_consecutive_messages_for_request(fixed.messages().clone())
}

pub(crate) fn emit_agent_event(events: &AgentEventDispatcher, event: AgentServiceEvent) {
    events.sink.emit(&event);
}

/// Mirror ~/.agents/AGENTS.md into the embedded goose path root so goose's
/// global hints loader sees it. A symlink stays current; an existing file at
/// the target is never overwritten.
fn link_global_agents_md(goose_path_root: &Path) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return;
    }
    let source = home.join(".agents").join("AGENTS.md");
    let target_dir = goose_path_root.join(".agents");
    link_agents_md_into(&source, &target_dir);
}

fn link_agents_md_into(source: &Path, target_dir: &Path) {
    if !source.is_file() {
        return;
    }
    let target = target_dir.join("AGENTS.md");
    // `Path::exists` follows symlinks, so a dangling link from an earlier
    // home layout reads as absent. The symlink call would then fail with
    // EEXIST and the copy would write through the stale link. Inspect the
    // link itself and clear it when its destination is gone.
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if target.exists() {
                return;
            }
            if fs::remove_file(&target).is_err() {
                return;
            }
        }
        Ok(_) => return,
        Err(_) => {}
    }
    let _ = fs::create_dir_all(target_dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        if symlink(source, &target).is_ok() {
            return;
        }
    }
    let _ = fs::copy(source, &target);
}

fn configure_embedded_goose(
    goose_path_root: &Path,
    model: &str,
    mode: &str,
    login_shell_search_paths: Option<&[String]>,
) -> Result<(), String> {
    fs::create_dir_all(goose_path_root.join("config"))
        .map_err(|e| format!("Failed to create Goose config dir: {e}"))?;
    // Goose resolves the global AGENTS.md relative to this path root, not the
    // real home. Link the user's ~/.agents/AGENTS.md in so it is honored.
    link_global_agents_md(goose_path_root);
    fs::create_dir_all(goose_path_root.join("data"))
        .map_err(|e| format!("Failed to create Goose data dir: {e}"))?;
    fs::create_dir_all(goose_path_root.join("state"))
        .map_err(|e| format!("Failed to create Goose state dir: {e}"))?;

    // SAFETY: this runs on the runtime bootstrap path before Goose starts
    // any thread that reads the environment. No other thread reads or
    // writes these variables concurrently.
    unsafe {
        std::env::set_var("GOOSE_PATH_ROOT", goose_path_root);
        // Maple's native provider owns upstream authentication. Goose must
        // never receive or persist a credential or retain the legacy loopback
        // proxy URL.
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("OPENAI_BASE_URL");
        std::env::remove_var("GOOSE_DISABLE_KEYRING");
        std::env::remove_var("GOOSE_MAX_TOKENS");
        // Maple still routes approvals through Goose's legacy reply loop. Keep
        // the experimental state-machine loop off until upstream makes it the
        // default.
        std::env::remove_var("GOOSE_STATE_MACHINE");
        std::env::remove_var("GOOSE_TOOL_PAIR_SUMMARIZATION");
        std::env::remove_var("GOOSE_PROVIDER");
        std::env::remove_var("GOOSE_MODEL");
    }

    remove_maple_owned_goose_file(
        &goose_path_root.join("config").join("secrets.yaml"),
        "secrets",
    )?;
    let config = goose::config::Config::global();
    config.invalidate_secrets_cache();
    delete_goose_config_key(config, "GOOSE_DISABLE_KEYRING")?;
    delete_goose_config_key(config, "GOOSE_MAX_TOKENS")?;
    delete_goose_config_key(config, "OPENAI_BASE_URL")?;
    configure_embedded_goose_search_paths(config, login_shell_search_paths)?;
    configure_embedded_goose_params(config, model, mode)?;

    set_owner_only_permissions(&goose_path_root.join("config").join("config.yaml"));
    Ok(())
}

fn configure_embedded_goose_search_paths(
    config: &goose::config::Config,
    login_shell_search_paths: Option<&[String]>,
) -> Result<(), String> {
    let Some(paths) = login_shell_search_paths else {
        // Linux and Windows retain Goose's normal inherited-PATH behavior.
        return Ok(());
    };
    // Goose prepends this supported setting to its built-in search directories
    // and inherited PATH for both STDIO executable lookup and the child PATH.
    // Persist [] after a failed probe so paths recovered by an earlier app run
    // cannot remain active after the user's shell configuration changes.
    config
        .set_goose_search_paths(paths.to_vec())
        .map_err(|e| format!("Failed to configure Goose search paths: {e}"))
}

fn configure_embedded_goose_params(
    config: &goose::config::Config,
    model: &str,
    mode: &str,
) -> Result<(), String> {
    goose::config::set_active_provider(config, MAPLE_PROVIDER_NAME, model)
        .map_err(|e| format!("Failed to configure Goose provider: {e}"))?;
    config
        .set_param("GOOSE_FAST_MODEL", model)
        .map_err(|e| format!("Failed to configure Goose fast model: {e}"))?;
    config
        .set_param("GOOSE_MODE", mode)
        .map_err(|e| format!("Failed to configure Goose mode: {e}"))?;
    // Maple does not expose Goose's hidden history rewrite. Preserve exact tool evidence and
    // provider prompt-cache continuity unless Maple supports that lifecycle end to end.
    config
        .set_param("GOOSE_TOOL_PAIR_SUMMARIZATION", false)
        .map_err(|e| format!("Failed to disable Goose tool-pair summarization: {e}"))?;
    Ok(())
}

fn delete_goose_config_key(config: &goose::config::Config, key: &str) -> Result<(), String> {
    match config.delete(key) {
        Ok(()) | Err(ConfigError::NotFound(_)) => Ok(()),
        Err(e) => Err(format!("Failed to clear Goose config key {key}: {e}")),
    }
}

fn remove_maple_owned_goose_file(path: &Path, description: &str) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Failed to remove Maple-owned Goose {description} file {}: {error}",
            path.display()
        )),
    }
}

fn reset_maple_owned_permission_file(path: &Path) -> Result<(), String> {
    // Atomic temp-and-rename with owner-only mode: a crash mid-write must
    // not leave a truncated permission file that Goose would read as empty
    // and treat as "nothing requires approval".
    crate::private_file::write_private_file(path, MAPLE_GOOSE_PERMISSION_CONFIG.as_bytes()).map_err(
        |error| {
            format!(
                "Failed to reset Maple-owned Goose permission file {}: {error}",
                path.display()
            )
        },
    )
}

fn parse_goose_mode(mode: &str) -> GooseMode {
    GooseMode::from_str(mode).unwrap_or(GooseMode::SmartApprove)
}

/// A permission mode whose unresolved interactive decisions belong to the
/// calling surface rather than Maple. `Approve` asks before every tool
/// call; legacy sessions persisted under `smart_approve` keep the same
/// caller-mediated policy.
fn is_caller_mediated_mode(mode: GooseMode) -> bool {
    matches!(mode, GooseMode::Approve | GooseMode::SmartApprove)
}

fn parse_user_permission_mode(mode: &str) -> Result<GooseMode, String> {
    match mode {
        "auto" => Ok(GooseMode::Auto),
        "approve" => Ok(GooseMode::Approve),
        "smart_approve" => Ok(GooseMode::SmartApprove),
        _ => Err(format!("Unsupported Agent permission mode: {mode}")),
    }
}

fn stopped_status() -> AgentRuntimeStatus {
    AgentRuntimeStatus {
        running: false,
        project_root: None,
        model: None,
        mode: None,
        active_runs: HashMap::new(),
    }
}

fn resolve_project_root(requested: Option<&str>, config: &AgentConfig) -> Result<PathBuf, String> {
    if let Some(path) = requested.filter(|value| !value.trim().is_empty()) {
        return normalize_project_root(Path::new(path));
    }

    // A removed project persists only as a device-local tombstone; the
    // roaming default may still name it. Never boot into a hidden root.
    if let Some(path) = config
        .default_project_root
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        && !is_removed_project_root(path, &config.removed_project_roots)
        && let Ok(root) = normalize_project_root(Path::new(path))
        && !is_removed_project_root(&path_string(&root), &config.removed_project_roots)
    {
        return Ok(root);
    }

    std::env::current_dir()
        .map_err(|e| format!("Failed to read current directory: {e}"))
        .and_then(|path| normalize_project_root(&path))
}

fn is_removed_project_root(path: &str, removed_project_roots: &[String]) -> bool {
    removed_project_roots.iter().any(|removed| removed == path)
}

fn normalize_project_root(path: &Path) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a folder", canonical.display()));
    }
    Ok(canonical)
}

fn agent_root_dir(paths: &AgentPathLayout) -> Result<PathBuf, anyhow::Error> {
    let path = paths.config_root.clone();
    fs::create_dir_all(&path)?;
    set_owner_only_dir_permissions(&path);
    Ok(path)
}

fn account_config_dir_path(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<PathBuf, anyhow::Error> {
    let scope = account_scope(user_id).map_err(anyhow::Error::msg)?;
    Ok(agent_root_dir(paths)?.join("accounts").join(scope))
}

fn account_local_data_dir_path(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<PathBuf, anyhow::Error> {
    let scope = account_scope(user_id).map_err(anyhow::Error::msg)?;
    Ok(paths.local_data_root.join("accounts").join(scope))
}

/// The device-local data directory this crate owns for one account:
/// Goose session history, image attachments, and anything else keyed to
/// the signed-in user. Removing it removes the account's local state.
///
/// Pure path arithmetic; it creates nothing and touches no disk. Callers
/// outside this crate should use it instead of rebuilding the layout by
/// hand, because the on-disk shape is this crate's private business.
pub fn account_local_data_dir(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, String> {
    account_local_data_dir_path(paths, user_id).map_err(|error| error.to_string())
}

/// The Goose session database for one account.
///
/// The agent runtime owns and writes this file. Other processes and other
/// parts of the app must open it read-only, and must tolerate it not
/// existing yet: it appears the first time the runtime starts.
pub fn account_sessions_db_path(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, String> {
    Ok(account_local_data_dir(paths, user_id)?
        .join(AGENT_HISTORY_SUBDIR)
        .join(goose::session::session_manager::SESSIONS_FOLDER)
        .join(goose::session::session_manager::DB_NAME))
}

/// The store of model-written tool call summaries for one account.
///
/// The app owns this file; the agent runtime never opens it. It lives
/// beside the runtime's own account data so that deleting the account
/// removes the summaries with it.
pub fn account_tool_summaries_db_path(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<PathBuf, String> {
    Ok(account_local_data_dir(paths, user_id)?.join(AGENT_TOOL_SUMMARIES_DB_NAME))
}

fn account_attachment_store(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<AgentAttachmentStore, String> {
    account_local_data_dir_path(paths, user_id)
        .map(AgentAttachmentStore::new)
        .map_err(|error| error.to_string())
}

fn agent_config_dir(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, anyhow::Error> {
    let path = account_config_dir_path(paths, user_id)?;
    fs::create_dir_all(&path)?;
    set_owner_only_dir_permissions(&path);
    Ok(path)
}

fn account_session_manager(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<Arc<SessionManager>, String> {
    Ok(Arc::new(SessionManager::new(account_history_dir(
        paths, user_id,
    )?)))
}

/// Path of the Goose session store (`sessions/sessions.db` and friends)
/// below an account directory.
const AGENT_HISTORY_SUBDIR: &str = "goose/data";

/// File name of the app-owned tool call summary store below an account
/// directory. See [`account_tool_summaries_db_path`].
const AGENT_TOOL_SUMMARIES_DB_NAME: &str = "tool_summaries.db";

/// The Goose data directory for one account, ready to open. Session history
/// is device-local, so it lives in the local data root, next to the
/// attachments. The account `config.json` stays in the config root, which a
/// user may sync between machines.
fn account_history_dir(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, String> {
    let local_account_dir =
        account_local_data_dir_path(paths, user_id).map_err(|error| error.to_string())?;
    history_dir_for_account_dir(&local_account_dir)
}

fn history_dir_for_account_dir(account_dir: &Path) -> Result<PathBuf, String> {
    let data_dir = account_dir.join(AGENT_HISTORY_SUBDIR);
    fs::create_dir_all(&data_dir)
        .map_err(|error| format!("Failed to create Goose data dir: {error}"))?;
    if let Some(parent) = data_dir.parent() {
        set_owner_only_dir_permissions(parent);
    }
    set_owner_only_dir_permissions(&data_dir);
    Ok(data_dir)
}

#[cfg(test)]
fn session_manager_for_account_dir(account_dir: &Path) -> Result<Arc<SessionManager>, String> {
    Ok(Arc::new(SessionManager::new(history_dir_for_account_dir(
        account_dir,
    )?)))
}

#[cfg(test)]
fn clear_agent_history(account_dir: &Path) -> Result<(), anyhow::Error> {
    remove_agent_history_path(&account_dir.join(AGENT_HISTORY_SUBDIR))
}

#[cfg(test)]
fn remove_agent_history_path(path: &Path) -> Result<(), anyhow::Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let result = if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(Into::into)
}
fn load_agent_config_inner(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<AgentConfig, anyhow::Error> {
    let path = agent_config_dir(paths, user_id)?.join("config.json");
    let removed_project_roots_path =
        account_local_data_dir_path(paths, user_id)?.join("removed_project_roots.json");
    load_agent_config_files(&path, &removed_project_roots_path)
}

fn load_agent_config_files(
    config_path: &Path,
    removed_project_roots_path: &Path,
) -> Result<AgentConfig, anyhow::Error> {
    let had_legacy_project_skills_trust = config_file_uses_legacy_project_skills_trust(config_path);
    let mut config = load_agent_config_file(config_path)?;
    // This field was introduced by the unshipped remove-project work. Never
    // adopt it from the roaming config: on Windows it may have come from a
    // different device using the same roaming profile.
    let had_roaming_removed_project_roots = !config.removed_project_roots.is_empty();
    let migrated = migrate_agent_config(&mut config);
    config.removed_project_roots = load_removed_project_roots_file(removed_project_roots_path)?;
    if migrated || had_roaming_removed_project_roots || had_legacy_project_skills_trust {
        save_agent_config_file(config_path, &config)?;
    }
    Ok(config)
}

fn config_file_uses_legacy_project_skills_trust(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
        .is_some_and(|config| config.get("projectSkillsTrust").is_some())
}

fn load_agent_config_file(path: &Path) -> Result<AgentConfig, anyhow::Error> {
    if !path.exists() {
        return Ok(AgentConfig::default());
    }
    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&contents)?)
}

fn migrate_agent_config(config: &mut AgentConfig) -> bool {
    let mut changed = false;
    if config.default_model == LEGACY_AGENT_DEFAULT_MODEL {
        config.default_model = default_agent_model();
        changed = true;
    }
    let original_removed_roots = config.removed_project_roots.clone();
    config.removed_project_roots =
        sanitize_project_root_paths(std::mem::take(&mut config.removed_project_roots));
    changed || config.removed_project_roots != original_removed_roots
}

fn save_agent_config_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    config: &AgentConfig,
) -> Result<(), anyhow::Error> {
    let path = agent_config_dir(paths, user_id)?.join("config.json");
    save_agent_config_file(&path, config)
}

fn save_agent_config_file(path: &Path, config: &AgentConfig) -> Result<(), anyhow::Error> {
    let mut roaming_config = config.clone();
    roaming_config.removed_project_roots.clear();
    write_json_file(path, &roaming_config)
}

fn load_removed_project_roots_file(path: &Path) -> Result<Vec<String>, anyhow::Error> {
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)?;
    let roots = serde_json::from_str::<Vec<String>>(&contents)?;
    let sanitized = sanitize_project_root_paths(roots.clone());
    if sanitized != roots {
        write_device_local_json_file(path, &sanitized)?;
    }
    Ok(sanitized)
}

fn save_removed_project_roots_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    roots: &[String],
) -> Result<(), anyhow::Error> {
    let path = account_local_data_dir_path(paths, user_id)?.join("removed_project_roots.json");
    write_device_local_json_file(&path, roots)
}

fn project_trust_status(
    config: &AgentConfig,
    project_root: &Path,
    available: bool,
) -> AgentProjectTrustStatus {
    let path = path_string(project_root);
    let decision = config
        .project_trust
        .iter()
        .find(|entry| entry.path == path)
        .map(|entry| entry.trusted);
    AgentProjectTrustStatus {
        path,
        decision,
        available,
        protected_features: if available {
            project_trust_features(project_root)
        } else {
            Vec::new()
        },
    }
}

fn project_trust_features(project_root: &Path) -> Vec<AgentProjectTrustFeature> {
    const MAX_SKILL_ENTRIES: usize = 4_096;
    let skills_root = project_root.join(".agents").join("skills");
    let mut pending = vec![skills_root];
    let mut visited = 0usize;
    while let Some(directory) = pending.pop() {
        if visited >= MAX_SKILL_ENTRIES {
            break;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if visited >= MAX_SKILL_ENTRIES {
                break;
            }
            visited += 1;
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_file()
                && path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md")
            {
                return vec![AgentProjectTrustFeature::Skills];
            }
            if file_type.is_dir() {
                pending.push(path);
            }
        }
    }
    Vec::new()
}

fn apply_project_trust(config: &mut AgentConfig, project_root: &Path, trusted: bool) {
    let path = path_string(project_root);
    if let Some(existing) = config
        .project_trust
        .iter_mut()
        .find(|entry| entry.path == path)
    {
        existing.trusted = trusted;
        return;
    }
    config
        .project_trust
        .push(AgentProjectTrust { path, trusted });
}

fn load_recent_project_roots_inner(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let path = agent_config_dir(paths, user_id)?.join("recent_roots.json");
    load_recent_project_roots_file(&path)
}

fn read_recent_project_roots_file(path: &Path) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&contents)?)
}

fn load_recent_project_roots_file(path: &Path) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    Ok(sanitize_recent_project_roots(
        read_recent_project_roots_file(path)?,
    ))
}

fn structurally_valid_project_root(path: &str) -> bool {
    !path.is_empty() && !path.contains('\0') && Path::new(path).is_absolute()
}

fn sanitize_project_root_paths(paths: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| structurally_valid_project_root(path) && seen.insert(path.clone()))
        .collect()
}

fn sanitize_recent_project_roots(roots: Vec<RecentProjectRoot>) -> Vec<RecentProjectRoot> {
    let mut seen = HashSet::new();
    roots
        .into_iter()
        .filter(|root| {
            structurally_valid_project_root(&root.path) && seen.insert(root.path.clone())
        })
        .collect()
}

fn project_root_record(path: String, last_used_ms: u128) -> RecentProjectRoot {
    let name = Path::new(&path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(&path)
        .to_string();
    RecentProjectRoot {
        path,
        name,
        last_used_ms,
    }
}

fn register_explicit_project_root(
    roots: Vec<RecentProjectRoot>,
    project_root: &Path,
    last_used_ms: u128,
) -> (Vec<RecentProjectRoot>, bool) {
    let original_len = roots.len();
    let mut roots = sanitize_recent_project_roots(roots);
    let sanitized = roots.len() != original_len;
    let path = path_string(project_root);
    if roots.iter().any(|root| root.path == path) {
        return (roots, sanitized);
    }

    roots.insert(0, project_root_record(path, last_used_ms));
    (roots, true)
}

fn register_explicit_project_root_file(
    file_path: &Path,
    project_root: &Path,
    last_used_ms: u128,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let roots = read_recent_project_roots_file(file_path)?;
    let (roots, changed) = register_explicit_project_root(roots, project_root, last_used_ms);
    if changed {
        write_json_file(file_path, &roots)?;
    }
    Ok(roots)
}

fn register_explicit_project_root_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    project_root: &Path,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let file_path = agent_config_dir(paths, user_id)?.join("recent_roots.json");
    register_explicit_project_root_file(&file_path, project_root, unix_ms())
}

fn restore_explicit_project_root(
    roots: Vec<RecentProjectRoot>,
    project_root: &Path,
    last_used_ms: u128,
) -> Vec<RecentProjectRoot> {
    let path = path_string(project_root);
    let mut roots = sanitize_recent_project_roots(roots)
        .into_iter()
        .filter(|root| root.path != path)
        .collect::<Vec<_>>();
    roots.insert(0, project_root_record(path, last_used_ms));
    roots
}

fn restore_explicit_project_root_file(
    file_path: &Path,
    project_root: &Path,
    last_used_ms: u128,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let roots = restore_explicit_project_root(
        read_recent_project_roots_file(file_path)?,
        project_root,
        last_used_ms,
    );
    write_json_file(file_path, &roots)?;
    Ok(roots)
}

fn restore_explicit_project_root_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    project_root: &Path,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let file_path = agent_config_dir(paths, user_id)?.join("recent_roots.json");
    restore_explicit_project_root_file(&file_path, project_root, unix_ms())
}

fn apply_project_root_order(
    roots: Vec<RecentProjectRoot>,
    paths: Vec<String>,
    last_used_ms: u128,
) -> Result<Vec<RecentProjectRoot>, String> {
    let roots = sanitize_recent_project_roots(roots);
    let mut requested_paths = Vec::new();
    let mut requested_set = HashSet::new();
    for path in paths {
        if structurally_valid_project_root(&path) && requested_set.insert(path.clone()) {
            requested_paths.push(path);
        }
    }

    let missing_paths = roots
        .iter()
        .filter(|root| !requested_set.contains(&root.path))
        .map(|root| root.path.clone())
        .collect::<Vec<_>>();
    if !missing_paths.is_empty() {
        return Err(format!(
            "Project order is stale and omitted known project roots: {}",
            missing_paths.join(", ")
        ));
    }

    let mut roots_by_path = roots
        .into_iter()
        .map(|root| (root.path.clone(), root))
        .collect::<HashMap<_, _>>();
    Ok(requested_paths
        .into_iter()
        .map(|path| {
            roots_by_path
                .remove(&path)
                .unwrap_or_else(|| project_root_record(path, last_used_ms))
        })
        .collect())
}

fn save_project_root_order_file(
    file_path: &Path,
    paths: Vec<String>,
    last_used_ms: u128,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let roots = read_recent_project_roots_file(file_path)?;
    let roots = apply_project_root_order(roots, paths, last_used_ms).map_err(anyhow::Error::msg)?;
    write_json_file(file_path, &roots)?;
    Ok(roots)
}

fn save_project_root_order_inner(
    layout: &AgentPathLayout,
    user_id: &str,
    mut paths: Vec<String>,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let file_path = agent_config_dir(layout, user_id)?.join("recent_roots.json");
    let removed = load_agent_config_inner(layout, user_id)?
        .removed_project_roots
        .into_iter()
        .collect::<HashSet<_>>();
    let requested = paths.iter().cloned().collect::<HashSet<_>>();
    paths.extend(
        read_recent_project_roots_file(&file_path)?
            .into_iter()
            .filter(|root| removed.contains(&root.path) && !requested.contains(&root.path))
            .map(|root| root.path),
    );
    save_project_root_order_file(&file_path, paths, unix_ms())
}

fn has_active_session_run(active_runs: &HashMap<String, ActiveAgentRun>, session_id: &str) -> bool {
    active_runs.values().any(|run| run.session_id == session_id)
}

enum DesktopSendPlan {
    Staged {
        run_id: String,
        queued: Box<AgentQueuedMessage>,
        queue: AgentDesktopQueueSnapshot,
    },
    Steered {
        run_id: String,
        queue: AgentDesktopQueueSnapshot,
    },
    Start {
        launch_messages: Vec<Message>,
        queue: AgentDesktopQueueSnapshot,
        consume_queue_ids: Vec<String>,
    },
}

fn user_message_from_prompt(text: &str) -> Message {
    Message::user().with_text(text).with_generated_id()
}

fn user_message_with_images(
    text: &str,
    images: &[PreparedAgentImage],
    vision_capable: bool,
) -> Message {
    if images.is_empty() {
        return user_message_from_prompt(text);
    }
    let attachments = images
        .iter()
        .map(|image| image.attachment.clone())
        .collect::<Vec<_>>();
    let mut message =
        Message::user().with_text(agent_image_prompt(text, &attachments, vision_capable));
    if vision_capable {
        for image in images {
            message = message.with_image(&image.base64_data, &image.attachment.mime_type);
        }
    }
    message.metadata.set_operation_note(
        MAPLE_IMAGE_ATTACHMENTS_OPERATION,
        "userText",
        Value::String(text.to_string()),
    );
    message.metadata.set_operation_note(
        MAPLE_IMAGE_ATTACHMENTS_OPERATION,
        "items",
        serde_json::to_value(&attachments).unwrap_or(Value::Array(Vec::new())),
    );
    message.metadata.set_operation_note(
        MAPLE_IMAGE_ATTACHMENTS_OPERATION,
        "visionCapable",
        Value::Bool(vision_capable),
    );
    message.with_generated_id()
}

fn agent_image_prompt(
    text: &str,
    attachments: &[AgentImageAttachment],
    vision_capable: bool,
) -> String {
    let mut prompt = text.trim().to_string();
    if !prompt.is_empty() {
        prompt.push_str("\n\n");
    }
    prompt.push_str("The user attached the following image");
    if attachments.len() == 1 {
        prompt.push_str(":\n");
    } else {
        prompt.push_str("s:\n");
    }
    for attachment in attachments {
        let name = serde_json::to_string(&attachment.name).unwrap_or_else(|_| "\"image\"".into());
        if vision_capable {
            prompt.push_str(&format!("- {name}\n"));
        } else {
            prompt.push_str(&format!("- {name}: {}\n", attachment.source));
        }
    }
    if !vision_capable {
        prompt.push_str(
            "Use read_image with an attachment source when you need visual details from that image.",
        );
    }
    prompt
}

fn message_image_attachments(message: &Message) -> Vec<AgentImageAttachment> {
    message
        .metadata
        .operation_note(MAPLE_IMAGE_ATTACHMENTS_OPERATION, "items")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn message_original_user_text(message: &Message) -> Option<String> {
    message
        .metadata
        .operation_note(MAPLE_IMAGE_ATTACHMENTS_OPERATION, "userText")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn replace_queued_message_text(message: &mut Message, text: &str) {
    let attachments = message_image_attachments(message);
    if attachments.is_empty() {
        let message_id = message.id.clone();
        *message = user_message_from_prompt(text);
        if let Some(message_id) = message_id {
            message.id = Some(message_id);
        }
        return;
    }
    let vision_capable = message
        .metadata
        .operation_note(MAPLE_IMAGE_ATTACHMENTS_OPERATION, "visionCapable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let replacement =
        Message::user().with_text(agent_image_prompt(text, &attachments, vision_capable));
    if let Some(content) = replacement.content.into_iter().next()
        && let Some(first) = message.content.first_mut()
    {
        *first = content;
    }
    message.metadata.set_operation_note(
        MAPLE_IMAGE_ATTACHMENTS_OPERATION,
        "userText",
        Value::String(text.to_string()),
    );
}

async fn take_desktop_steer_plan(
    state: &MapleAgentService,
    account_scope: &str,
    request: &AgentSendMessageRequest,
    text: &str,
    draft_message: Option<&Message>,
) -> Result<DesktopSendPlan, String> {
    reject_foreign_surface_session(state, account_scope, &request.session_id).await?;
    if request.queue_id.is_some() && !request.attachments.is_empty() {
        return Err(QUEUED_MESSAGE_ATTACHMENTS_ERROR.to_string());
    }
    if let Some(run_id) =
        desktop_run_id_for_session(state, account_scope, &request.session_id).await?
    {
        if let Some(queue_id) = request.queue_id.as_deref() {
            if !text.is_empty() {
                update_desktop_queue_item(
                    state,
                    account_scope,
                    &request.session_id,
                    queue_id,
                    text,
                )
                .await?;
            }
            let (removed, snapshot) =
                remove_desktop_queue_item(state, account_scope, &request.session_id, queue_id)
                    .await?;
            publish_desktop_queue_changed(
                state,
                account_scope,
                &request.session_id,
                snapshot.clone(),
            )
            .await;
            steer_into_desktop_run(
                state,
                account_scope,
                &request.session_id,
                &queued_user_message(&removed),
            )
            .await?;
            return Ok(DesktopSendPlan::Steered {
                run_id,
                queue: snapshot,
            });
        }
        if draft_message.is_none() {
            let Some((items, snapshot)) = take_all_desktop_queue_items_from_map(
                &state.desktop_queues,
                account_scope,
                &request.session_id,
            )
            .await
            else {
                let leftover =
                    snapshot_desktop_queue(state, account_scope, &request.session_id).await;
                return Err(if leftover.items.is_empty() {
                    "Prompt cannot be empty".to_string()
                } else {
                    "Queued messages cannot be steered while one is being edited".to_string()
                });
            };
            publish_desktop_queue_changed(
                state,
                account_scope,
                &request.session_id,
                snapshot.clone(),
            )
            .await;
            for item in &items {
                steer_into_desktop_run(
                    state,
                    account_scope,
                    &request.session_id,
                    &queued_user_message(item),
                )
                .await?;
            }
            return Ok(DesktopSendPlan::Steered {
                run_id,
                queue: snapshot,
            });
        }
        if text.len() > MAX_DESKTOP_QUEUE_TEXT_BYTES {
            return Err("Queued Agent message is too large".to_string());
        }
        let snapshot = snapshot_desktop_queue(state, account_scope, &request.session_id).await;
        steer_into_desktop_run(
            state,
            account_scope,
            &request.session_id,
            draft_message.expect("non-empty draft checked above"),
        )
        .await?;
        return Ok(DesktopSendPlan::Steered {
            run_id,
            queue: snapshot,
        });
    }

    if let Some(queue_id) = request.queue_id.as_deref() {
        if !text.is_empty() {
            update_desktop_queue_item(state, account_scope, &request.session_id, queue_id, text)
                .await?;
        }
        let snapshot = snapshot_desktop_queue(state, account_scope, &request.session_id).await;
        let selected = snapshot
            .items
            .iter()
            .find(|item| item.queue_id == queue_id)
            .ok_or_else(|| "Queued Agent message has already been sent".to_string())?;
        return Ok(DesktopSendPlan::Start {
            launch_messages: vec![queued_user_message(selected)],
            consume_queue_ids: vec![queue_id.to_string()],
            queue: snapshot,
        });
    }

    let launch = prepare_desktop_launch_message(
        state,
        account_scope,
        &request.session_id,
        draft_message.cloned(),
    )
    .await?;
    Ok(DesktopSendPlan::Start {
        launch_messages: launch.launch_messages,
        queue: launch.queue,
        consume_queue_ids: launch.consume_queue_ids,
    })
}

async fn steer_into_desktop_run(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
    message: &Message,
) -> Result<(), String> {
    let (agent, events, permission_routing, steered_unacked) = {
        let runtime = state.inner.lock().await;
        let current = runtime
            .as_ref()
            .ok_or_else(|| "Agent runtime is not running".to_string())?;
        ensure_runtime_account(current, account_scope)?;
        let active_run = current
            .active_runs
            .values()
            .find(|run| run.session_id == session_id && desktop_run_is_stageable(run))
            .ok_or_else(|| "No active Agent run to steer".to_string())?;
        (
            Arc::clone(&active_run.agent),
            active_run.events.clone(),
            active_run.permission_routing,
            Arc::clone(&active_run.steered_unacked),
        )
    };
    agent.steer(session_id, message.clone()).await;
    steered_unacked.lock().await.push(message.clone());
    if let Some(mut item) = message_to_timeline_items(message, false).into_iter().next() {
        // Keep the pending-assistant loader off until Goose starts the next
        // turn. This row is already in the live loop but not yet picked up.
        item.status = Some("steered".to_string());
        record_and_emit_timeline_item(
            &events,
            &state.live_timelines,
            session_id,
            permission_routing,
            item,
        )
        .await;
    }
    Ok(())
}

async fn ack_steered_message(steered_unacked: &Mutex<Vec<Message>>, message: &Message) {
    let Some(message_id) = message.id.as_deref() else {
        return;
    };
    steered_unacked
        .lock()
        .await
        .retain(|pending| pending.id.as_deref() != Some(message_id));
}

async fn persist_unacked_steers(
    session_manager: &SessionManager,
    session_id: &str,
    steered_unacked: &Mutex<Vec<Message>>,
) {
    let pending = {
        let mut steered_unacked = steered_unacked.lock().await;
        std::mem::take(&mut *steered_unacked)
    };
    if pending.is_empty() {
        return;
    }
    let already_persisted: HashSet<String> =
        match session_manager.get_session(session_id, true).await {
            Ok(session) => session
                .conversation
                .as_ref()
                .map(|conversation| {
                    conversation
                        .messages()
                        .iter()
                        .filter_map(|message| message.id.clone())
                        .collect()
                })
                .unwrap_or_default(),
            Err(error) => {
                log::warn!(
                    "Failed to inspect session before persisting steered Agent messages: {error}"
                );
                HashSet::new()
            }
        };
    for message in pending {
        if message
            .id
            .as_deref()
            .is_some_and(|message_id| already_persisted.contains(message_id))
        {
            continue;
        }
        if let Err(error) = session_manager.add_message(session_id, &message).await {
            log::warn!("Failed to persist steered Agent message: {error}");
        }
    }
}

fn steered_run_handle(run_id: String, queue: AgentDesktopQueueSnapshot) -> AgentRunHandle {
    let (_events_tx, events) = mpsc::channel(1);
    let (_terminal_tx, terminal) = watch::channel(None);
    let (_usage_tx, usage) = watch::channel(None);
    AgentRunHandle {
        run_id,
        events,
        terminal,
        usage,
        event_overflowed: Arc::new(AtomicBool::new(false)),
        permission_responder: None,
        cancellation: None,
        queued: None,
        queue,
    }
}

fn staged_run_handle(
    run_id: String,
    queued: AgentQueuedMessage,
    queue: AgentDesktopQueueSnapshot,
) -> AgentRunHandle {
    let (_events_tx, events) = mpsc::channel(1);
    let (_terminal_tx, terminal) = watch::channel(None);
    let (_usage_tx, usage) = watch::channel(None);
    AgentRunHandle {
        run_id,
        events,
        terminal,
        usage,
        event_overflowed: Arc::new(AtomicBool::new(false)),
        permission_responder: None,
        cancellation: None,
        queued: Some(queued),
        queue,
    }
}

fn desktop_run_is_stageable(run: &ActiveAgentRun) -> bool {
    run.permission_routing == AgentPermissionRouting::Desktop
        && !run.token.is_cancelled()
        && run.accepting_queue.load(Ordering::Acquire)
}

fn desktop_queue_key(account_scope: &str, session_id: &str) -> (String, String) {
    (account_scope.to_string(), session_id.to_string())
}

async fn reject_foreign_surface_session(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
) -> Result<(), String> {
    let runtime = state.inner.lock().await;
    let Some(current) = runtime.as_ref() else {
        return Ok(());
    };
    ensure_runtime_account(current, account_scope)?;
    if current.active_runs.values().any(|run| {
        run.session_id == session_id
            && run.permission_routing == AgentPermissionRouting::CallingSurface
    }) {
        return Err("This Agent task is controlled by another Agent surface".to_string());
    }
    Ok(())
}

async fn desktop_run_id_for_session(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
) -> Result<Option<String>, String> {
    let runtime = state.inner.lock().await;
    let Some(current) = runtime.as_ref() else {
        return Ok(None);
    };
    ensure_runtime_account(current, account_scope)?;
    Ok(current.active_runs.iter().find_map(|(run_id, run)| {
        (run.session_id == session_id && desktop_run_is_stageable(run)).then(|| run_id.clone())
    }))
}

async fn snapshot_desktop_queue(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
) -> AgentDesktopQueueSnapshot {
    state
        .desktop_queues
        .lock()
        .await
        .get(&desktop_queue_key(account_scope, session_id))
        .map(DesktopSessionQueue::snapshot)
        .unwrap_or_else(empty_desktop_queue_snapshot)
}

struct DesktopLaunchPlan {
    launch_messages: Vec<Message>,
    consume_queue_ids: Vec<String>,
    queue: AgentDesktopQueueSnapshot,
}

fn queued_user_message(queued: &AgentQueuedMessage) -> Message {
    let mut message = queued.message.clone();
    message.id = Some(queued.message_id.clone());
    message
}

/// Persist every message of a promoted batch except the last, which
/// `run_agent_prompt` persists itself. On failure the error carries how many
/// leading messages were written so the caller can restore the rest.
async fn persist_leading_user_messages(
    session_manager: &SessionManager,
    session_id: &str,
    messages: &[Message],
) -> Result<(), (usize, String)> {
    if messages.len() < 2 {
        return Ok(());
    }
    for (persisted, message) in messages[..messages.len() - 1].iter().enumerate() {
        session_manager
            .add_message(session_id, message)
            .await
            .map_err(|error| {
                (
                    persisted,
                    format!("Failed to persist queued Agent message: {error}"),
                )
            })?;
    }
    Ok(())
}

/// Put messages that were drained for a run but never persisted back at the
/// head of the desktop queue, in their original order, so the chips reappear
/// ahead of anything the user staged in the meantime.
async fn restore_unpersisted_desktop_queue_messages(
    queues: &Mutex<HashMap<(String, String), DesktopSessionQueue>>,
    account_scope: &str,
    session_id: &str,
    messages: &[Message],
) -> AgentDesktopQueueSnapshot {
    let mut queues = queues.lock().await;
    let queue = queues
        .entry(desktop_queue_key(account_scope, session_id))
        .or_insert_with(|| DesktopSessionQueue {
            revision: 0,
            items: VecDeque::new(),
            editing_queue_id: None,
        });
    for message in messages.iter().rev() {
        let Some(message_id) = message.id.clone() else {
            continue;
        };
        queue.items.push_front(AgentQueuedMessage {
            queue_id: next_queue_id(),
            message_id,
            session_id: session_id.to_string(),
            text: message_original_user_text(message).unwrap_or_else(|| message.as_concat_text()),
            attachments: message_image_attachments(message),
            created_ms: unix_ms(),
            message: message.clone(),
        });
    }
    queue.revision = queue.revision.saturating_add(1);
    queue.snapshot()
}

async fn emit_promoted_queue_items(
    events: &AgentRunEventPublisher,
    live_timelines: &LiveTimelines,
    session_id: &str,
    permission_routing: AgentPermissionRouting,
    queued: &[AgentQueuedMessage],
    snapshot: AgentDesktopQueueSnapshot,
) {
    for item in queued {
        let message = queued_user_message(item);
        if let Some(user_item) = message_to_timeline_items(&message, false)
            .into_iter()
            .next()
        {
            record_and_emit_timeline_item(
                events,
                live_timelines,
                session_id,
                permission_routing,
                user_item.clone(),
            )
            .await;
            events
                .publish(AgentRunEvent::QueuePromoted {
                    snapshot: snapshot.clone(),
                    queue_id: item.queue_id.clone(),
                    item: user_item,
                })
                .await;
        }
    }
}

#[cfg(test)]
async fn prepare_desktop_launch(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
    draft_text: &str,
) -> Result<DesktopLaunchPlan, String> {
    let draft_message = (!draft_text.is_empty()).then(|| user_message_from_prompt(draft_text));
    prepare_desktop_launch_message(state, account_scope, session_id, draft_message).await
}

async fn prepare_desktop_launch_message(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
    draft_message: Option<Message>,
) -> Result<DesktopLaunchPlan, String> {
    let leftover = snapshot_desktop_queue(state, account_scope, session_id).await;
    if leftover.items.is_empty() {
        let Some(draft_message) = draft_message else {
            return Err("Prompt cannot be empty".to_string());
        };
        return Ok(DesktopLaunchPlan {
            launch_messages: vec![draft_message],
            consume_queue_ids: Vec::new(),
            queue: leftover,
        });
    }
    // The draft rides along after the leftover chips without entering the
    // queue. Enqueuing it here would leave it staged if setup fails before
    // the queue is consumed, and a retry would then send it twice.
    let launch_messages = leftover
        .items
        .iter()
        .map(queued_user_message)
        .chain(draft_message)
        .collect();
    Ok(DesktopLaunchPlan {
        launch_messages,
        consume_queue_ids: leftover
            .items
            .iter()
            .map(|item| item.queue_id.clone())
            .collect(),
        queue: leftover,
    })
}

#[cfg(test)]
async fn enqueue_desktop_queue_item(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
    text: &str,
) -> Result<(AgentQueuedMessage, AgentDesktopQueueSnapshot), String> {
    enqueue_desktop_queue_message(
        state,
        account_scope,
        session_id,
        text,
        user_message_from_prompt(text),
    )
    .await
}

async fn enqueue_desktop_queue_message(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
    text: &str,
    message: Message,
) -> Result<(AgentQueuedMessage, AgentDesktopQueueSnapshot), String> {
    if text.len() > MAX_DESKTOP_QUEUE_TEXT_BYTES {
        return Err("Queued Agent message is too large".to_string());
    }
    let message_id = message
        .id
        .clone()
        .ok_or_else(|| "Failed to identify queued Agent message".to_string())?;
    let queued = AgentQueuedMessage {
        queue_id: next_queue_id(),
        message_id,
        session_id: session_id.to_string(),
        text: text.to_string(),
        attachments: message_image_attachments(&message),
        created_ms: unix_ms(),
        message,
    };
    let mut queues = state.desktop_queues.lock().await;
    let queue = queues
        .entry(desktop_queue_key(account_scope, session_id))
        .or_insert_with(|| DesktopSessionQueue {
            revision: 0,
            items: VecDeque::new(),
            editing_queue_id: None,
        });
    if queue.items.len() >= MAX_DESKTOP_QUEUE_ITEMS {
        return Err("Agent task already has too many queued messages".to_string());
    }
    queue.revision = queue.revision.saturating_add(1);
    queue.items.push_back(queued.clone());
    Ok((queued, queue.snapshot()))
}

async fn remove_desktop_queue_item(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
    queue_id: &str,
) -> Result<(AgentQueuedMessage, AgentDesktopQueueSnapshot), String> {
    let mut queues = state.desktop_queues.lock().await;
    let Some(queue) = queues.get_mut(&desktop_queue_key(account_scope, session_id)) else {
        return Err("Queued Agent message is no longer available".to_string());
    };
    let Some(index) = queue
        .items
        .iter()
        .position(|item| item.queue_id == queue_id)
    else {
        return Err("Queued Agent message has already been sent".to_string());
    };
    let removed = queue
        .items
        .remove(index)
        .expect("queue index was just resolved");
    if queue.editing_queue_id.as_deref() == Some(removed.queue_id.as_str()) {
        queue.editing_queue_id = None;
    }
    queue.revision = queue.revision.saturating_add(1);
    Ok((removed, queue.snapshot()))
}

async fn update_desktop_queue_item(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
    queue_id: &str,
    text: &str,
) -> Result<(AgentQueuedMessage, AgentDesktopQueueSnapshot), String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Prompt cannot be empty".to_string());
    }
    if text.len() > MAX_DESKTOP_QUEUE_TEXT_BYTES {
        return Err("Queued Agent message is too large".to_string());
    }
    let mut queues = state.desktop_queues.lock().await;
    let Some(queue) = queues.get_mut(&desktop_queue_key(account_scope, session_id)) else {
        return Err("Queued Agent message is no longer available".to_string());
    };
    let Some(item) = queue
        .items
        .iter_mut()
        .find(|item| item.queue_id == queue_id)
    else {
        return Err("Queued Agent message has already been sent".to_string());
    };
    item.text = text.to_string();
    replace_queued_message_text(&mut item.message, text);
    // Only the chip being updated leaves edit mode; an edit in progress on
    // another chip must keep holding promotion off.
    if queue.editing_queue_id.as_deref() == Some(queue_id) {
        queue.editing_queue_id = None;
    }
    queue.revision = queue.revision.saturating_add(1);
    Ok((item.clone(), queue.snapshot()))
}

async fn begin_desktop_queue_edit(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
    queue_id: &str,
) -> Result<(), String> {
    let mut queues = state.desktop_queues.lock().await;
    let Some(queue) = queues.get_mut(&desktop_queue_key(account_scope, session_id)) else {
        return Err("Queued Agent message is no longer available".to_string());
    };
    if !queue.items.iter().any(|item| item.queue_id == queue_id) {
        return Err("Queued Agent message has already been sent".to_string());
    }
    queue.editing_queue_id = Some(queue_id.to_string());
    Ok(())
}

async fn end_desktop_queue_edit(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
    queue_id: &str,
) -> Result<(), String> {
    let mut queues = state.desktop_queues.lock().await;
    let Some(queue) = queues.get_mut(&desktop_queue_key(account_scope, session_id)) else {
        return Ok(());
    };
    if queue.editing_queue_id.as_deref() == Some(queue_id) {
        queue.editing_queue_id = None;
    }
    Ok(())
}

async fn take_all_desktop_queue_items_from_map(
    queues: &Mutex<HashMap<(String, String), DesktopSessionQueue>>,
    account_scope: &str,
    session_id: &str,
) -> Option<(Vec<AgentQueuedMessage>, AgentDesktopQueueSnapshot)> {
    let mut queues = queues.lock().await;
    let key = desktop_queue_key(account_scope, session_id);
    let queue = queues.get_mut(&key)?;
    if queue.items.is_empty() || queue.editing_queue_id.is_some() {
        return None;
    }
    let items: Vec<AgentQueuedMessage> = queue.items.drain(..).collect();
    queue.revision = queue.revision.saturating_add(1);
    Some((items, queue.snapshot()))
}

async fn clear_desktop_queue(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
) -> AgentDesktopQueueSnapshot {
    clear_desktop_queue_in_map(&state.desktop_queues, account_scope, session_id).await
}

async fn clear_desktop_queue_in_map(
    queues: &Mutex<HashMap<(String, String), DesktopSessionQueue>>,
    account_scope: &str,
    session_id: &str,
) -> AgentDesktopQueueSnapshot {
    let mut queues = queues.lock().await;
    let Some(mut queue) = queues.remove(&desktop_queue_key(account_scope, session_id)) else {
        return empty_desktop_queue_snapshot();
    };
    queue.revision = queue.revision.saturating_add(1);
    queue.items.clear();
    queue.snapshot()
}

async fn publish_desktop_queue_changed(
    state: &MapleAgentService,
    account_scope: &str,
    session_id: &str,
    snapshot: AgentDesktopQueueSnapshot,
) {
    let events = {
        let runtime = state.inner.lock().await;
        runtime.as_ref().and_then(|current| {
            if current.account_scope != account_scope {
                return None;
            }
            current
                .active_runs
                .values()
                .find(|run| run.session_id == session_id)
                .map(|run| run.events.clone())
        })
    };
    if let Some(events) = events {
        events.publish(AgentRunEvent::QueueChanged(snapshot)).await;
    }
}

fn project_has_active_session_run(
    session_roots: &HashMap<String, String>,
    active_session_ids: &HashSet<String>,
    project_root: &str,
) -> bool {
    active_session_ids.iter().any(|session_id| {
        session_roots
            .get(session_id)
            .is_some_and(|root| root == project_root)
    })
}

fn apply_project_root_removal(
    config: &mut AgentConfig,
    project_root: &str,
    fallback_path: Option<&str>,
) -> Result<(), String> {
    if fallback_path.is_some_and(|fallback| {
        config
            .removed_project_roots
            .iter()
            .any(|removed| removed == fallback)
    }) {
        return Err("Project fallback is already removed".to_string());
    }
    if !config
        .removed_project_roots
        .iter()
        .any(|removed| removed == project_root)
    {
        config.removed_project_roots.push(project_root.to_string());
    }
    if config.default_project_root.as_deref() == Some(project_root) {
        config.default_project_root = fallback_path.map(ToOwned::to_owned);
    }
    Ok(())
}

fn update_runtime_project_root_after_removal(
    runtime_project_root: &mut PathBuf,
    removed_project_root: &str,
    fallback_path: Option<&str>,
) {
    if runtime_project_root == Path::new(removed_project_root) {
        *runtime_project_root = fallback_path.map(PathBuf::from).unwrap_or_default();
    }
}

fn ensure_session_project_root_is_visible(
    project_root: &Path,
    removed_project_roots: &[String],
) -> Result<(), String> {
    let project_root = path_string(project_root);
    if project_root.is_empty()
        || removed_project_roots
            .iter()
            .any(|removed| removed == &project_root)
    {
        return Err("Select a project folder before creating an Agent task".to_string());
    }
    Ok(())
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<(), anyhow::Error> {
    crate::private_file::write_private_json(path, value)?;
    Ok(())
}

fn write_device_local_json_file<T: Serialize + ?Sized>(
    path: &Path,
    value: &T,
) -> Result<(), anyhow::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Device-local Agent data path has no parent"))?;
    fs::create_dir_all(parent)?;
    set_owner_only_dir_permissions(parent);
    crate::private_file::write_private_json(path, value)?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) {}

#[cfg(unix)]
fn set_owner_only_dir_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_owner_only_dir_permissions(_path: &Path) {}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// Cross-module integration-test fixtures. `mod tests` keeps its own local
/// fixtures for unit scope; adapters that live outside this module (for
/// example the ACP stdio-surface tests) start their runtime here so the
/// hermetic wiring exists in exactly one place.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) struct NoopEventSink;
    impl AgentEventSink for NoopEventSink {
        fn emit(&self, _event: &AgentServiceEvent) {}
    }

    /// A started hermetic Agent runtime plus the handle a long-lived
    /// adapter (ACP, proxy) would retain. The Maple API session points at
    /// an unreachable endpoint, so model-catalog requests fail and fall
    /// back to the configured default model.
    pub(crate) struct StartedTestAgent {
        pub(crate) handle: AgentRuntimeHandle,
        pub(crate) project_root: PathBuf,
        #[allow(dead_code)]
        pub(crate) root: PathBuf,
    }

    pub(crate) async fn started_agent_runtime(label: &str) -> StartedTestAgent {
        let root = std::env::temp_dir().join(format!(
            "maple-agent-test-support-{label}-{}-{}",
            std::process::id(),
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let paths =
            AgentPathLayout::from_app_roots(root.join("app-config"), root.join("app-local-data"));
        let user_id = format!("{label}-user");
        // The account history dir, not an ad-hoc one: the prompt path opens
        // its own manager over that dir (for image attachments), so the
        // runtime must own the same store the way the started app does.
        let session_manager = account_session_manager(&paths, &user_id)
            .expect("test history dir should be creatable");
        let service = MapleAgentService::new(MapleAgentHostResources::new(
            paths,
            Arc::new(NoopEventSink),
            AgentToolContextSpec::default(),
            "You are a test agent.".to_string(),
        ));
        let project_root = root.join("project");
        fs::create_dir_all(&project_root).expect("project directory should be created");
        let permission_manager = Arc::new(PermissionManager::new(root.join("permissions")));
        let goose_config = GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        );
        let agent_manager = Arc::new(
            AgentManager::new(goose_config, None)
                .await
                .expect("test Agent manager should start"),
        );
        let runtime = AgentRuntime {
            agent_manager,
            session_manager,
            maple_api_session: crate::maple_api::test_maple_api_session(&user_id),
            active_runs: HashMap::new(),
            session_title_tasks: HashMap::new(),
            session_tool_contexts: HashMap::new(),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            web_tool_state: Arc::new(WebToolState::default()),
            project_root: project_root.clone(),
            model: DEFAULT_AGENT_MODEL.to_string(),
            mode: DEFAULT_GOOSE_MODE.to_string(),
            account_scope: account_scope(&user_id).expect("test user id should scope"),
            lifetime: CancellationToken::new(),
        };
        *service.inner.lock().await = Some(runtime);
        let handle = service
            .handle_for_user(&user_id)
            .await
            .expect("test handle should bind to the started runtime");
        StartedTestAgent {
            handle,
            project_root,
            root,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_mcp_url_accepts_ipv6_loopback() {
        assert!(validate_transient_mcp_url("http://[::1]:9000/mcp", "s").is_ok());
        assert!(validate_transient_mcp_url("http://[2606:4700::1111]:9000/mcp", "s").is_err());
    }

    #[test]
    fn context_limit_requires_present_and_agreeing_metadata() {
        // Absent metadata means unknown.
        assert_eq!(reconcile_context_limit(None, None), None);
        // Exactly one field wins.
        assert_eq!(reconcile_context_limit(Some(256_000), None), Some(256_000));
        assert_eq!(reconcile_context_limit(None, Some(128_000)), Some(128_000));
        // Equal values agree.
        assert_eq!(
            reconcile_context_limit(Some(200_000), Some(200_000)),
            Some(200_000)
        );
        // Disagreeing values are unknown, matching Maple's frontend rule.
        assert_eq!(reconcile_context_limit(Some(128_000), Some(200_000)), None);
        // Zero or overflow is not a usable limit.
        assert_eq!(reconcile_context_limit(Some(0), None), None);
        assert_eq!(reconcile_context_limit(Some(u64::MAX), None), None);
    }

    use goose_providers::base::{MessageStream, Provider, stream_from_single_message};
    use goose_providers::conversation::token_usage::{ProviderUsage, Usage};
    use goose_providers::errors::ProviderError;
    use goose_providers::model::ModelConfig;
    use rmcp::model::{Annotations, Role as McpRole, TextContent};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::AtomicUsize;

    struct NoopAgentEventSink;

    impl AgentEventSink for NoopAgentEventSink {
        fn emit(&self, _event: &AgentServiceEvent) {}
    }

    #[derive(Default)]
    struct RecordingAgentEventSink {
        events: std::sync::Mutex<Vec<AgentServiceEvent>>,
    }

    impl AgentEventSink for RecordingAgentEventSink {
        fn emit(&self, event: &AgentServiceEvent) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.clone());
        }
    }

    struct InertMapleTransport;

    #[async_trait::async_trait]
    impl provider::MapleInferenceTransport for InertMapleTransport {
        async fn send_inference_request(
            self: Arc<Self>,
            _request: maple_sdk::InferenceRequest,
            _cancel_token: CancellationToken,
        ) -> maple_sdk::Result<maple_sdk::InferenceResponse> {
            Err(maple_sdk::Error::Other(
                "test transport should not be called".to_string(),
            ))
        }
    }

    #[derive(Clone, Copy)]
    enum TerminalMapleFailure {
        Session,
        AttestationVerification,
    }

    struct TerminalMapleTransport(TerminalMapleFailure);

    #[async_trait::async_trait]
    impl provider::MapleInferenceTransport for TerminalMapleTransport {
        async fn send_inference_request(
            self: Arc<Self>,
            _request: maple_sdk::InferenceRequest,
            _cancel_token: CancellationToken,
        ) -> maple_sdk::Result<maple_sdk::InferenceResponse> {
            match self.0 {
                TerminalMapleFailure::Session => Err(maple_sdk::Error::Session(
                    "private exhausted session detail".to_string(),
                )),
                TerminalMapleFailure::AttestationVerification => {
                    Err(maple_sdk::Error::AttestationVerificationFailed(
                        "private attestation detail".to_string(),
                    ))
                }
            }
        }
    }

    struct SessionTitleTestProvider {
        calls: Arc<AtomicUsize>,
        response: &'static str,
    }

    #[async_trait::async_trait]
    impl Provider for SessionTitleTestProvider {
        fn get_name(&self) -> &str {
            MAPLE_PROVIDER_NAME
        }

        async fn stream(
            &self,
            _model_config: &ModelConfig,
            _system: &str,
            _messages: &[Message],
            _tools: &[rmcp::model::Tool],
        ) -> Result<MessageStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(stream_from_single_message(
                Message::assistant().with_text(self.response),
                ProviderUsage::new("title-test".to_string(), Usage::default()),
            ))
        }
    }

    #[derive(Clone, Debug)]
    struct SessionTitleRequestCapture {
        model_name: String,
        temperature: Option<f32>,
        max_tokens: Option<i32>,
        reasoning: Option<bool>,
        request_params_present: bool,
        system: String,
        user: String,
        tool_count: usize,
    }

    struct CapturingSessionTitleProvider {
        capture: Arc<std::sync::Mutex<Option<SessionTitleRequestCapture>>>,
    }

    #[async_trait::async_trait]
    impl Provider for CapturingSessionTitleProvider {
        fn get_name(&self) -> &str {
            MAPLE_PROVIDER_NAME
        }

        async fn stream(
            &self,
            model_config: &ModelConfig,
            system: &str,
            messages: &[Message],
            tools: &[rmcp::model::Tool],
        ) -> Result<MessageStream, ProviderError> {
            let user = messages
                .first()
                .map(Message::as_concat_text)
                .unwrap_or_default();
            *self
                .capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(SessionTitleRequestCapture {
                    model_name: model_config.model_name.clone(),
                    temperature: model_config.temperature,
                    max_tokens: model_config.max_tokens,
                    reasoning: model_config.reasoning,
                    request_params_present: model_config.request_params.is_some(),
                    system: system.to_string(),
                    user,
                    tool_count: tools.len(),
                });
            Ok(stream_from_single_message(
                Message::assistant().with_text("Bounded Llama Title"),
                ProviderUsage::new("captured-title-test".to_string(), Usage::default()),
            ))
        }
    }

    struct BlockingSessionTitleProvider {
        cancel_token: CancellationToken,
        cancelled: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Provider for BlockingSessionTitleProvider {
        fn get_name(&self) -> &str {
            MAPLE_PROVIDER_NAME
        }

        async fn stream(
            &self,
            _model_config: &ModelConfig,
            _system: &str,
            _messages: &[Message],
            _tools: &[rmcp::model::Tool],
        ) -> Result<MessageStream, ProviderError> {
            self.cancel_token.cancelled().await;
            self.cancelled.store(true, Ordering::SeqCst);
            Err(ProviderError::ExecutionError(
                "title request cancelled".to_string(),
            ))
        }
    }

    struct DelayedSessionTitleProvider {
        title_started: Arc<tokio::sync::Notify>,
        title_release: Arc<tokio::sync::Notify>,
        title_cancel_token: CancellationToken,
    }

    #[async_trait::async_trait]
    impl Provider for DelayedSessionTitleProvider {
        fn get_name(&self) -> &str {
            MAPLE_PROVIDER_NAME
        }

        async fn stream(
            &self,
            _model_config: &ModelConfig,
            system: &str,
            _messages: &[Message],
            _tools: &[rmcp::model::Tool],
        ) -> Result<MessageStream, ProviderError> {
            let response = if system == SESSION_TITLE_SYSTEM_PROMPT {
                self.title_started.notify_one();
                tokio::select! {
                    biased;
                    _ = self.title_cancel_token.cancelled() => {
                        return Err(ProviderError::ExecutionError(
                            "title request cancelled".to_string(),
                        ));
                    }
                    _ = self.title_release.notified() => "Friendly Check-In",
                }
            } else {
                "I'm doing well!"
            };
            Ok(stream_from_single_message(
                Message::assistant().with_text(response),
                ProviderUsage::new("delayed-title-test".to_string(), Usage::default()),
            ))
        }
    }

    async fn session_title_test_context(
        label: &str,
        provider: Arc<dyn Provider>,
    ) -> (PathBuf, Arc<SessionManager>, Agent, Session) {
        let test_root = recent_roots_test_dir(label);
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(SessionManager::new(test_root.join("sessions")));
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root,
                DEFAULT_AGENT_SESSION_TITLE.to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let agent = Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        ));
        agent
            .update_provider(provider, ModelConfig::new(DEFAULT_AGENT_MODEL), &session.id)
            .await
            .unwrap();
        session_manager
            .update(&session.id)
            .system_generated_name("Explain reactive titles".to_string())
            .apply()
            .await
            .unwrap();
        session_manager
            .add_message(
                &session.id,
                &Message::user().with_text("Explain how reactive Agent titles work"),
            )
            .await
            .unwrap();
        (test_root, session_manager, agent, session)
    }

    async fn assert_terminal_maple_failure_fails_before_goose_message_projection(
        label: &str,
        failure: TerminalMapleFailure,
        expected_error: &str,
    ) {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let maple_provider = Arc::new(provider::MapleProvider::new(Arc::new(
            TerminalMapleTransport(failure),
        )));
        let (test_root, session_manager, agent, session) =
            session_title_test_context(label, maple_provider).await;
        let run_id = format!("{label}-run");
        let (events, _run_events) = AgentRunEventPublisher::new(
            AgentEventDispatcher::new(sink.clone()),
            session.id.clone(),
            run_id.clone(),
            AgentHostEventPolicy::Publish,
        );
        let cancellation = CancellationToken::new();
        let prompt = "Trigger a terminal Maple connection failure";

        let result = provider::with_run_cancellation(
            cancellation.clone(),
            run_agent_prompt(AgentPromptRun {
                events,
                agent: Arc::new(agent),
                session_manager: Arc::clone(&session_manager),
                session_title_lifecycle: Arc::new(Mutex::new(())),
                live_timelines: Arc::new(Mutex::new(HashMap::new())),
                subagents: Arc::new(Mutex::new(HashMap::new())),
                subagent_host: None,
                session_id: session.id.clone(),
                user_message: Message::user().with_text(prompt).with_generated_id(),
                permission_modes: Arc::new(Mutex::new(HashMap::new())),
                web_tool_state: Arc::new(WebToolState::default()),
                web_permission_context: WebPermissionContext::from_user_prompt(prompt),
                cancel_token: cancellation,
                session_title_start: None,
                pending_permissions: Arc::new(Mutex::new(HashMap::new())),
                issued_permission_ids: Arc::new(Mutex::new(HashSet::new())),
                cancelled_permission_ids: Arc::new(Mutex::new(HashSet::new())),
                run_id,
                permission_routing: AgentPermissionRouting::Desktop,
                steered_unacked: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .await;

        let error = match result {
            Ok(_) => panic!("terminal Maple connection error should fail the prompt"),
            Err(error) => error,
        };
        assert_eq!(error, expected_error);
        let emitted = sink
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!emitted.iter().any(|event| matches!(
            event,
            AgentServiceEvent::Run {
                event: AgentRunEvent::TimelineItem(_),
                ..
            }
        )));

        drop(emitted);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn terminal_maple_connection_error_fails_before_goose_message_projection() {
        assert_terminal_maple_failure_fails_before_goose_message_projection(
            "terminal-maple-error",
            TerminalMapleFailure::Session,
            provider::SECURE_CONNECTION_ERROR_MESSAGE,
        )
        .await;
    }

    #[tokio::test]
    async fn terminal_maple_attestation_error_fails_before_goose_message_projection() {
        assert_terminal_maple_failure_fails_before_goose_message_projection(
            "terminal-maple-attestation-error",
            TerminalMapleFailure::AttestationVerification,
            provider::ATTESTATION_VERIFICATION_ERROR_MESSAGE,
        )
        .await;
    }

    fn recent_roots_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "maple-agent-recent-roots-{label}-{}-{}",
            std::process::id(),
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn agent_service_test_context(
        label: &str,
        event_sink: Arc<dyn AgentEventSink>,
    ) -> (PathBuf, AgentPathLayout, MapleAgentService) {
        let test_root = recent_roots_test_dir(label);
        let paths = AgentPathLayout::from_app_roots(
            test_root.join("app-config"),
            test_root.join("app-local-data"),
        );
        let service = MapleAgentService::new(MapleAgentHostResources::new(
            paths.clone(),
            event_sink,
            AgentToolContextSpec::default(),
            "You are a test agent.".to_string(),
        ));
        (test_root, paths, service)
    }

    async fn tool_context_cleanup_test_context(
        label: &str,
    ) -> (
        PathBuf,
        MapleAgentService,
        Arc<SessionManager>,
        PathBuf,
        Arc<str>,
    ) {
        let (test_root, _paths, service) =
            agent_service_test_context(label, Arc::new(NoopAgentEventSink));
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).expect("project directory should be created");
        let session_manager = Arc::new(SessionManager::new(test_root.join("sessions")));
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let goose_config = GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        );
        let agent_manager = Arc::new(
            AgentManager::new(goose_config, None)
                .await
                .expect("test Agent manager should start"),
        );
        let user_id = format!("{label}-user");
        let account_scope: Arc<str> = Arc::from(account_scope(&user_id).unwrap());
        let runtime = AgentRuntime {
            agent_manager,
            session_manager: Arc::clone(&session_manager),
            maple_api_session: crate::maple_api::test_maple_api_session(&user_id),
            active_runs: HashMap::new(),
            session_title_tasks: HashMap::new(),
            session_tool_contexts: HashMap::new(),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            web_tool_state: Arc::new(WebToolState::default()),
            project_root: project_root.clone(),
            model: DEFAULT_AGENT_MODEL.to_string(),
            mode: DEFAULT_GOOSE_MODE.to_string(),
            account_scope: account_scope.to_string(),
            lifetime: CancellationToken::new(),
        };
        *service.inner.lock().await = Some(runtime);
        (
            test_root,
            service,
            session_manager,
            project_root,
            account_scope,
        )
    }

    fn persisted_session_except_title(session: &Session) -> Value {
        let mut value = serde_json::to_value(session).unwrap();
        let fields = value.as_object_mut().unwrap();
        fields.remove("name");
        fields.remove("user_set_name");
        fields.remove("updated_at");
        value
    }

    async fn python_lifecycle_fixture(
        label: &str,
    ) -> (
        PathBuf,
        AgentRuntimeHandle,
        Session,
        SharedAgentToolContext,
        Arc<PythonTaskBinding>,
    ) {
        let (root, service, manager, project_root, scope) =
            tool_context_cleanup_test_context(label).await;
        let session = manager
            .create_session(
                project_root,
                "Python lifecycle".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let (context, binding) = {
            let mut guard = service.inner.lock().await;
            let runtime = guard.as_mut().unwrap();
            let context = resolve_session_tool_context(
                &mut runtime.session_tool_contexts,
                &scope,
                &session.id,
                None,
                &AgentToolContextSpec::default(),
            )
            .unwrap();
            let binding = resolve_session_python(
                &service,
                runtime,
                &session,
                &context,
                AgentPermissionRouting::Desktop,
            )
            .unwrap();
            (context, binding)
        };
        let handle = service
            .handle_for_user(&format!("{label}-user"))
            .await
            .unwrap();
        (root, handle, session, context, binding)
    }

    #[tokio::test]
    async fn python_binding_is_retained_across_runs_and_rejects_root_changes() {
        let (root, handle, session, context, original) =
            python_lifecycle_fixture("python-reuse").await;
        {
            let mut guard = handle.service.inner.lock().await;
            let runtime = guard.as_mut().unwrap();
            let second = resolve_session_python(
                &handle.service,
                runtime,
                &session,
                &context,
                AgentPermissionRouting::Desktop,
            )
            .unwrap();
            assert!(Arc::ptr_eq(&original, &second));
            let mut changed = session.clone();
            changed.working_dir = root.join("another-root");
            assert!(
                resolve_session_python(
                    &handle.service,
                    runtime,
                    &changed,
                    &context,
                    AgentPermissionRouting::Desktop,
                )
                .is_err()
            );
        }
        assert!(!context.is_revoked());
        assert!(handle.service.python_runtime.snapshot().holders.is_empty());
        assert!(
            !handle
                .python_status(session.id.clone())
                .await
                .unwrap()
                .resettable
        );
        handle.reset_python(session.id).await.unwrap();
        assert!(
            !original.is_closed(),
            "empty reset preserves the logical task binding"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn python_archive_fences_prepared_binding_and_allows_later_fresh_use() {
        let (root, handle, session, context, original) =
            python_lifecycle_fixture("python-archive").await;
        let summary = handle
            .set_session_archived(session.id.clone(), true)
            .await
            .unwrap();
        assert!(summary.archived);
        assert!(original.is_closed());
        assert!(
            !context.is_revoked(),
            "archive must preserve the surrounding tool context"
        );
        let replacement = {
            let mut guard = handle.service.inner.lock().await;
            resolve_session_python(
                &handle.service,
                guard.as_mut().unwrap(),
                &session,
                &context,
                AgentPermissionRouting::Desktop,
            )
            .unwrap()
        };
        assert!(!Arc::ptr_eq(&original, &replacement));
        assert!(!replacement.is_closed());
        assert_eq!(original.key(), replacement.key());
        assert!(
            replacement
                .status()
                .state_loss_reason
                .unwrap()
                .contains("archived")
        );
        assert!(handle.service.python_runtime.snapshot().holders.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn python_reset_rejects_preparing_run_and_external_lease() {
        let (root, handle, session, _context, original) =
            python_lifecycle_fixture("python-reset-authority").await;
        let manager = Arc::clone(
            &handle
                .service
                .inner
                .lock()
                .await
                .as_ref()
                .unwrap()
                .agent_manager,
        );
        manager
            .try_register_cancel_token(&session.id, CancellationToken::new())
            .await
            .unwrap();
        let error = handle.reset_python(session.id.clone()).await.unwrap_err();
        assert!(error.contains("preparing"));
        assert!(!original.is_closed());
        manager.unregister_cancel_token(&session.id).await;
        {
            let mut guard = handle.service.inner.lock().await;
            guard
                .as_mut()
                .unwrap()
                .session_tool_contexts
                .get_mut(&session.id)
                .unwrap()
                .owner = AgentToolContextOwner::Leased;
        }
        let error = handle.reset_python(session.id.clone()).await.unwrap_err();
        assert!(error.contains("another Agent surface"));
        assert!(!handle.python_status(session.id).await.unwrap().resettable);
        assert!(handle.service.python_runtime.snapshot().holders.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn python_project_removal_fences_only_affected_prepared_bindings() {
        let (root, handle, session, context, original) =
            python_lifecycle_fixture("python-project-removal").await;
        handle
            .remove_project_root(path_string(&session.working_dir), None)
            .await
            .unwrap();
        assert!(original.is_closed());
        assert!(!context.is_revoked());
        assert!(
            original
                .status()
                .state_loss_reason
                .unwrap()
                .contains("project")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn python_menu_reset_confirms_native_cleanup_and_completed_run_stop_preserves_state() {
        let (root, handle, session, context, _original) =
            python_lifecycle_fixture("python-native-menu").await;
        let manifest = std::env::var_os("MAPLE_CODE_MODE_RUNTIME_MANIFEST")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/debug/runtime/python/runtime.json")
            });
        let package = maple_code_mode::PackagedPython::from_manifest(manifest)
            .expect("run `just python-prepare` before native Python tests");
        let binding = Arc::new(
            PythonTaskBinding::new(
                handle.service.python_runtime.clone(),
                python_task_key(&handle.account_scope, &session.id),
                session.id.clone(),
                session.working_dir.clone(),
                context.clone(),
            )
            .with_packaged_python(package),
        );
        handle
            .service
            .inner
            .lock()
            .await
            .as_mut()
            .unwrap()
            .session_tool_contexts
            .get_mut(&session.id)
            .unwrap()
            .python = Some(Arc::clone(&binding));
        let call =
            ToolCallContext::new(session.id.clone(), Some(session.working_dir.clone()), None);
        let run = CancellationToken::new();
        let first = binding
            .call(
                code_mode::PythonParams {
                    code: "answer = 42\nanswer".into(),
                    reset: false,
                },
                &call,
                async { None },
                run.clone(),
            )
            .await;
        assert_ne!(first.is_error, Some(true), "{first:?}");
        assert!(
            handle
                .python_status(session.id.clone())
                .await
                .unwrap()
                .resettable
        );
        context.cancel_run(&run);
        let second = binding
            .call(
                code_mode::PythonParams {
                    code: "answer".into(),
                    reset: false,
                },
                &call,
                async { panic!("retained worker must not probe PATH") },
                CancellationToken::new(),
            )
            .await;
        assert_ne!(second.is_error, Some(true), "{second:?}");
        assert_eq!(
            second.structured_content.unwrap()["maple_python"]["value"],
            "42"
        );
        handle.reset_python(session.id.clone()).await.unwrap();
        assert!(!handle.python_status(session.id).await.unwrap().resettable);
        assert!(
            handle.service.python_runtime.snapshot().holders.is_empty(),
            "menu success requires released capacity"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn python_capacity_callback_hides_foreign_and_revoked_task_metadata() {
        let (root, handle, session, context, binding) =
            python_lifecycle_fixture("python-capacity-auth").await;
        let diagnostics = {
            let guard = handle.service.inner.lock().await;
            let current = guard.as_ref().unwrap();
            python_capacity_diagnostics(
                Arc::downgrade(&handle.service.inner),
                handle.account_scope.to_string(),
                session.id.clone(),
                current.session_tool_contexts[&session.id].installation_id,
                context.clone(),
                current.lifetime.clone(),
                Arc::clone(&current.session_manager),
                AgentPermissionRouting::Desktop,
            )
        };
        let holders = vec![
            maple_code_mode::Holder {
                key: binding.key().into(),
                generation: 1,
                phase: maple_code_mode::WorkerPhase::Idle,
            },
            maple_code_mode::Holder {
                key: "foreign-account:private-title".into(),
                generation: 2,
                phase: maple_code_mode::WorkerPhase::CleanupPending,
            },
        ];
        let visible = diagnostics(holders.clone()).await;
        assert!(visible.contains("Python lifecycle: idle"));
        assert!(!visible.contains("private-title"));
        assert!(visible.contains("1 slot(s)"));
        context.revoke();
        let revoked = diagnostics(holders).await;
        assert!(!revoked.contains("Python lifecycle"));
        assert!(!revoked.contains("private-title"));
        assert!(revoked.contains("2 slot(s)"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn python_capacity_guidance_names_authorized_holders_and_counts_the_rest() {
        let message = format_python_capacity(
            4,
            vec![
                (
                    "Desktop task".into(),
                    AgentToolContextOwner::Maple,
                    maple_code_mode::WorkerPhase::Idle,
                ),
                (
                    "ACP task".into(),
                    AgentToolContextOwner::Leased,
                    maple_code_mode::WorkerPhase::CleanupPending,
                ),
            ],
            AgentPermissionRouting::Desktop,
        );
        assert!(message.contains("Desktop task: idle"));
        assert!(message.contains("Reset Python"));
        assert!(message.contains("ACP task: cleanup pending"));
        assert!(message.contains("owning client can close"));
        assert!(message.contains("2 slot(s) belong to other or retired owners"));
        let acp = format_python_capacity(4, Vec::new(), AgentPermissionRouting::CallingSurface);
        assert!(!acp.contains("Desktop task"));
        assert!(acp.contains("session/close"));
        assert!(acp.contains("\"reset\":true"));
    }

    fn test_project_path(label: &str) -> String {
        std::env::temp_dir()
            .join(format!("maple-agent-project-{label}"))
            .to_string_lossy()
            .to_string()
    }

    fn test_recent_root(label: &str, last_used_ms: u128) -> RecentProjectRoot {
        project_root_record(test_project_path(label), last_used_ms)
    }

    fn recent_root_paths(roots: &[RecentProjectRoot]) -> Vec<String> {
        roots.iter().map(|root| root.path.clone()).collect()
    }

    fn test_permission_request(request_id: &str) -> AgentPermissionRequest {
        AgentPermissionRequest {
            request_id: request_id.to_string(),
            tool_name: "shell".to_string(),
            arguments: serde_json::Map::from_iter([(
                "command".to_string(),
                Value::String("git status --short".to_string()),
            )]),
            prompt: Some("Run this command?".to_string()),
        }
    }

    fn test_pending_permission(
        run_id: &str,
        routing: AgentPermissionRouting,
        request_id: &str,
    ) -> PendingAgentPermission {
        PendingAgentPermission {
            run_id: run_id.to_string(),
            routing,
            request: test_permission_request(request_id),
        }
    }

    fn test_live_timeline(
        routing: AgentPermissionRouting,
        timeline: LiveTimeline,
    ) -> LiveTimelineEntry {
        LiveTimelineEntry { routing, timeline }
    }

    #[test]
    fn desktop_status_excludes_calling_surface_runs() {
        let status = active_run_status([
            (
                "desktop-run",
                "desktop-session",
                AgentPermissionRouting::Desktop,
            ),
            (
                "acp-run",
                "acp-session",
                AgentPermissionRouting::CallingSurface,
            ),
        ]);

        assert_eq!(
            status,
            HashMap::from([("desktop-session".to_string(), "desktop-run".to_string())])
        );
    }

    /// A task whose persisted MCP servers stall must not hold Maple's global
    /// lifecycle fences: Stop, status, and task switching stay responsive, and
    /// cancelling the run aborts the stalled startup instead of waiting out the
    /// per-server MCP timeout.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stalled_mcp_startup_releases_the_lifecycle_fences() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) = agent_service_test_context("stalled-mcp-fence", sink);
        let user_id = "stalled-mcp-fence-user";
        let account_scope = account_scope(user_id).unwrap();
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(account_session_manager(&paths, user_id).unwrap());
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root.clone(),
                "Stalled MCP task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        // `sleep` never speaks MCP, so the initialize handshake stalls until the
        // extension's own timeout, exactly like a wedged real server does.
        let mut extension_data = session.extension_data.clone();
        goose::session::EnabledExtensionsState::new(vec![ExtensionConfig::Stdio {
            name: "stalled".to_string(),
            description: "stalled test server".to_string(),
            cmd: "sleep".to_string(),
            args: vec!["120".to_string()],
            envs: Default::default(),
            env_keys: Vec::new(),
            timeout: Some(DEFAULT_MCP_TIMEOUT_SECONDS),
            cwd: None,
            bundled: None,
            available_tools: Vec::new(),
        }])
        .to_extension_data(&mut extension_data)
        .unwrap();
        session_manager
            .update(&session.id)
            .extension_data(extension_data)
            .apply()
            .await
            .unwrap();
        let agent_manager = Arc::new(
            AgentManager::new(
                GooseAgentConfig::new(
                    Arc::clone(&session_manager),
                    permission_manager,
                    None,
                    GooseMode::SmartApprove,
                    true,
                    GoosePlatform::GooseDesktop,
                ),
                Some(2),
            )
            .await
            .unwrap(),
        );
        *state.inner.lock().await = Some(AgentRuntime {
            agent_manager: Arc::clone(&agent_manager),
            session_manager: Arc::clone(&session_manager),
            maple_api_session: crate::maple_api::test_maple_api_session(user_id),
            active_runs: HashMap::new(),
            session_title_tasks: HashMap::new(),
            session_tool_contexts: HashMap::new(),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            web_tool_state: Arc::new(WebToolState::default()),
            project_root: project_root.clone(),
            model: DEFAULT_AGENT_MODEL.to_string(),
            mode: DEFAULT_GOOSE_MODE.to_string(),
            account_scope: account_scope.clone(),
            lifetime: CancellationToken::new(),
        });

        let handle = state.handle_for_user(user_id).await.unwrap();
        let send_session_id = session.id.clone();
        let send_handle = handle.clone();
        let send = tokio::spawn(async move {
            send_handle
                .send_message(AgentSendMessageRequest {
                    session_id: send_session_id,
                    text: "start the stalled task".to_string(),
                    model: None,
                    context_limit: None,
                    mode: None,
                    vision_capable: false,
                    steer: false,
                    queue_id: None,
                    attachments: Vec::new(),
                })
                .await
        });

        // The send has claimed the session, yet neither fence is held.
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if agent_manager.is_session_busy(&session.id).await
                    && state.runtime_lifecycle.try_lock().is_ok()
                    && state.session_lifecycle.try_lock().is_ok()
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("MCP startup must not hold the lifecycle fences");

        tokio::time::timeout(std::time::Duration::from_secs(10), handle.status())
            .await
            .expect("status must answer while MCP servers are starting")
            .unwrap();

        // Stop must both answer and end the stalled startup, rather than
        // leaving it to run out the MCP timeout.
        tokio::time::timeout(std::time::Duration::from_secs(10), handle.stop())
            .await
            .expect("stop must answer while MCP servers are starting")
            .unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(30), send)
            .await
            .expect("a cancelled MCP startup must not wait out the MCP timeout")
            .unwrap();
        let error = match error {
            Ok(_) => panic!("a cancelled MCP startup must fail the send"),
            Err(error) => error,
        };
        assert_eq!(error, MCP_STARTUP_CANCELLED_ERROR);
        assert!(!agent_manager.is_session_busy(&session.id).await);
        assert!(state.inner.lock().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn desktop_send_during_active_run_stages_a_native_queue() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) =
            agent_service_test_context("desktop-steer-queue", sink.clone());
        let user_id = "desktop-steer-queue-user";
        let account_scope = account_scope(user_id).unwrap();
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(account_session_manager(&paths, user_id).unwrap());
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root.clone(),
                "Steer task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            Arc::clone(&permission_manager),
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));
        let agent_manager = Arc::new(
            AgentManager::new(
                GooseAgentConfig::new(
                    Arc::clone(&session_manager),
                    permission_manager,
                    None,
                    GooseMode::SmartApprove,
                    true,
                    GoosePlatform::GooseDesktop,
                ),
                Some(2),
            )
            .await
            .unwrap(),
        );
        let (run_events, _run_events_rx) = AgentRunEventPublisher::new(
            AgentEventDispatcher::new(sink.clone()),
            session.id.clone(),
            "desktop-steer-run".to_string(),
            AgentHostEventPolicy::Suppress,
        );
        *state.inner.lock().await = Some(AgentRuntime {
            agent_manager,
            session_manager: Arc::clone(&session_manager),
            maple_api_session: crate::maple_api::test_maple_api_session(user_id),
            active_runs: HashMap::from([(
                "desktop-steer-run".to_string(),
                ActiveAgentRun {
                    agent,
                    permission_routing: AgentPermissionRouting::Desktop,
                    token: CancellationToken::new(),
                    tool_context: SharedAgentToolContext::new(AgentToolContextSpec::default()),
                    session_id: session.id.clone(),
                    events: run_events,
                    cancelled_permission_ids: Arc::new(Mutex::new(HashSet::new())),
                    accepting_queue: Arc::new(AtomicBool::new(true)),
                    steered_unacked: Arc::new(Mutex::new(Vec::new())),
                    task_handle: tokio::spawn(async {}),
                },
            )]),
            session_title_tasks: HashMap::new(),
            session_tool_contexts: HashMap::new(),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            web_tool_state: Arc::new(WebToolState::default()),
            project_root,
            model: DEFAULT_AGENT_MODEL.to_string(),
            mode: DEFAULT_GOOSE_MODE.to_string(),
            account_scope: account_scope.clone(),
            lifetime: CancellationToken::new(),
        });

        let handle = state.handle_for_user(user_id).await.unwrap();
        {
            let runtime = state.inner.lock().await;
            let current = runtime.as_ref().expect("runtime");
            assert!(
                current
                    .active_runs
                    .values()
                    .any(|run| { run.session_id == session.id && desktop_run_is_stageable(run) }),
                "test fixture must expose a stageable desktop run"
            );
        }
        let first = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            handle.send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: "also check the tests".to_string(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: false,
                queue_id: None,
                attachments: Vec::new(),
            }),
        )
        .await
        .expect("desktop queue stage should return")
        .unwrap();
        let second = handle
            .send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: "then open the readme".to_string(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: false,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();

        assert_eq!(first.run_id, "desktop-steer-run");
        assert_eq!(second.run_id, "desktop-steer-run");
        let first_queued = first.queued.expect("first mid-run send should stage");
        let second_queued = second.queued.expect("second mid-run send should stage");
        assert_eq!(first_queued.text, "also check the tests");
        assert_eq!(second_queued.text, "then open the readme");
        assert_eq!(
            second
                .queue
                .items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["also check the tests", "then open the readme"]
        );

        let loaded = handle.load_session(session.id.clone()).await.unwrap();
        assert_eq!(loaded.queue.items, second.queue.items);

        let remaining = handle
            .cancel_queued_message(AgentQueueControlRequest {
                session_id: session.id.clone(),
                queue_id: first_queued.queue_id.clone(),
            })
            .await
            .unwrap();
        assert_eq!(
            remaining
                .items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["then open the readme"]
        );

        let edited = handle
            .unqueue_message_for_edit(AgentQueueControlRequest {
                session_id: session.id.clone(),
                queue_id: second_queued.queue_id.clone(),
            })
            .await
            .unwrap();
        assert_eq!(edited.text, "then open the readme");
        assert!(
            handle
                .load_session(session.id.clone())
                .await
                .unwrap()
                .queue
                .items
                .is_empty()
        );

        let restaged = handle
            .send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: "retry after edit".to_string(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: false,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(
            restaged.queued.as_ref().map(|item| item.text.as_str()),
            Some("retry after edit")
        );
        let later = handle
            .send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: "keep me queued".to_string(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: false,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        let first_chip = later.queue.items[0].queue_id.clone();
        let steered = handle
            .send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: String::new(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: true,
                queue_id: Some(first_chip),
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(steered.run_id, "desktop-steer-run");
        assert!(steered.queued.is_none());
        assert_eq!(
            steered
                .queue
                .items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["keep me queued"]
        );
        handle
            .cancel_desktop_run("desktop-steer-run".to_string())
            .await
            .unwrap();
        let after_stop = handle.load_session(session.id.clone()).await.unwrap();
        assert_eq!(
            after_stop
                .queue
                .items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["keep me queued"],
            "Stop must leave staged chips so the user can edit, drop, or send them"
        );

        let launch =
            prepare_desktop_launch(&state, &account_scope, &session.id, "draft after stop")
                .await
                .unwrap();
        assert_eq!(
            launch
                .launch_messages
                .iter()
                .map(|message| message.as_concat_text())
                .collect::<Vec<_>>(),
            vec!["keep me queued".to_string(), "draft after stop".to_string()]
        );
        assert_eq!(
            launch
                .queue
                .items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["keep me queued"],
            "the draft rides along with the launch instead of being staged"
        );
        assert_eq!(
            launch.consume_queue_ids,
            launch
                .queue
                .items
                .iter()
                .map(|item| item.queue_id.clone())
                .collect::<Vec<_>>()
        );

        let empty_error = match prepare_desktop_launch(&state, &account_scope, "no-queue", "").await
        {
            Ok(_) => panic!("an empty composer and empty queue cannot start a run"),
            Err(error) => error,
        };
        assert!(empty_error.contains("empty"), "{empty_error}");

        // Isolated Goose managers can block on discard/teardown. The queue
        // contents above are the Maple-owned contract for this fixture.
        std::mem::forget(state.inner.lock().await.take());
        std::mem::forget(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn desktop_send_rejects_staging_onto_an_acp_run() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) =
            agent_service_test_context("desktop-steer-rejects-acp", sink.clone());
        let user_id = "desktop-steer-rejects-acp-user";
        let account_scope = account_scope(user_id).unwrap();
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(account_session_manager(&paths, user_id).unwrap());
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root.clone(),
                "ACP task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            Arc::clone(&permission_manager),
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));
        let agent_manager = Arc::new(
            AgentManager::new(
                GooseAgentConfig::new(
                    Arc::clone(&session_manager),
                    permission_manager,
                    None,
                    GooseMode::SmartApprove,
                    true,
                    GoosePlatform::GooseDesktop,
                ),
                Some(2),
            )
            .await
            .unwrap(),
        );
        let (run_events, _run_events_rx) = AgentRunEventPublisher::new(
            AgentEventDispatcher::new(sink),
            session.id.clone(),
            "acp-steer-run".to_string(),
            AgentHostEventPolicy::Suppress,
        );
        *state.inner.lock().await = Some(AgentRuntime {
            agent_manager,
            session_manager: Arc::clone(&session_manager),
            maple_api_session: crate::maple_api::test_maple_api_session(user_id),
            active_runs: HashMap::from([(
                "acp-steer-run".to_string(),
                ActiveAgentRun {
                    agent,
                    permission_routing: AgentPermissionRouting::CallingSurface,
                    token: CancellationToken::new(),
                    tool_context: SharedAgentToolContext::new(AgentToolContextSpec::default()),
                    session_id: session.id.clone(),
                    events: run_events,
                    cancelled_permission_ids: Arc::new(Mutex::new(HashSet::new())),
                    accepting_queue: Arc::new(AtomicBool::new(true)),
                    steered_unacked: Arc::new(Mutex::new(Vec::new())),
                    task_handle: tokio::spawn(async {}),
                },
            )]),
            session_title_tasks: HashMap::new(),
            session_tool_contexts: HashMap::new(),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            web_tool_state: Arc::new(WebToolState::default()),
            project_root,
            model: DEFAULT_AGENT_MODEL.to_string(),
            mode: DEFAULT_GOOSE_MODE.to_string(),
            account_scope,
            lifetime: CancellationToken::new(),
        });

        let handle = state.handle_for_user(user_id).await.unwrap();
        let error = match handle
            .send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: "should not join the ACP run".to_string(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: false,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
        {
            Ok(_) => panic!("desktop send should not join an ACP-owned run"),
            Err(error) => error,
        };
        assert!(
            error.contains("another Agent surface"),
            "unexpected error: {error}"
        );

        std::mem::forget(state.inner.lock().await.take());
        std::mem::forget(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn desktop_queue_revision_stays_monotonic_after_the_queue_empties() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, _paths, state) = agent_service_test_context("desktop-queue-revision", sink);
        let account_scope = account_scope("desktop-queue-revision-user").unwrap();
        let session_id = "session-revision";

        enqueue_desktop_queue_item(&state, &account_scope, session_id, "first")
            .await
            .unwrap();
        let (_, after_take) = take_all_desktop_queue_items_from_map(
            &state.desktop_queues,
            &account_scope,
            session_id,
        )
        .await
        .expect("the only staged item should be taken");
        assert!(after_take.items.is_empty());
        assert!(after_take.revision >= 2);

        let (_, restaged) =
            enqueue_desktop_queue_item(&state, &account_scope, session_id, "second")
                .await
                .unwrap();
        assert_eq!(restaged.items.len(), 1);
        assert!(
            restaged.revision > after_take.revision,
            "a later enqueue must not restart the revision at 1: {} then {}",
            after_take.revision,
            restaged.revision
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn desktop_queue_take_all_drains_every_item_in_order() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, _paths, state) = agent_service_test_context("desktop-queue-take-all", sink);
        let account_scope = account_scope("desktop-queue-take-all-user").unwrap();
        let session_id = "session-take-all";

        enqueue_desktop_queue_item(&state, &account_scope, session_id, "first")
            .await
            .unwrap();
        enqueue_desktop_queue_item(&state, &account_scope, session_id, "second")
            .await
            .unwrap();
        let (items, after_take) = take_all_desktop_queue_items_from_map(
            &state.desktop_queues,
            &account_scope,
            session_id,
        )
        .await
        .expect("both staged items should be taken together");
        assert_eq!(
            items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert!(after_take.items.is_empty());
        assert!(
            take_all_desktop_queue_items_from_map(
                &state.desktop_queues,
                &account_scope,
                session_id,
            )
            .await
            .is_none()
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn unpersisted_promoted_messages_return_to_the_queue_head() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, _paths, state) =
            agent_service_test_context("desktop-queue-restore-unpersisted", sink);
        let account_scope = account_scope("desktop-queue-restore-user").unwrap();
        let session_id = "session-restore-unpersisted";

        enqueue_desktop_queue_item(&state, &account_scope, session_id, "staged later")
            .await
            .unwrap();
        let promoted = vec![
            user_message_from_prompt("first promoted"),
            user_message_from_prompt("second promoted"),
        ];
        let snapshot = restore_unpersisted_desktop_queue_messages(
            &state.desktop_queues,
            &account_scope,
            session_id,
            &promoted,
        )
        .await;

        assert_eq!(
            snapshot
                .items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first promoted", "second promoted", "staged later"]
        );
        assert_eq!(
            snapshot.items[0].message_id.as_str(),
            promoted[0].id.as_deref().unwrap()
        );
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn desktop_queue_take_all_holds_while_an_item_is_being_edited() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, _paths, state) =
            agent_service_test_context("desktop-queue-hold-edit", sink);
        let account_scope = account_scope("desktop-queue-hold-edit-user").unwrap();
        let session_id = "session-hold-edit";

        enqueue_desktop_queue_item(&state, &account_scope, session_id, "first")
            .await
            .unwrap();
        enqueue_desktop_queue_item(&state, &account_scope, session_id, "second")
            .await
            .unwrap();
        let first_id = snapshot_desktop_queue(&state, &account_scope, session_id)
            .await
            .items[0]
            .queue_id
            .clone();
        begin_desktop_queue_edit(&state, &account_scope, session_id, &first_id)
            .await
            .unwrap();
        assert!(
            take_all_desktop_queue_items_from_map(
                &state.desktop_queues,
                &account_scope,
                session_id,
            )
            .await
            .is_none(),
            "an open edit must hold the leftover queue"
        );
        let second_id = snapshot_desktop_queue(&state, &account_scope, session_id)
            .await
            .items[1]
            .queue_id
            .clone();
        update_desktop_queue_item(&state, &account_scope, session_id, &second_id, "second v2")
            .await
            .unwrap();
        assert!(
            take_all_desktop_queue_items_from_map(
                &state.desktop_queues,
                &account_scope,
                session_id,
            )
            .await
            .is_none(),
            "updating another chip must not end the open edit"
        );
        end_desktop_queue_edit(&state, &account_scope, session_id, &first_id)
            .await
            .unwrap();
        let (items, _) = take_all_desktop_queue_items_from_map(
            &state.desktop_queues,
            &account_scope,
            session_id,
        )
        .await
        .expect("discarding the edit should release the leftover queue");
        assert_eq!(
            items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second v2"]
        );

        enqueue_desktop_queue_item(&state, &account_scope, session_id, "kept")
            .await
            .unwrap();
        enqueue_desktop_queue_item(&state, &account_scope, session_id, "also")
            .await
            .unwrap();
        let kept_id = snapshot_desktop_queue(&state, &account_scope, session_id)
            .await
            .items[0]
            .queue_id
            .clone();
        begin_desktop_queue_edit(&state, &account_scope, session_id, &kept_id)
            .await
            .unwrap();
        update_desktop_queue_item(&state, &account_scope, session_id, &kept_id, "kept revised")
            .await
            .unwrap();
        let (updated, _) = take_all_desktop_queue_items_from_map(
            &state.desktop_queues,
            &account_scope,
            session_id,
        )
        .await
        .expect("saving the edit should release the leftover queue");
        assert_eq!(
            updated
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["kept revised", "also"]
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn desktop_steer_of_one_idle_chip_leaves_the_rest_queued() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, _paths, state) =
            agent_service_test_context("desktop-steer-one-idle-chip", sink);
        let account_scope = account_scope("desktop-steer-one-idle-user").unwrap();
        let session_id = "session-steer-one-idle";
        enqueue_desktop_queue_item(&state, &account_scope, session_id, "first")
            .await
            .unwrap();
        enqueue_desktop_queue_item(&state, &account_scope, session_id, "second")
            .await
            .unwrap();
        let first_id = snapshot_desktop_queue(&state, &account_scope, session_id)
            .await
            .items[0]
            .queue_id
            .clone();
        let plan = take_desktop_steer_plan(
            &state,
            &account_scope,
            &AgentSendMessageRequest {
                session_id: session_id.to_string(),
                text: String::new(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: true,
                queue_id: Some(first_id.clone()),
                attachments: Vec::new(),
            },
            "",
            None,
        )
        .await
        .unwrap();
        match plan {
            DesktopSendPlan::Start {
                launch_messages,
                queue,
                consume_queue_ids,
            } => {
                assert_eq!(
                    launch_messages
                        .iter()
                        .map(|message| message.as_concat_text())
                        .collect::<Vec<_>>(),
                    vec!["first".to_string()]
                );
                assert_eq!(consume_queue_ids, vec![first_id]);
                assert_eq!(
                    queue
                        .items
                        .iter()
                        .map(|item| item.text.as_str())
                        .collect::<Vec<_>>(),
                    vec!["first", "second"]
                );
            }
            _ => panic!("idle chip steer should start a single-message run"),
        }

        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn persist_unacked_steers_skips_messages_already_in_history() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, _state) =
            agent_service_test_context("persist-unacked-steers-skip", sink);
        let user_id = "persist-unacked-steers-skip-user";
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = account_session_manager(&paths, user_id).unwrap();
        let session = session_manager
            .create_session(
                project_root,
                "Steer persist skip".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let already = Message::user()
            .with_text("already persisted")
            .with_id("steer-already");
        let fresh = Message::user()
            .with_text("fresh steer")
            .with_id("steer-fresh");
        session_manager
            .add_message(&session.id, &already)
            .await
            .unwrap();
        persist_unacked_steers(
            session_manager.as_ref(),
            &session.id,
            &Mutex::new(vec![already.clone(), fresh.clone()]),
        )
        .await;
        let loaded = session_manager
            .get_session(&session.id, true)
            .await
            .unwrap();
        let ids = loaded
            .conversation
            .expect("session conversation")
            .messages()
            .iter()
            .filter_map(|message| message.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec!["steer-already".to_string(), "steer-fresh".to_string()]
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn desktop_empty_steer_sends_the_whole_leftover_stack() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) =
            agent_service_test_context("desktop-steer-empty-stack", sink.clone());
        let user_id = "desktop-steer-empty-stack-user";
        let account_scope = account_scope(user_id).unwrap();
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(account_session_manager(&paths, user_id).unwrap());
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root.clone(),
                "Steer stack task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            Arc::clone(&permission_manager),
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));
        let agent_manager = Arc::new(
            AgentManager::new(
                GooseAgentConfig::new(
                    Arc::clone(&session_manager),
                    permission_manager,
                    None,
                    GooseMode::SmartApprove,
                    true,
                    GoosePlatform::GooseDesktop,
                ),
                Some(2),
            )
            .await
            .unwrap(),
        );
        let (run_events, _run_events_rx) = AgentRunEventPublisher::new(
            AgentEventDispatcher::new(sink.clone()),
            session.id.clone(),
            "desktop-steer-empty-stack-run".to_string(),
            AgentHostEventPolicy::Suppress,
        );
        let steered_unacked = Arc::new(Mutex::new(Vec::new()));
        *state.inner.lock().await = Some(AgentRuntime {
            agent_manager,
            session_manager: Arc::clone(&session_manager),
            maple_api_session: crate::maple_api::test_maple_api_session(user_id),
            active_runs: HashMap::from([(
                "desktop-steer-empty-stack-run".to_string(),
                ActiveAgentRun {
                    agent,
                    permission_routing: AgentPermissionRouting::Desktop,
                    token: CancellationToken::new(),
                    tool_context: SharedAgentToolContext::new(AgentToolContextSpec::default()),
                    session_id: session.id.clone(),
                    events: run_events,
                    cancelled_permission_ids: Arc::new(Mutex::new(HashSet::new())),
                    accepting_queue: Arc::new(AtomicBool::new(true)),
                    steered_unacked: Arc::clone(&steered_unacked),
                    task_handle: tokio::spawn(async {}),
                },
            )]),
            session_title_tasks: HashMap::new(),
            session_tool_contexts: HashMap::new(),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            web_tool_state: Arc::new(WebToolState::default()),
            project_root,
            model: DEFAULT_AGENT_MODEL.to_string(),
            mode: DEFAULT_GOOSE_MODE.to_string(),
            account_scope: account_scope.clone(),
            lifetime: CancellationToken::new(),
        });

        let handle = state.handle_for_user(user_id).await.unwrap();
        let empty_error = match handle
            .send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: String::new(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: true,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
        {
            Ok(_) => panic!("empty steer without chips should fail"),
            Err(error) => error,
        };
        assert_eq!(empty_error, "Prompt cannot be empty");

        handle
            .send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: "first leftover".to_string(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: false,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        handle
            .send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: "second leftover".to_string(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: false,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        let first_id = snapshot_desktop_queue(&state, &account_scope, &session.id)
            .await
            .items[0]
            .queue_id
            .clone();
        begin_desktop_queue_edit(&state, &account_scope, &session.id, &first_id)
            .await
            .unwrap();
        let held_error = match handle
            .send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: String::new(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: true,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
        {
            Ok(_) => panic!("empty steer should hold while a chip is being edited"),
            Err(error) => error,
        };
        assert_eq!(
            held_error,
            "Queued messages cannot be steered while one is being edited"
        );
        end_desktop_queue_edit(&state, &account_scope, &session.id, &first_id)
            .await
            .unwrap();

        let steered = handle
            .send_message(AgentSendMessageRequest {
                session_id: session.id.clone(),
                text: String::new(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: true,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(steered.run_id, "desktop-steer-empty-stack-run");
        assert!(steered.queued.is_none());
        assert!(steered.queue.items.is_empty());
        assert_eq!(
            steered_unacked
                .lock()
                .await
                .iter()
                .map(|message| message.as_concat_text())
                .collect::<Vec<_>>(),
            vec!["first leftover".to_string(), "second leftover".to_string()]
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn desktop_queue_update_keeps_original_position() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, _paths, state) =
            agent_service_test_context("desktop-queue-update-in-place", sink);
        let account_scope = account_scope("desktop-queue-update-user").unwrap();
        let session_id = "session-update-in-place";

        enqueue_desktop_queue_item(&state, &account_scope, session_id, "first")
            .await
            .unwrap();
        enqueue_desktop_queue_item(&state, &account_scope, session_id, "second")
            .await
            .unwrap();
        enqueue_desktop_queue_item(&state, &account_scope, session_id, "third")
            .await
            .unwrap();
        let snapshot = snapshot_desktop_queue(&state, &account_scope, session_id).await;
        let first_id = snapshot.items[0].queue_id.clone();
        let (updated, after_update) = update_desktop_queue_item(
            &state,
            &account_scope,
            session_id,
            &first_id,
            "  first revised  ",
        )
        .await
        .unwrap();
        assert_eq!(updated.queue_id, first_id);
        assert_eq!(
            after_update
                .items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first revised", "second", "third"]
        );
        assert!(after_update.revision > snapshot.revision);
        let empty = update_desktop_queue_item(&state, &account_scope, session_id, &first_id, "   ")
            .await
            .unwrap_err();
        assert!(empty.contains("empty"), "{empty}");

        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn desktop_queue_rejects_oversized_and_excess_items() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, _paths, state) = agent_service_test_context("desktop-queue-bounds", sink);
        let account_scope = account_scope("desktop-queue-bounds-user").unwrap();
        let session_id = "session-bounds";

        let oversized = "x".repeat(MAX_DESKTOP_QUEUE_TEXT_BYTES + 1);
        let error = enqueue_desktop_queue_item(&state, &account_scope, session_id, &oversized)
            .await
            .expect_err("oversized staged text must be rejected");
        assert!(error.contains("too large"), "{error}");

        for index in 0..MAX_DESKTOP_QUEUE_ITEMS {
            enqueue_desktop_queue_item(
                &state,
                &account_scope,
                session_id,
                &format!("queued {index}"),
            )
            .await
            .unwrap();
        }
        let error = enqueue_desktop_queue_item(&state, &account_scope, session_id, "one more")
            .await
            .expect_err("the 17th staged message must be rejected");
        assert!(error.contains("too many"), "{error}");

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn run_cancellation_scope_rejects_cross_surface_and_wrong_session_access() {
        assert!(
            validate_run_cancellation_scope(
                "session-1",
                AgentPermissionRouting::CallingSurface,
                None,
                AgentPermissionRouting::Desktop,
            )
            .is_err()
        );
        assert!(
            validate_run_cancellation_scope(
                "session-1",
                AgentPermissionRouting::CallingSurface,
                Some("session-2"),
                AgentPermissionRouting::CallingSurface,
            )
            .is_err()
        );
        assert!(
            validate_run_cancellation_scope(
                "session-1",
                AgentPermissionRouting::CallingSurface,
                Some("session-1"),
                AgentPermissionRouting::CallingSurface,
            )
            .is_ok()
        );
    }

    #[test]
    fn fresh_agent_config_defaults_to_glm() {
        assert_eq!(AgentConfig::default().default_model, DEFAULT_AGENT_MODEL);
        assert!(AgentConfig::default().mcp_servers.is_empty());

        let config: AgentConfig = serde_json::from_value(json!({
            "defaultProjectRoot": null,
            "runtimeKind": "goose-direct"
        }))
        .expect("legacy config without a model should deserialize");
        assert_eq!(config.default_model, DEFAULT_AGENT_MODEL);
        assert!(config.mcp_servers.is_empty());
        assert!(config.project_trust.is_empty());
    }

    #[test]
    fn legacy_project_skills_trust_migrates_to_generic_project_trust() {
        let test_root = recent_roots_test_dir("legacy-project-trust");
        let config_path = test_root.join("config.json");
        let removed_roots_path = test_root.join("removed-project-roots.json");
        write_json_file(
            &config_path,
            &json!({
                "defaultProjectRoot": null,
                "defaultModel": DEFAULT_AGENT_MODEL,
                "mcpServers": [],
                "projectSkillsTrust": [
                    { "path": "/tmp/maple-project", "trusted": true }
                ]
            }),
        )
        .unwrap();
        let config = load_agent_config_files(&config_path, &removed_roots_path)
            .expect("legacy project skills trust should deserialize");

        assert_eq!(
            config.project_trust,
            vec![AgentProjectTrust {
                path: "/tmp/maple-project".to_string(),
                trusted: true,
            }]
        );
        let encoded = serde_json::from_slice::<Value>(&fs::read(&config_path).unwrap()).unwrap();
        assert_eq!(
            encoded["projectTrust"],
            json!([{ "path": "/tmp/maple-project", "trusted": true }])
        );
        assert!(encoded.get("projectSkillsTrust").is_none());
        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn removed_project_roots_round_trip_only_through_device_local_storage() {
        let test_root = recent_roots_test_dir("device-local-removed-roots");
        let config_path = test_root.join("roaming/config.json");
        let removed_roots_path = test_root.join("local/removed_project_roots.json");
        let removed = test_project_path("device-local-removed");
        let mut config = AgentConfig {
            default_project_root: Some(removed.clone()),
            ..AgentConfig::default()
        };
        config.removed_project_roots = vec![removed.clone()];

        save_agent_config_file(&config_path, &config).unwrap();
        write_device_local_json_file(&removed_roots_path, &config.removed_project_roots).unwrap();

        let roaming = serde_json::from_slice::<Value>(&fs::read(&config_path).unwrap()).unwrap();
        assert!(roaming.get("removedProjectRoots").is_none());
        let loaded = load_agent_config_files(&config_path, &removed_roots_path).unwrap();
        assert_eq!(loaded.removed_project_roots, vec![removed.clone()]);
        assert_eq!(
            serde_json::to_value(&loaded).unwrap()["removedProjectRoots"],
            json!([removed])
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn device_local_removed_project_roots_do_not_cross_devices() {
        let test_root = recent_roots_test_dir("cross-device-removed-roots");
        let config_path = test_root.join("roaming/config.json");
        let device_a_path = test_root.join("device-a/removed_project_roots.json");
        let device_b_path = test_root.join("device-b/removed_project_roots.json");
        let removed = test_project_path("cross-device-removed");
        save_agent_config_file(&config_path, &AgentConfig::default()).unwrap();
        write_device_local_json_file(&device_a_path, std::slice::from_ref(&removed)).unwrap();

        let device_a = load_agent_config_files(&config_path, &device_a_path).unwrap();
        let device_b = load_agent_config_files(&config_path, &device_b_path).unwrap();

        assert_eq!(device_a.removed_project_roots, vec![removed]);
        assert!(device_b.removed_project_roots.is_empty());

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn roaming_removed_project_roots_are_ignored_and_scrubbed() {
        let test_root = recent_roots_test_dir("ignore-roaming-removed-roots");
        let config_path = test_root.join("roaming/config.json");
        let removed_roots_path = test_root.join("local/removed_project_roots.json");
        let removed = test_project_path("roaming-removed");
        let config = AgentConfig {
            removed_project_roots: vec![removed],
            ..AgentConfig::default()
        };
        write_json_file(&config_path, &config).unwrap();

        let loaded = load_agent_config_files(&config_path, &removed_roots_path).unwrap();

        assert!(loaded.removed_project_roots.is_empty());
        let roaming = serde_json::from_slice::<Value>(&fs::read(&config_path).unwrap()).unwrap();
        assert!(roaming.get("removedProjectRoots").is_none());

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn device_local_removed_project_roots_are_sanitized_on_load() {
        let test_root = recent_roots_test_dir("sanitize-local-removed-roots");
        let path = test_root.join("removed_project_roots.json");
        let removed = test_project_path("sanitize-local-removed");
        write_json_file(
            &path,
            &vec![
                removed.clone(),
                "relative/project".to_string(),
                removed.clone(),
            ],
        )
        .unwrap();

        let loaded = load_removed_project_roots_file(&path).unwrap();

        assert_eq!(loaded, vec![removed.clone()]);
        let persisted = serde_json::from_slice::<Vec<String>>(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted, vec![removed]);

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn device_local_serialization_failure_preserves_existing_state() {
        struct RejectSerialization;
        impl Serialize for RejectSerialization {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("injected serialization failure"))
            }
        }

        let test_root = recent_roots_test_dir("local-write-rollback");
        let path = test_root.join("removed_project_roots.json");
        let removed = vec![test_project_path("local-write-rollback")];
        write_device_local_json_file(&path, &removed).unwrap();
        let before = fs::read(&path).unwrap();

        assert!(write_device_local_json_file(&path, &RejectSerialization).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn embedded_goose_disables_hidden_tool_pair_summarization() {
        let test_root = recent_roots_test_dir("embedded-goose-config");
        let config = goose::config::Config::new_with_file_secrets(
            test_root.join("config.yaml"),
            test_root.join("secrets.yaml"),
        )
        .unwrap();
        config
            .set_param("GOOSE_TOOL_PAIR_SUMMARIZATION", true)
            .unwrap();

        configure_embedded_goose_params(&config, DEFAULT_AGENT_MODEL, DEFAULT_GOOSE_MODE).unwrap();

        assert!(
            !config
                .get_param::<bool>("GOOSE_TOOL_PAIR_SUMMARIZATION")
                .unwrap()
        );
        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn embedded_goose_search_paths_update_is_desktop_scoped_and_clears_stale_values() {
        let test_root = recent_roots_test_dir("embedded-goose-search-paths");
        let config_path = test_root.join("config.yaml");
        let config = goose::config::Config::new_with_file_secrets(
            &config_path,
            test_root.join("secrets.yaml"),
        )
        .unwrap();
        let sentinel = vec!["/preserved/on/non-macos".to_string()];
        config.set_goose_search_paths(sentinel).unwrap();

        configure_embedded_goose_search_paths(&config, None).unwrap();
        let persisted = fs::read_to_string(&config_path).unwrap();
        assert!(persisted.contains("- /preserved/on/non-macos"));

        let recovered = vec!["/login/first".to_string(), "/login/second".to_string()];
        configure_embedded_goose_search_paths(&config, Some(&recovered)).unwrap();
        let persisted = fs::read_to_string(&config_path).unwrap();
        assert!(persisted.contains("- /login/first"));
        assert!(persisted.contains("- /login/second"));
        assert!(!persisted.contains("/preserved/on/non-macos"));

        configure_embedded_goose_search_paths(&config, Some(&[])).unwrap();
        let persisted = fs::read_to_string(&config_path).unwrap();
        assert!(persisted.contains("GOOSE_SEARCH_PATHS: []"));
        assert!(!persisted.contains("/login/first"));
        assert!(!persisted.contains("/login/second"));
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn cold_session_agent_preserves_its_persisted_maple_model() {
        let test_root = recent_roots_test_dir("cold-session-model");
        let session_manager = Arc::new(SessionManager::new(test_root.join("sessions")));
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let agent_manager = Arc::new(
            AgentManager::new(
                GooseAgentConfig::new(
                    Arc::clone(&session_manager),
                    permission_manager,
                    None,
                    GooseMode::SmartApprove,
                    true,
                    GoosePlatform::GooseDesktop,
                ),
                Some(2),
            )
            .await
            .unwrap(),
        );
        let transport = Arc::new(InertMapleTransport);
        agent_manager
            .set_default_provider(Arc::new(MapleProvider::new(Arc::clone(&transport))))
            .await;

        let session = session_manager
            .create_session(
                test_root.clone(),
                "Cold task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let persisted_model_config = goose_providers::model::ModelConfig::new("gemma-3-27b")
            .with_context_limit(Some(64_321))
            .with_temperature(Some(0.42));
        session_manager
            .update(&session.id)
            .provider_name(MAPLE_PROVIDER_NAME)
            .model_config(persisted_model_config)
            .apply()
            .await
            .unwrap();
        let session = session_manager
            .get_session(&session.id, false)
            .await
            .unwrap();

        let manager_result = get_or_create_session_agent(
            &agent_manager,
            &transport,
            &session,
            "You are a test agent.",
            RuntimeContext::default(),
        )
        .await
        .unwrap();

        assert!(manager_result.agent_created);
        assert_eq!(
            manager_result.agent.provider().await.unwrap().get_name(),
            MAPLE_PROVIDER_NAME
        );
        let persisted = session_manager
            .get_session(&session.id, false)
            .await
            .unwrap();
        assert_eq!(
            persisted
                .model_config
                .as_ref()
                .map(|model| model.model_name.as_str()),
            Some("gemma-3-27b")
        );
        assert_eq!(
            persisted
                .model_config
                .as_ref()
                .and_then(|model| model.context_limit),
            Some(64_321)
        );
        assert_eq!(
            persisted
                .model_config
                .as_ref()
                .and_then(|model| model.temperature),
            Some(0.42)
        );

        drop(manager_result);
        drop(agent_manager);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn maple_context_limits_are_isolated_and_persisted_per_session() {
        let test_root = recent_roots_test_dir("per-session-context-limits");
        let session_manager = Arc::new(SessionManager::new(test_root.join("sessions")));
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let agent_manager = Arc::new(
            AgentManager::new(
                GooseAgentConfig::new(
                    Arc::clone(&session_manager),
                    permission_manager,
                    None,
                    GooseMode::SmartApprove,
                    true,
                    GoosePlatform::GooseDesktop,
                ),
                Some(2),
            )
            .await
            .unwrap(),
        );
        let transport = Arc::new(InertMapleTransport);
        agent_manager
            .set_default_provider(Arc::new(MapleProvider::new(Arc::clone(&transport))))
            .await;

        let glm_session = session_manager
            .create_session(
                test_root.clone(),
                "GLM task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let kimi_session = session_manager
            .create_session(
                test_root.clone(),
                "Kimi task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();

        let glm_agent = get_or_create_session_agent(
            &agent_manager,
            &transport,
            &glm_session,
            "You are a test agent.",
            RuntimeContext::default(),
        )
        .await
        .unwrap();
        install_maple_provider(
            &glm_agent.agent,
            &transport,
            &glm_session,
            "glm-5-2",
            Some(384_000),
        )
        .await
        .unwrap();
        drop(glm_agent);

        let kimi_agent = get_or_create_session_agent(
            &agent_manager,
            &transport,
            &kimi_session,
            "You are a test agent.",
            RuntimeContext::default(),
        )
        .await
        .unwrap();
        install_maple_provider(
            &kimi_agent.agent,
            &transport,
            &kimi_session,
            "auto:powerful",
            Some(256_000),
        )
        .await
        .unwrap();
        drop(kimi_agent);

        let persisted_glm = session_manager
            .get_session(&glm_session.id, false)
            .await
            .unwrap();
        let persisted_kimi = session_manager
            .get_session(&kimi_session.id, false)
            .await
            .unwrap();
        assert_eq!(
            persisted_glm
                .model_config
                .as_ref()
                .map(|config| (config.model_name.as_str(), config.context_limit)),
            Some(("glm-5-2", Some(384_000)))
        );
        assert_eq!(
            persisted_kimi
                .model_config
                .as_ref()
                .map(|config| (config.model_name.as_str(), config.context_limit)),
            Some(("auto:powerful", Some(256_000)))
        );

        let glm_agent = get_or_create_session_agent(
            &agent_manager,
            &transport,
            &persisted_glm,
            "You are a test agent.",
            RuntimeContext::default(),
        )
        .await
        .unwrap();
        install_maple_provider(
            &glm_agent.agent,
            &transport,
            &persisted_glm,
            "glm-5-2",
            None,
        )
        .await
        .unwrap();
        drop(glm_agent);
        let persisted_glm = session_manager
            .get_session(&glm_session.id, false)
            .await
            .unwrap();
        assert_eq!(
            persisted_glm
                .model_config
                .as_ref()
                .and_then(|config| config.context_limit),
            Some(384_000)
        );

        drop(agent_manager);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn project_trust_is_reversible_and_persists_both_decisions() {
        let test_root = recent_roots_test_dir("skills-trust");
        let project = test_root.join("project");
        let config_path = test_root.join("config.json");
        fs::create_dir_all(&project).unwrap();
        let project = normalize_project_root(&project).unwrap();
        let mut config = AgentConfig::default();

        assert_eq!(project_trust_status(&config, &project, true).decision, None);
        apply_project_trust(&mut config, &project, false);
        apply_project_trust(&mut config, &project, false);
        apply_project_trust(&mut config, &project, true);
        write_json_file(&config_path, &config).unwrap();

        let loaded = load_agent_config_file(&config_path).unwrap();
        let status = project_trust_status(&loaded, &project, true);
        assert_eq!(status.path, path_string(&project));
        assert_eq!(status.decision, Some(true));
        assert!(status.available);
        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn project_trust_detects_skills_only_from_real_skill_files() {
        let test_root = recent_roots_test_dir("project-trust-features");
        let project = test_root.join("project");
        let skill = project.join(".agents/skills/review-maple");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("README.md"), "not a skill").unwrap();

        assert!(project_trust_features(&project).is_empty());
        fs::write(skill.join("SKILL.md"), "# Review Maple").unwrap();
        assert_eq!(
            project_trust_features(&project),
            vec![AgentProjectTrustFeature::Skills]
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[cfg(unix)]
    #[test]
    fn project_trust_feature_detection_does_not_follow_skill_symlinks() {
        use std::os::unix::fs::symlink;

        let test_root = recent_roots_test_dir("project-trust-symlink-features");
        let project = test_root.join("project");
        let external = test_root.join("external");
        fs::create_dir_all(project.join(".agents/skills")).unwrap();
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("SKILL.md"), "# External skill").unwrap();
        symlink(&external, project.join(".agents/skills/external")).unwrap();

        assert!(project_trust_features(&project).is_empty());

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn project_skills_root_must_still_be_available() {
        let test_root = recent_roots_test_dir("skills-root-available");
        let project = test_root.join("project");
        fs::create_dir_all(&project).unwrap();
        let canonical_project = normalize_project_root(&project).unwrap();

        assert!(project_skills_root_is_available(&canonical_project));
        fs::remove_dir_all(&project).unwrap();
        assert!(!project_skills_root_is_available(&canonical_project));

        let _ = fs::remove_dir_all(test_root);
    }

    #[cfg(unix)]
    #[test]
    fn project_skills_root_rejects_symlink_replacement() {
        use std::os::unix::fs::symlink;

        let test_root = recent_roots_test_dir("skills-root-replaced");
        let project = test_root.join("project");
        let replacement = test_root.join("replacement");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&replacement).unwrap();
        let canonical_project = normalize_project_root(&project).unwrap();

        fs::remove_dir_all(&project).unwrap();
        symlink(&replacement, &project).unwrap();
        assert!(!project_skills_root_is_available(&canonical_project));

        let _ = fs::remove_dir_all(test_root);
    }

    #[cfg(unix)]
    #[test]
    fn project_trust_uses_the_canonical_folder_path() {
        use std::os::unix::fs::symlink;

        let test_root = recent_roots_test_dir("skills-trust-symlink");
        let project = test_root.join("project");
        let alias = test_root.join("alias");
        fs::create_dir_all(&project).unwrap();
        symlink(&project, &alias).unwrap();

        let canonical_project = normalize_project_root(&project).unwrap();
        let canonical_alias = normalize_project_root(&alias).unwrap();
        assert_eq!(canonical_project, canonical_alias);
        let mut config = AgentConfig::default();
        apply_project_trust(&mut config, &canonical_project, true);
        assert_eq!(
            project_trust_status(&config, &canonical_alias, true).decision,
            Some(true)
        );
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn untrusted_skills_client_keeps_project_instructions_out_of_context() {
        use goose::agents::ToolCallContext;
        use goose::agents::extension::PlatformExtensionContext;
        use goose::agents::mcp_client::McpClientTrait;

        let test_root = recent_roots_test_dir("skills-discovery");
        let project = test_root.join("project");
        let inert = test_root.join("inert");
        let skill_name = format!(
            "maple-project-skill-{}-{}",
            std::process::id(),
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        );
        let description = format!("unique description for {skill_name}");
        let body = format!("unique body for {skill_name}");
        let skill_dir = project.join(".agents/skills").join(&skill_name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::create_dir_all(&inert).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {skill_name}\ndescription: {description}\n---\n{body}"),
        )
        .unwrap();
        let session_manager = Arc::new(SessionManager::new(test_root.join("sessions")));
        let make_client = |working_dir: PathBuf| {
            SkillsClient::new(PlatformExtensionContext {
                extension_manager: None,
                session_manager: Arc::clone(&session_manager),
                scheduler: None,
                session: Some(Arc::new(Session {
                    working_dir,
                    ..Session::default()
                })),
                use_login_shell_path: false,
                tool_confirmation_router: None,
            })
            .unwrap()
        };

        let trusted = make_client(project);
        let trusted_instructions = trusted.get_instructions().unwrap();
        assert!(trusted_instructions.contains(&skill_name));
        assert!(trusted_instructions.contains(&description));
        assert!(!trusted_instructions.contains(&body));

        let untrusted = TrustAwareSkillsClient {
            inner: make_client(inert),
            blocked_project_root: Some(test_root.join("project")),
        };
        let untrusted_instructions = untrusted.get_instructions().unwrap_or_default();
        assert!(!untrusted_instructions.contains(&skill_name));
        assert!(!untrusted_instructions.contains(&description));
        let arguments = serde_json::from_value(json!({"name": skill_name})).unwrap();
        let result = untrusted
            .call_tool(
                &ToolCallContext::new("test".to_string(), None, None),
                "load_skill",
                Some(arguments),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        let message = result.content[0]
            .as_text()
            .expect("missing skill response should be text")
            .text
            .as_str();
        assert!(message.contains("No trusted skill named"), "{message}");
        assert!(message.contains("Project trust is disabled"), "{message}");
        assert!(
            message.contains("Do not read project skill files directly"),
            "{message}"
        );
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn maple_skills_registration_is_unprefixed_transient_and_coexists_with_skills_mcp() {
        use goose::agents::ToolCallContext;
        use goose::agents::mcp_client::McpClientTrait;

        let test_root = recent_roots_test_dir("skills-registration");
        let project = test_root.join("project");
        let external_root = test_root.join("external");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&external_root).unwrap();
        let project_skill_name = format!(
            "maple-registration-skill-{}-{}",
            std::process::id(),
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        );
        let project_skill_dir = project.join(".agents/skills").join(&project_skill_name);
        fs::create_dir_all(&project_skill_dir).unwrap();
        fs::write(
            project_skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {project_skill_name}\ndescription: Maple registration test skill\n---\nUse the Maple registration test instructions."
            ),
        )
        .unwrap();

        let session_manager = Arc::new(SessionManager::new(test_root.join("sessions")));
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project.clone(),
                "Skills registration".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));

        // Simulate a user-configured MCP server named `skills` without making
        // a network connection. Its transport config should still prefix its
        // tool independently from Maple's first-class platform client.
        let mcp_config = mcp_server_to_extension(&AgentMcpServer {
            name: "skills".to_string(),
            description: "User MCP named skills".to_string(),
            enabled: true,
            timeout_seconds: 30,
            transport: AgentMcpTransport::StreamableHttp {
                url: "https://example.invalid/mcp".to_string(),
                environment: Vec::new(),
                headers: Vec::new(),
            },
        })
        .unwrap();
        let mcp_client = skills_client_for_working_dir(&agent, &session, external_root).unwrap();
        agent
            .extension_manager
            .add_client(
                "skills".to_string(),
                mcp_config.clone(),
                Arc::new(mcp_client),
                None,
                None,
            )
            .await;
        let initial_skills = TrustAwareSkillsClient {
            inner: skills_client_for_working_dir(&agent, &session, project.clone()).unwrap(),
            blocked_project_root: None,
        };
        let skills_instructions = initial_skills.get_instructions().unwrap_or_default();
        assert!(skills_instructions.contains(&project_skill_name));
        assert!(!skills_instructions.contains("goose-doc-guide"));

        let builtin_result = initial_skills
            .call_tool(
                &ToolCallContext::new("test".to_string(), None, None),
                "load_skill",
                Some(serde_json::from_value(json!({"name": "goose-doc-guide"})).unwrap()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(builtin_result.is_error, Some(true));

        let project_result = initial_skills
            .call_tool(
                &ToolCallContext::new("test".to_string(), None, None),
                "load_skill",
                Some(serde_json::from_value(json!({"name": project_skill_name})).unwrap()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_ne!(project_result.is_error, Some(true));
        attach_prepared_skills_client(&agent, initial_skills).await;

        let prompt_extensions = agent.extension_manager.get_extensions_info(&project).await;
        assert!(
            prompt_extensions
                .iter()
                .any(|extension| extension.name == MAPLE_SKILLS_CLIENT_KEY)
        );
        assert!(
            !prompt_extensions
                .iter()
                .any(|extension| extension.name.contains("runtime_only"))
        );

        let tools = agent.list_tools(&session.id, None).await;
        let maple_tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "load_skill")
            .expect("Maple skills tool should be unprefixed");
        assert_eq!(
            goose::agents::extension_manager::get_tool_owner(maple_tool).as_deref(),
            Some(MAPLE_SKILLS_CLIENT_KEY)
        );
        let mcp_tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "skills__load_skill")
            .expect("user MCP tool should remain prefixed");
        assert_eq!(
            goose::agents::extension_manager::get_tool_owner(mcp_tool).as_deref(),
            Some("skills")
        );
        assert_eq!(
            tools
                .iter()
                .filter(|tool| tool.name.as_ref() == "load_skill")
                .count(),
            1
        );

        let prepared_skills = TrustAwareSkillsClient {
            inner: skills_client_for_working_dir(&agent, &session, project).unwrap(),
            blocked_project_root: None,
        };
        detach_transient_skills_client(&agent).await;
        agent.persist_extension_state(&session.id).await.unwrap();
        let persisted = session_manager
            .get_session(&session.id, false)
            .await
            .unwrap();
        let persisted_extensions =
            goose::session::EnabledExtensionsState::from_extension_data(&persisted.extension_data)
                .expect("extension state should be persisted");
        assert_eq!(persisted_extensions.extensions, vec![mcp_config]);

        let tools_after_detach = agent.list_tools(&session.id, None).await;
        assert!(
            !tools_after_detach
                .iter()
                .any(|tool| tool.name.as_ref() == "load_skill")
        );
        assert!(
            tools_after_detach
                .iter()
                .any(|tool| tool.name.as_ref() == "skills__load_skill")
        );

        attach_prepared_skills_client(&agent, prepared_skills).await;
        let tools_after_restore = agent.list_tools(&session.id, None).await;
        assert!(
            tools_after_restore
                .iter()
                .any(|tool| tool.name.as_ref() == "load_skill")
        );
        assert!(
            tools_after_restore
                .iter()
                .any(|tool| tool.name.as_ref() == "skills__load_skill")
        );

        let _ = fs::remove_dir_all(test_root);
    }

    fn stdio_mcp(name: &str, enabled: bool) -> AgentMcpServer {
        AgentMcpServer {
            name: name.to_string(),
            description: "Test server".to_string(),
            enabled,
            timeout_seconds: 30,
            transport: AgentMcpTransport::Stdio {
                command: "tool --flag 'two words'".to_string(),
                environment: vec![AgentMcpKeyValue {
                    key: "MCP_TOKEN".to_string(),
                    value: "super-secret-value".to_string(),
                }],
            },
        }
    }

    #[test]
    fn mcp_stdio_command_and_environment_are_frozen_in_the_session_snapshot() {
        let servers = normalize_mcp_servers(vec![stdio_mcp("My Server", true)]).unwrap();
        let config = mcp_server_to_extension(&servers[0]).unwrap();
        let ExtensionConfig::Stdio {
            cmd,
            args,
            envs,
            env_keys,
            ..
        } = &config
        else {
            panic!("expected stdio extension");
        };
        assert_eq!(cmd, "tool");
        assert_eq!(args, &["--flag", "two words"]);
        assert_eq!(
            envs.get_env().get("MCP_TOKEN").map(String::as_str),
            Some("super-secret-value")
        );
        assert!(env_keys.is_empty());

        let persisted = serde_json::to_string(&config).unwrap();
        assert!(persisted.contains("super-secret-value"));
    }

    #[test]
    fn mcp_stdio_command_preserves_windows_paths_and_apostrophes() {
        let cases = [
            (
                r"C:\tools\mcp.exe --arg value",
                r"C:\tools\mcp.exe",
                vec!["--arg", "value"],
            ),
            (
                r#""C:\Program Files\server\mcp.exe" --arg"#,
                r"C:\Program Files\server\mcp.exe",
                vec!["--arg"],
            ),
            (
                "O'Reilly wrote don't split",
                "O'Reilly",
                vec!["wrote", "don't", "split"],
            ),
        ];

        for (command, expected_cmd, expected_args) in cases {
            let mut server = stdio_mcp("portable", true);
            let AgentMcpTransport::Stdio {
                command: server_command,
                ..
            } = &mut server.transport
            else {
                unreachable!();
            };
            *server_command = command.to_string();
            let server = normalize_mcp_servers(vec![server]).unwrap().remove(0);
            let ExtensionConfig::Stdio { cmd, args, .. } =
                mcp_server_to_extension(&server).unwrap()
            else {
                panic!("expected stdio extension");
            };
            assert_eq!(cmd, expected_cmd);
            assert_eq!(args, expected_args);
        }
    }

    #[test]
    fn mcp_server_names_use_goose_normalization_and_reserve_only_public_maple_names() {
        let duplicate = normalize_mcp_servers(vec![
            stdio_mcp("My Server", true),
            stdio_mcp("myserver", false),
        ])
        .unwrap_err();
        assert!(duplicate.contains("conflicts"));

        let reserved = normalize_mcp_servers(vec![stdio_mcp("Developer", true)]).unwrap_err();
        assert!(reserved.contains("reserved"));

        let reserved_skills =
            normalize_mcp_servers(vec![stdio_mcp(MAPLE_SKILLS_CLIENT_KEY, true)]).unwrap_err();
        assert!(reserved_skills.contains("reserved"));

        // This was a valid user-defined MCP name before Skills support and
        // must remain recoverable after upgrade.
        assert!(normalize_mcp_servers(vec![stdio_mcp("maple_internal_skills", true)]).is_ok());
        assert!(MAPLE_SKILLS_CLIENT_KEY.chars().count() <= MAX_MCP_SERVER_NAME_CHARS);
    }

    #[test]
    fn mcp_validation_rejects_unsafe_env_and_duplicate_headers() {
        let mut unsafe_server = stdio_mcp("unsafe", true);
        let AgentMcpTransport::Stdio { environment, .. } = &mut unsafe_server.transport else {
            unreachable!();
        };
        environment[0].key = "NODE_OPTIONS".to_string();
        assert!(
            normalize_mcp_servers(vec![unsafe_server])
                .unwrap_err()
                .contains("cannot override")
        );

        let duplicate_headers = AgentMcpServer {
            name: "http".to_string(),
            description: String::new(),
            enabled: true,
            timeout_seconds: 30,
            transport: AgentMcpTransport::StreamableHttp {
                url: "http://127.0.0.1:3000/mcp".to_string(),
                environment: Vec::new(),
                headers: vec![
                    AgentMcpKeyValue {
                        key: "Authorization".to_string(),
                        value: "first".to_string(),
                    },
                    AgentMcpKeyValue {
                        key: "authorization".to_string(),
                        value: "second".to_string(),
                    },
                ],
            },
        };
        assert!(
            normalize_mcp_servers(vec![duplicate_headers])
                .unwrap_err()
                .contains("duplicate HTTP header")
        );
    }

    #[test]
    fn mcp_environment_values_are_independent_between_servers() {
        let first = stdio_mcp("first", true);
        let mut second = stdio_mcp("second", true);
        let AgentMcpTransport::Stdio { environment, .. } = &mut second.transport else {
            unreachable!();
        };
        environment[0].value = "different-value".to_string();

        assert!(normalize_mcp_servers(vec![first, second]).is_ok());
    }

    #[test]
    fn mcp_connection_errors_exclude_non_mcp_extension_failures() {
        let mcp_keys = HashSet::from(["fixturestdio".to_string()]);
        let errors = mcp_connection_errors(
            vec![
                goose::agents::ExtensionLoadResult {
                    name: "developer".to_string(),
                    success: false,
                    error: Some("built-in failed".to_string()),
                },
                goose::agents::ExtensionLoadResult {
                    name: "Fixture STDIO".to_string(),
                    success: false,
                    error: Some("server failed".to_string()),
                },
                goose::agents::ExtensionLoadResult {
                    name: "fixture_stdio".to_string(),
                    success: true,
                    error: None,
                },
            ],
            &mcp_keys,
        );

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].name, "Fixture STDIO");
        assert_eq!(errors[0].error, "server failed");
    }

    #[test]
    fn mcp_connection_error_events_are_bounded() {
        let short = format_mcp_connection_errors(&[
            AgentMcpConnectionError {
                name: "first".to_string(),
                error: "one".to_string(),
            },
            AgentMcpConnectionError {
                name: "second".to_string(),
                error: "two".to_string(),
            },
        ]);
        assert_eq!(
            short,
            "Some MCP servers could not connect: first: one; second: two"
        );

        let many = (0..5)
            .map(|index| AgentMcpConnectionError {
                name: format!("server-{index}"),
                error: "🪿".repeat(MAX_MCP_CONNECTION_ERROR_CHARS + 50),
            })
            .collect::<Vec<_>>();
        let bounded = format_mcp_connection_errors(&many);
        assert!(bounded.contains("server-0"));
        assert!(bounded.contains("server-2"));
        assert!(!bounded.contains("server-3"));
        assert!(bounded.contains("and 2 more"));
        assert!(bounded.contains('…'));
        assert!(bounded.chars().count() <= MAX_AGENT_ERROR_CHARS);
    }

    #[test]
    fn malformed_agent_config_is_rejected_without_being_rewritten() {
        let test_root = std::env::temp_dir().join(format!(
            "maple-agent-malformed-config-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let path = test_root.join("config.json");
        let original = br#"{"defaultModel":"glm-5-2","mcpServers":[{"transport":{"type":"future_transport"}}]}"#;
        fs::create_dir_all(&test_root).unwrap();
        fs::write(&path, original).unwrap();

        assert!(load_agent_config_file(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn legacy_recent_project_root_order_is_preserved_while_invalid_duplicates_are_sanitized() {
        let test_root = recent_roots_test_dir("legacy-order");
        let path = test_root.join("recent_roots.json");
        let mut first = test_recent_root("legacy-first", 10);
        first.name = "Preserved first metadata".to_string();
        let second = test_recent_root("legacy-second", 20);
        let mut duplicate_first = first.clone();
        duplicate_first.name = "Discarded duplicate metadata".to_string();
        duplicate_first.last_used_ms = 999;
        let invalid = RecentProjectRoot {
            path: "relative/project".to_string(),
            name: "invalid".to_string(),
            last_used_ms: 30,
        };
        write_json_file(
            &path,
            &vec![first.clone(), invalid, second.clone(), duplicate_first],
        )
        .unwrap();

        let loaded = load_recent_project_roots_file(&path).unwrap();

        assert_eq!(loaded, vec![first.clone(), second.clone()]);
        let registered =
            register_explicit_project_root_file(&path, Path::new(&second.path), 1_000).unwrap();
        assert_eq!(registered, vec![first.clone(), second.clone()]);
        assert_eq!(
            read_recent_project_roots_file(&path).unwrap(),
            vec![first, second]
        );
        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn registering_recent_project_roots_adds_only_genuinely_new_projects_at_the_top() {
        let test_root = recent_roots_test_dir("registration");
        let file_path = test_root.join("recent_roots.json");
        let first = test_recent_root("register-first", 10);
        let second = test_recent_root("register-second", 20);
        let third_path = test_project_path("register-third");
        write_json_file(&file_path, &vec![first.clone(), second.clone()]).unwrap();

        let original_bytes = fs::read(&file_path).unwrap();
        let existing =
            register_explicit_project_root_file(&file_path, Path::new(&second.path), 2_000)
                .unwrap();
        assert_eq!(existing, vec![first.clone(), second.clone()]);
        assert_eq!(fs::read(&file_path).unwrap(), original_bytes);

        let added =
            register_explicit_project_root_file(&file_path, Path::new(&third_path), 3_000).unwrap();
        assert_eq!(
            recent_root_paths(&added),
            vec![third_path.clone(), first.path.clone(), second.path.clone()]
        );
        assert_eq!(added[0].last_used_ms, 3_000);

        let after_add_bytes = fs::read(&file_path).unwrap();
        let touched_again =
            register_explicit_project_root_file(&file_path, Path::new(&first.path), 4_000).unwrap();
        assert_eq!(touched_again, added);
        assert_eq!(fs::read(&file_path).unwrap(), after_add_bytes);

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn restoring_removed_project_promotes_it_without_touching_other_roots() {
        let first = test_recent_root("restore-first", 10);
        let restored = test_recent_root("restore-target", 20);
        let third = test_recent_root("restore-third", 30);

        let roots = restore_explicit_project_root(
            vec![first.clone(), restored.clone(), third.clone()],
            Path::new(&restored.path),
            1_000,
        );

        assert_eq!(
            recent_root_paths(&roots),
            vec![restored.path, first.path, third.path]
        );
        assert_eq!(roots[0].last_used_ms, 1_000);
    }

    #[test]
    fn removing_project_preserves_tasks_mcp_and_skills_configuration_boundaries() {
        let removed = test_project_path("remove-target");
        let fallback = test_project_path("remove-fallback");
        let mut config = AgentConfig {
            default_project_root: Some(removed.clone()),
            default_model: "test-model".to_string(),
            mcp_servers: vec![stdio_mcp("kept-mcp", true)],
            project_trust: vec![AgentProjectTrust {
                path: removed.clone(),
                trusted: true,
            }],
            removed_project_roots: Vec::new(),
        };

        apply_project_root_removal(&mut config, &removed, Some(&fallback)).unwrap();

        assert_eq!(
            config.default_project_root.as_deref(),
            Some(fallback.as_str())
        );
        assert_eq!(config.removed_project_roots, vec![removed.clone()]);
        assert_eq!(config.mcp_servers, vec![stdio_mcp("kept-mcp", true)]);
        assert_eq!(
            config.project_trust,
            vec![AgentProjectTrust {
                path: removed,
                trusted: true,
            }]
        );
    }

    #[test]
    fn final_project_removal_clears_runtime_root_and_blocks_hidden_session_creation() {
        let removed = test_project_path("remove-final");
        let mut runtime_root = PathBuf::from(&removed);

        update_runtime_project_root_after_removal(&mut runtime_root, &removed, None);

        assert!(runtime_root.as_os_str().is_empty());
        assert!(
            ensure_session_project_root_is_visible(&runtime_root, std::slice::from_ref(&removed))
                .is_err()
        );
        assert!(
            ensure_session_project_root_is_visible(
                Path::new(&removed),
                std::slice::from_ref(&removed),
            )
            .is_err()
        );
        assert!(
            ensure_session_project_root_is_visible(
                Path::new(&test_project_path("remove-visible")),
                std::slice::from_ref(&removed),
            )
            .is_ok()
        );
    }

    #[test]
    fn running_task_guard_matches_only_tasks_in_the_removed_project() {
        let session_roots = HashMap::from([
            ("running-target".to_string(), "/target".to_string()),
            ("running-other".to_string(), "/other".to_string()),
        ]);
        let active = HashSet::from(["running-target".to_string()]);
        assert!(project_has_active_session_run(
            &session_roots,
            &active,
            "/target"
        ));
        assert!(!project_has_active_session_run(
            &session_roots,
            &active,
            "/other"
        ));
    }

    #[test]
    fn only_explicit_folder_add_can_call_recent_project_registration() {
        // Starting a runtime and creating/loading a session need the full Goose/Tauri stack in
        // command tests. Guard the stronger architectural invariant instead: the registration
        // helper has exactly one caller (agent_save_recent_project_root) plus its definition.
        // Any attempt to touch recent-root membership from a use/session path fails this test.
        let registration_helper = concat!("register_explicit_project_root", "_inner(");
        assert_eq!(
            include_str!("agent.rs")
                .matches(registration_helper)
                .count(),
            2
        );
    }

    #[test]
    fn resolving_legacy_session_derived_root_preserves_position_when_explicitly_saved() {
        let test_root = recent_roots_test_dir("legacy-capped-session-root");
        let file_path = test_root.join("recent_roots.json");
        let saved_roots = (0..20)
            .map(|index| {
                let path = test_root.join(format!("saved-{index}"));
                fs::create_dir_all(&path).unwrap();
                project_root_record(path.to_string_lossy().to_string(), index)
            })
            .collect::<Vec<_>>();
        let session_derived_root = test_root.join("session-derived");
        fs::create_dir_all(&session_derived_root).unwrap();
        write_json_file(&file_path, &saved_roots).unwrap();
        let original = fs::read(&file_path).unwrap();

        let resolved = resolve_project_root(
            Some(&session_derived_root.to_string_lossy()),
            &AgentConfig::default(),
        )
        .unwrap();

        assert_eq!(resolved, session_derived_root.canonicalize().unwrap());
        assert_eq!(fs::read(&file_path).unwrap(), original);
        assert_eq!(
            load_recent_project_roots_file(&file_path).unwrap(),
            saved_roots
        );

        let mut visible_order = recent_root_paths(&saved_roots);
        visible_order.push(path_string(&resolved));
        let explicitly_saved =
            save_project_root_order_file(&file_path, visible_order.clone(), 2_000).unwrap();
        assert_eq!(recent_root_paths(&explicitly_saved), visible_order);
        assert_eq!(
            explicitly_saved.last().unwrap().path,
            path_string(&resolved)
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn explicit_project_root_order_round_trips_first_middle_and_last_positions() {
        let test_root = recent_roots_test_dir("round-trip");
        let file_path = test_root.join("recent_roots.json");
        let first = test_recent_root("round-trip-first", 10);
        let second = test_recent_root("round-trip-second", 20);
        let third = test_recent_root("round-trip-third", 30);
        write_json_file(
            &file_path,
            &vec![first.clone(), second.clone(), third.clone()],
        )
        .unwrap();

        let first_to_middle = vec![second.path.clone(), first.path.clone(), third.path.clone()];
        save_project_root_order_file(&file_path, first_to_middle.clone(), 100).unwrap();
        assert_eq!(
            recent_root_paths(&load_recent_project_roots_file(&file_path).unwrap()),
            first_to_middle
        );

        let middle_to_first = vec![first.path.clone(), second.path.clone(), third.path.clone()];
        save_project_root_order_file(&file_path, middle_to_first.clone(), 200).unwrap();
        assert_eq!(
            recent_root_paths(&load_recent_project_roots_file(&file_path).unwrap()),
            middle_to_first
        );

        let first_to_last = vec![second.path.clone(), third.path.clone(), first.path.clone()];
        save_project_root_order_file(&file_path, first_to_last.clone(), 300).unwrap();
        assert_eq!(
            recent_root_paths(&load_recent_project_roots_file(&file_path).unwrap()),
            first_to_last
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn explicit_project_root_order_deduplicates_ignores_malformed_and_adds_offline_roots() {
        let test_root = recent_roots_test_dir("request-sanitizing");
        let file_path = test_root.join("recent_roots.json");
        let first = test_recent_root("sanitize-first", 10);
        let second = test_recent_root("sanitize-second", 20);
        let third = test_recent_root("sanitize-third", 30);
        let offline_path = test_root
            .join("offline-project")
            .to_string_lossy()
            .to_string();
        assert!(!Path::new(&offline_path).exists());
        write_json_file(
            &file_path,
            &vec![first.clone(), second.clone(), third.clone()],
        )
        .unwrap();

        let saved = save_project_root_order_file(
            &file_path,
            vec![
                second.path.clone(),
                second.path.clone(),
                "relative/project".to_string(),
                String::new(),
                third.path.clone(),
                format!("{}\0invalid", test_project_path("nul")),
                first.path.clone(),
                offline_path.clone(),
            ],
            400,
        )
        .unwrap();

        assert_eq!(
            recent_root_paths(&saved),
            vec![second.path, third.path, first.path, offline_path.clone()]
        );
        assert_eq!(saved.last().unwrap().last_used_ms, 400);
        assert_eq!(load_recent_project_roots_file(&file_path).unwrap(), saved);

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn stale_project_root_order_requests_are_rejected_without_modifying_the_file() {
        let test_root = recent_roots_test_dir("stale-request");
        let file_path = test_root.join("recent_roots.json");
        let first = test_recent_root("stale-first", 10);
        let second = test_recent_root("stale-second", 20);
        let third = test_recent_root("stale-third", 30);
        write_json_file(
            &file_path,
            &vec![first.clone(), second.clone(), third.clone()],
        )
        .unwrap();
        let original = fs::read(&file_path).unwrap();

        let error = save_project_root_order_file(
            &file_path,
            vec![
                third.path.clone(),
                "relative/ignored".to_string(),
                first.path.clone(),
            ],
            500,
        )
        .unwrap_err();

        assert!(error.to_string().contains("stale"));
        assert!(error.to_string().contains(&second.path));
        assert_eq!(fs::read(&file_path).unwrap(), original.to_vec());
        assert_eq!(
            load_recent_project_roots_file(&file_path).unwrap(),
            vec![first, second, third]
        );

        let malformed_only = save_project_root_order_file(
            &file_path,
            vec![String::new(), "still/relative".to_string()],
            600,
        );
        assert!(malformed_only.is_err());
        assert_eq!(fs::read(&file_path).unwrap(), original.to_vec());

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn corrupt_recent_project_roots_are_never_overwritten_by_registration_or_reorder() {
        let test_root = recent_roots_test_dir("corrupt-json");
        let file_path = test_root.join("recent_roots.json");
        let original = br#"[{"path":"unterminated""#;
        fs::create_dir_all(&test_root).unwrap();
        fs::write(&file_path, original).unwrap();
        let project_path = test_project_path("corrupt-new");

        assert!(
            register_explicit_project_root_file(&file_path, Path::new(&project_path), 700).is_err()
        );
        assert_eq!(fs::read(&file_path).unwrap(), original);

        assert!(save_project_root_order_file(&file_path, vec![project_path], 800).is_err());
        assert_eq!(fs::read(&file_path).unwrap(), original);

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn recent_project_root_persistence_has_no_twenty_project_cap() {
        let test_root = recent_roots_test_dir("more-than-twenty");
        let file_path = test_root.join("recent_roots.json");
        let paths = (0..25)
            .map(|index| test_project_path(&format!("uncapped-{index}")))
            .collect::<Vec<_>>();

        for (index, path) in paths.iter().enumerate() {
            register_explicit_project_root_file(&file_path, Path::new(path), index as u128)
                .unwrap();
        }
        assert_eq!(
            load_recent_project_roots_file(&file_path).unwrap().len(),
            25
        );

        let saved = save_project_root_order_file(&file_path, paths.clone(), 900).unwrap();
        assert_eq!(recent_root_paths(&saved), paths);
        assert_eq!(
            load_recent_project_roots_file(&file_path).unwrap().len(),
            25
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn recent_project_root_files_remain_isolated_by_account_scope() {
        let test_root = recent_roots_test_dir("account-isolation");
        let first_scope = account_scope("recent-roots-user-a").unwrap();
        let second_scope = account_scope("recent-roots-user-b").unwrap();
        let first_file = test_root
            .join("accounts")
            .join(first_scope)
            .join("recent_roots.json");
        let second_file = test_root
            .join("accounts")
            .join(second_scope)
            .join("recent_roots.json");
        let first_project = test_project_path("account-a-project");
        let second_project = test_project_path("account-b-project");

        register_explicit_project_root_file(&first_file, Path::new(&first_project), 1_000).unwrap();
        register_explicit_project_root_file(&second_file, Path::new(&second_project), 2_000)
            .unwrap();

        assert_eq!(
            recent_root_paths(&load_recent_project_roots_file(&first_file).unwrap()),
            vec![first_project]
        );
        assert_eq!(
            recent_root_paths(&load_recent_project_roots_file(&second_file).unwrap()),
            vec![second_project]
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn agent_sessions_remain_sorted_by_updated_time_newest_first() {
        let summary = |id: &str, updated_ms: i64| AgentSessionSummary {
            web_enabled: true,
            id: id.to_string(),
            title: id.to_string(),
            project_root: test_project_path("session-sort"),
            created_ms: 0,
            updated_ms,
            message_count: 0,
            model: None,
            mode: DEFAULT_GOOSE_MODE.to_string(),
            archived: false,
            acp: false,
        };
        let mut sessions = vec![
            summary("oldest", 10),
            summary("newest", 30),
            summary("middle", 20),
        ];

        sort_sessions_newest_first(&mut sessions);

        assert_eq!(
            sessions
                .into_iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![
                "newest".to_string(),
                "middle".to_string(),
                "oldest".to_string()
            ]
        );
    }

    #[test]
    fn mcp_selection_distinguishes_defaults_from_explicit_empty() {
        let configured = normalize_mcp_servers(vec![
            stdio_mcp("default", true),
            stdio_mcp("optional", false),
        ])
        .unwrap();
        assert_eq!(select_mcp_servers(&configured, None).unwrap().len(), 1);
        assert!(
            select_mcp_servers(&configured, Some(&[]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            select_mcp_servers(&configured, Some(&["optional".to_string()])).unwrap()[0].name,
            "optional"
        );
    }

    #[test]
    fn catalog_vision_preserves_explicit_alias_capabilities() {
        let catalog = serde_json::from_value(json!({
            "object": "list",
            "data": [{"id": "vision-model", "capabilities": {"vision": true}}],
            "aliases": [
                {"id": "explicit-no", "target_model": "vision-model", "capabilities": {"vision": false}},
                {"id": "explicit-yes", "target_model": "missing-model", "capabilities": {"vision": true}},
                {"id": "default-no", "target_model": "vision-model", "capabilities": {}}
            ]
        }))
        .unwrap();
        assert_eq!(
            catalog_supports_vision(&catalog, "explicit-no"),
            Some(false)
        );
        assert_eq!(
            catalog_supports_vision(&catalog, "explicit-yes"),
            Some(true)
        );
        assert_eq!(catalog_supports_vision(&catalog, "default-no"), Some(false));
    }

    #[test]
    fn catalog_vision_falls_back_only_when_alias_capabilities_are_absent() {
        let catalog = serde_json::from_value(json!({
            "object": "list",
            "data": [
                {"id": "vision-model", "capabilities": {"vision": true}},
                {"id": "text-model", "capabilities": {"vision": false}},
                {"id": "unknown-model"}
            ],
            "aliases": [
                {"id": "vision-alias", "target_model": "vision-model"},
                {"id": "text-alias", "target_model": "text-model", "capabilities": null},
                {"id": "unknown-alias", "target_model": "unknown-model"},
                {"id": "missing-alias", "target_model": "missing-model"},
                {"id": "empty-alias", "target_model": " "}
            ]
        }))
        .unwrap();
        for (id, expected) in [
            ("vision-model", Some(true)),
            ("vision-alias", Some(true)),
            ("text-model", Some(false)),
            ("text-alias", Some(false)),
            ("unknown-model", None),
            ("unknown-alias", None),
            ("missing-alias", None),
            ("empty-alias", None),
            ("nonexistent", None),
        ] {
            assert_eq!(catalog_supports_vision(&catalog, id), expected, "{id}");
        }
    }

    #[test]
    fn agent_send_vision_capability_is_catalog_driven_and_fails_closed() {
        let without_capability: AgentSendMessageRequest = serde_json::from_value(json!({
            "sessionId": "session-1",
            "text": "Inspect the image",
            "model": "future-vision-model",
            "mode": "smart_approve"
        }))
        .unwrap();
        assert!(!without_capability.vision_capable);
        assert_eq!(without_capability.context_limit, None);

        let with_capability: AgentSendMessageRequest = serde_json::from_value(json!({
            "sessionId": "session-1",
            "text": "Inspect the image",
            "model": "future-vision-model",
            "mode": "smart_approve",
            "contextLimit": 384000,
            "visionCapable": true
        }))
        .unwrap();
        assert!(with_capability.vision_capable);
        assert_eq!(with_capability.context_limit, Some(384_000));

        let create_request: AgentCreateSessionRequest = serde_json::from_value(json!({
            "projectRoot": "/tmp/project",
            "model": "kimi-k2-6",
            "contextLimit": 256000
        }))
        .unwrap();
        assert_eq!(create_request.context_limit, Some(256_000));
    }

    #[test]
    fn maple_model_config_uses_only_valid_per_session_context_limits() {
        assert_eq!(
            maple_model_config("glm-5-2", Some(384_000))
                .unwrap()
                .context_limit,
            Some(384_000)
        );
        assert_eq!(
            maple_model_config("auto:powerful", Some(256_000))
                .unwrap()
                .context_limit,
            Some(256_000)
        );
        assert_eq!(
            maple_model_config("glm-5-2", None).unwrap().context_limit,
            None
        );
        assert_eq!(
            maple_model_config("glm-5-2", Some(0))
                .unwrap()
                .context_limit,
            None
        );
    }

    #[test]
    fn agent_session_model_locks_after_first_message() {
        assert!(validate_session_model_lock(0, Some("glm-5-2"), "gemma4-31b").is_ok());
        assert!(validate_session_model_lock(3, Some("glm-5-2"), "glm-5-2").is_ok());
        let error = validate_session_model_lock(3, Some("glm-5-2"), "gemma4-31b").unwrap_err();
        assert!(error.contains("locked to model glm-5-2"));
        assert!(error.contains("Start a new task"));
    }

    #[tokio::test]
    async fn stale_model_catalog_serves_without_waiting_on_the_network() {
        let root = std::env::temp_dir().join(format!(
            "maple-model-catalog-stale-{}-{}",
            std::process::id(),
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let paths =
            AgentPathLayout::from_app_roots(root.join("app-config"), root.join("app-local-data"));
        let cache_path = model_catalog_cache_path(&paths, "cache-user").unwrap();
        fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        fs::write(
            &cache_path,
            serde_json::to_string(&CachedModelCatalog {
                fetched_at_ms: 0,
                models: vec!["model-a".to_string(), "model-b".to_string()],
            })
            .unwrap(),
        )
        .unwrap();
        // The test session points at an unreachable endpoint: a blocking
        // fetch would fail the elapsed-time bound below.
        let session = crate::maple_api::test_maple_api_session("cache-user");
        let started = std::time::Instant::now();
        let fetch = cached_model_catalog_fetch(&paths, "cache-user", &session, 0).await;
        let elapsed = started.elapsed();
        assert!(
            matches!(&fetch, CatalogFetch::Stale(models) if models == &["model-a".to_string(), "model-b".to_string()]),
            "expected the stale list, got {fetch:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "a stale cache must answer without the network, took {elapsed:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn model_catalog_cache_freshness_follows_the_ttl() {
        assert!(!model_catalog_is_fresh(0), "epoch means never fetched");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(model_catalog_is_fresh(now));
        assert!(!model_catalog_is_fresh(now - MODEL_CATALOG_TTL_MS - 1_000));
    }

    #[tokio::test]
    async fn persisted_permission_mode_survives_a_reload() {
        let agent = test_support::started_agent_runtime("persist-mode").await;
        let created = agent
            .handle
            .create_session(Some(AgentCreateSessionRequest {
                project_root: Some(agent.project_root.to_string_lossy().into_owned()),
                title: Some("mode target".to_string()),
                model: None,
                context_limit: None,
                mode: Some("smart_approve".to_string()),
                mcp_server_names: None,
                system_prompt: None,
            }))
            .await
            .unwrap();
        agent
            .handle
            .persist_session_permission_mode(&created.session.id, "auto")
            .await
            .unwrap();
        let summaries = agent
            .handle
            .list_sessions(Some(agent.project_root.to_string_lossy().into_owned()))
            .await
            .unwrap();
        let summary = summaries
            .iter()
            .find(|summary| summary.id == created.session.id)
            .expect("the task should be listed");
        assert_eq!(summary.mode, "auto");
    }

    #[test]
    fn external_surfaces_admit_caller_mediated_and_auto_modes_only() {
        // The stdio-surface test pins the same policy end to end; this is
        // the direct pin so a predicate edit cannot drift silently.
        assert!(is_caller_mediated_mode(
            parse_user_permission_mode("smart_approve").unwrap()
        ));
        assert!(is_caller_mediated_mode(
            parse_user_permission_mode("approve").unwrap()
        ));
        assert!(!is_caller_mediated_mode(
            parse_user_permission_mode("auto").unwrap()
        ));
        assert!(parse_user_permission_mode("chat").is_err());
    }

    #[tokio::test]
    async fn permission_policy_is_session_scoped_and_mutable_mid_run() {
        assert_eq!(
            parse_user_permission_mode("smart_approve"),
            Ok(GooseMode::SmartApprove)
        );
        assert_eq!(parse_user_permission_mode("auto"), Ok(GooseMode::Auto));
        assert!(parse_user_permission_mode("approve") == Ok(GooseMode::Approve));

        let modes = SessionPermissionModes::default();
        assert_eq!(
            selected_permission_mode(&modes, "session-1").await,
            GooseMode::SmartApprove
        );
        modes
            .lock()
            .await
            .insert("session-1".to_string(), GooseMode::Auto);
        assert_eq!(
            selected_permission_mode(&modes, "session-1").await,
            GooseMode::Auto
        );
        assert_eq!(
            selected_permission_mode(&modes, "session-2").await,
            GooseMode::SmartApprove
        );

        let mut claimed = HashMap::from([("session-1".to_string(), GooseMode::SmartApprove)]);
        assert_eq!(
            select_session_permission_mode(&mut claimed, "session-1", GooseMode::Auto),
            (GooseMode::SmartApprove, ModeRollback::None),
            "a delayed send must not overwrite a newer authoritative policy"
        );
        assert_eq!(
            select_session_permission_mode(&mut claimed, "session-2", GooseMode::Auto),
            (
                GooseMode::Auto,
                ModeRollback::Remove("session-2".to_string())
            )
        );
    }

    #[test]
    fn agent_mode_accepts_only_one_shot_permission_decisions() {
        assert_eq!(
            permission_from_decision("allow_once").unwrap(),
            Permission::AllowOnce
        );
        assert_eq!(
            permission_from_decision("deny_once").unwrap(),
            Permission::DenyOnce
        );
        assert!(permission_from_decision("always_allow").is_err());
        assert!(permission_from_decision("always_deny").is_err());
    }

    fn delegate_message(id: &str, arguments: serde_json::Map<String, Value>) -> Message {
        Message::assistant().with_tool_request(
            id,
            Ok(
                rmcp::model::CallToolRequestParams::new(SUBAGENT_DELEGATE_TOOL.to_string())
                    .with_arguments(arguments),
            ),
        )
    }

    fn tool_result_message(id: &str, result: CallToolResult) -> Message {
        Message::user().with_tool_response(id, Ok(result))
    }

    fn subagent_notification(session_id: &str, tool: &str, arguments: Value) -> ServerNotification {
        #[expect(deprecated)]
        ServerNotification::LoggingMessageNotification(rmcp::model::Notification::new(
            rmcp::model::LoggingMessageNotificationParam::new(
                rmcp::model::LoggingLevel::Info,
                json!({
                    "type": SUBAGENT_TOOL_REQUEST_TYPE,
                    "subagent_id": session_id,
                    "tool_call": { "name": tool, "arguments": arguments },
                }),
            ),
        ))
    }

    #[test]
    fn subagent_card_follows_one_delegated_task() {
        let mut tracker = SubagentTracker::default();
        let started = subagent_events(
            &mut tracker,
            &delegate_message(
                "delegate-1",
                rmcp::object!({ "instructions": "Review the parser\nand report back" }),
            ),
        );
        assert!(matches!(
            started.as_slice(),
            [AgentRunEvent::SubagentStarted { id, task, background: false }]
                if id == "delegate-1" && task == "Review the parser"
        ));
        // The same tool-request message repeated must not add a row.
        assert!(
            subagent_events(
                &mut tracker,
                &delegate_message("delegate-1", rmcp::object!({ "instructions": "Review" })),
            )
            .is_empty()
        );

        let activity = tracker.notification(
            "delegate-1",
            &subagent_notification("sub-1", "shell", json!({ "command": "cargo test" })),
        );
        assert!(matches!(
            activity,
            Some(AgentRunEvent::SubagentActivity { ref id, ref tool })
                if id == "delegate-1" && tool == "Terminal: cargo test"
        ));
        // A notification for a tool call Maple is not tracking is ignored.
        assert!(
            tracker
                .notification(
                    "other-request",
                    &subagent_notification("sub-1", "shell", json!({ "command": "ls" }))
                )
                .is_none()
        );

        let finished = subagent_events(
            &mut tracker,
            &tool_result_message(
                "delegate-1",
                CallToolResult::success(vec![ContentBlock::text("done")]),
            ),
        );
        assert!(matches!(
            finished.as_slice(),
            [AgentRunEvent::SubagentFinished { id }] if id == "delegate-1"
        ));
        assert!(tracker.running.is_empty());
    }

    #[test]
    fn background_subagent_stays_until_its_result_is_collected() {
        let mut tracker = SubagentTracker::default();
        subagent_events(
            &mut tracker,
            &delegate_message(
                "delegate-1",
                rmcp::object!({ "instructions": "Build the release", "async": true }),
            ),
        );
        let mut meta = rmcp::model::MetaObject::new();
        meta.0.insert(
            "subagent_session_id".to_string(),
            Value::String("task-9".to_string()),
        );
        // The call returns at once; the subagent keeps working.
        assert!(
            subagent_events(
                &mut tracker,
                &tool_result_message(
                    "delegate-1",
                    CallToolResult::success(vec![ContentBlock::text("Task task-9 started")])
                        .with_meta(Some(meta)),
                ),
            )
            .is_empty()
        );

        // Nothing pushes the end of a background task back, so it must
        // be handed to a watcher exactly once.
        assert_eq!(
            tracker.take_unwatched(),
            vec![("delegate-1".to_string(), "task-9".to_string())]
        );
        assert!(tracker.take_unwatched().is_empty());

        // A peek reports progress against the same row and leaves it.
        let peeking = Message::assistant().with_tool_request(
            "load-1",
            Ok(
                rmcp::model::CallToolRequestParams::new(SUBAGENT_LOAD_TOOL.to_string())
                    .with_arguments(rmcp::object!({ "source": "task-9", "peek": true })),
            ),
        );
        assert!(subagent_events(&mut tracker, &peeking).is_empty());
        assert!(matches!(
            tracker.notification(
                "load-1",
                &subagent_notification("task-9", "write", json!({ "path": "out.txt" })),
            ),
            Some(AgentRunEvent::SubagentActivity { ref id, .. }) if id == "delegate-1"
        ));
        assert!(
            subagent_events(
                &mut tracker,
                &tool_result_message(
                    "load-1",
                    CallToolResult::success(vec![ContentBlock::text("still running")]),
                ),
            )
            .is_empty()
        );

        // Collecting the result ends the row.
        let collecting = Message::assistant().with_tool_request(
            "load-2",
            Ok(
                rmcp::model::CallToolRequestParams::new(SUBAGENT_LOAD_TOOL.to_string())
                    .with_arguments(rmcp::object!({ "source": "task-9" })),
            ),
        );
        assert!(subagent_events(&mut tracker, &collecting).is_empty());
        let finished = subagent_events(
            &mut tracker,
            &tool_result_message(
                "load-2",
                CallToolResult::success(vec![ContentBlock::text("all done")]),
            ),
        );
        assert!(matches!(
            finished.as_slice(),
            [AgentRunEvent::SubagentFinished { id }] if id == "delegate-1"
        ));
        assert!(tracker.running.is_empty());
        assert!(tracker.background.is_empty());
    }

    #[test]
    fn a_delegate_call_that_fails_clears_its_row() {
        let mut tracker = SubagentTracker::default();
        subagent_events(
            &mut tracker,
            &delegate_message(
                "delegate-1",
                rmcp::object!({ "source": "reviewer", "async": true }),
            ),
        );
        // A denied or failed call carries no task ID to follow.
        let finished = subagent_events(
            &mut tracker,
            &Message::user().with_tool_response(
                "delegate-1",
                Err(rmcp::model::ErrorData::new(
                    rmcp::model::ErrorCode::INTERNAL_ERROR,
                    "denied".to_string(),
                    None,
                )),
            ),
        );
        assert!(matches!(
            finished.as_slice(),
            [AgentRunEvent::SubagentFinished { id }] if id == "delegate-1"
        ));
    }

    #[test]
    fn a_run_that_ends_keeps_only_its_background_subagents() {
        let mut tracker = SubagentTracker::default();
        subagent_events(
            &mut tracker,
            &delegate_message(
                "waited-for",
                rmcp::object!({ "instructions": "Review this" }),
            ),
        );
        subagent_events(
            &mut tracker,
            &delegate_message(
                "in-background",
                rmcp::object!({ "instructions": "Build the release", "async": true }),
            ),
        );
        let mut meta = rmcp::model::MetaObject::new();
        meta.0.insert(
            "subagent_session_id".to_string(),
            Value::String("task-9".to_string()),
        );
        subagent_events(
            &mut tracker,
            &tool_result_message(
                "in-background",
                CallToolResult::success(vec![ContentBlock::text("started")]).with_meta(Some(meta)),
            ),
        );

        // The turn ends while the subagent Maple waited for is still
        // pending, which is what a Stop leaves behind.
        tracker.retain_background();
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 1, "only the background subagent works on");
        assert_eq!(snapshot[0].id, "in-background");
        assert!(snapshot[0].background);
        assert_eq!(snapshot[0].task, "Build the release");

        // The next turn of the same task collects it, and the row ends.
        let collecting = Message::assistant().with_tool_request(
            "load-1",
            Ok(
                rmcp::model::CallToolRequestParams::new(SUBAGENT_LOAD_TOOL.to_string())
                    .with_arguments(rmcp::object!({ "source": "task-9" })),
            ),
        );
        assert!(subagent_events(&mut tracker, &collecting).is_empty());
        let finished = subagent_events(
            &mut tracker,
            &tool_result_message(
                "load-1",
                CallToolResult::success(vec![ContentBlock::text("all done")]),
            ),
        );
        assert!(matches!(
            finished.as_slice(),
            [AgentRunEvent::SubagentFinished { id }] if id == "in-background"
        ));
        assert!(tracker.is_empty());
    }

    #[test]
    fn a_snapshot_repeats_the_latest_tool_of_each_subagent() {
        let mut tracker = SubagentTracker::default();
        subagent_events(
            &mut tracker,
            &delegate_message(
                "delegate-1",
                rmcp::object!({ "instructions": "Check the tests", "async": true }),
            ),
        );
        tracker.notification(
            "delegate-1",
            &subagent_notification("sub-1", "shell", json!({ "command": "cargo test" })),
        );
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot[0].activity.as_deref(),
            Some("Terminal: cargo test")
        );
    }

    #[tokio::test]
    async fn the_subagent_extension_exposes_the_tools_the_permission_file_names() {
        let root = std::env::temp_dir().join(format!(
            "maple-subagent-extension-{}-{}",
            std::process::id(),
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let manager = Arc::new(
            goose::agents::extension_manager::ExtensionManager::new_without_provider(
                root.join("data"),
            ),
        );
        manager
            .add_extension(maple_subagent_extension_config(), None, None, Some("s1"))
            .await
            .expect("Goose must still ship the summon platform extension");

        let tools = manager.get_prefixed_tools("s1", None).await.unwrap();
        let names = tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>();
        // Maple's permission file keys on these exact names; a prefix or a
        // rename would route the calls past the ask-before policy.
        for tool in MAPLE_SUBAGENT_TOOLS {
            assert!(names.contains(&tool), "{tool} is missing from {names:?}");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn maple_permission_file_forces_every_routed_tool_through_ask_before() {
        let root = std::env::temp_dir().join(format!(
            "maple-permissions-{}-{}",
            std::process::id(),
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("permission.yaml");
        fs::write(
            &path,
            "user:\n  always_allow:\n  - shell\n  - external_mcp\n  ask_before: []\n  never_allow: []\n",
        )
        .unwrap();

        reset_maple_owned_permission_file(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "the reset file must be owner-only"
            );
        }
        assert!(
            !fs::read_dir(&root).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
            "the atomic write must leave no temporary file behind"
        );
        let manager = PermissionManager::new(root.clone());
        for tool in MAPLE_DEVELOPER_TOOLS {
            // todo_write only records plan state for the UI and
            // request_user_input only opens a prompt; neither has side
            // effects and both are always allowed. read stays in ask_before:
            // under smart_approve Maple's read-only automation approves
            // benign local reads and still prompts for secret paths.
            let expected = if matches!(tool, "todo_write" | "request_user_input") {
                goose::config::permission::PermissionLevel::AlwaysAllow
            } else {
                goose::config::permission::PermissionLevel::AskBefore
            };
            assert_eq!(manager.get_user_permission(tool), Some(expected));
        }
        assert_eq!(
            manager.get_user_permission("load_skill"),
            Some(goose::config::permission::PermissionLevel::AlwaysAllow)
        );
        for tool in MAPLE_SUBAGENT_TOOLS {
            // Delegation is an explicit admission boundary; load only
            // collects a result. The pinned fork forwards child approvals.
            let expected = if tool == "delegate" {
                goose::config::permission::PermissionLevel::AskBefore
            } else {
                goose::config::permission::PermissionLevel::AlwaysAllow
            };
            assert_eq!(manager.get_user_permission(tool), Some(expected));
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn explicit_web_ask_before_overrides_annotations_and_smart_cache() {
        use goose::permission::permission_inspector::PermissionInspector;
        use goose::tool_inspection::{InspectionAction, ToolInspector};
        use rmcp::model::CallToolRequestParams;

        let root = std::env::temp_dir().join(format!(
            "maple-web-permissions-{}-{}",
            std::process::id(),
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        reset_maple_owned_permission_file(&root.join("permission.yaml")).unwrap();
        let manager = Arc::new(PermissionManager::new(root.clone()));
        let provider: goose::agents::types::SharedProvider = Arc::new(Mutex::new(None));
        let inspector = PermissionInspector::new(
            Arc::clone(&manager),
            provider,
            Arc::new(SessionManager::new(root.join("data"))),
        );
        inspector
            .apply_tool_annotations(&[web_tools::web_search_tool(), web_tools::open_url_tool()]);
        for tool in ["web_search", "open_url"] {
            manager.update_smart_approve_permission(
                tool,
                goose::config::permission::PermissionLevel::AlwaysAllow,
            );
        }

        let message = Message::assistant()
            .with_tool_request(
                "search-request",
                Ok(CallToolRequestParams::new("web_search".to_string())
                    .with_arguments(rmcp::object!({ "query": "maple" }))),
            )
            .with_tool_request(
                "open-request",
                Ok(
                    CallToolRequestParams::new("open_url".to_string()).with_arguments(
                        rmcp::object!({
                            "url": "https://example.com",
                            "purpose": "Read the source"
                        }),
                    ),
                ),
            );
        let requests = message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::ToolRequest(request) => Some(request.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let results = inspector
            .inspect("session", &requests, &[], GooseMode::SmartApprove)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|result| matches!(result.action, InspectionAction::RequireApproval(None)))
        );
        for tool in ["web_search", "open_url"] {
            assert_eq!(
                manager.get_smart_approve_permission(tool),
                Some(goose::config::permission::PermissionLevel::AlwaysAllow)
            );
            assert_eq!(
                manager.get_user_permission(tool),
                Some(goose::config::permission::PermissionLevel::AskBefore)
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_powerful_agent_default_migrates_to_glm() {
        let mut config = AgentConfig {
            default_project_root: Some("/tmp/project".to_string()),
            default_model: LEGACY_AGENT_DEFAULT_MODEL.to_string(),
            mcp_servers: Vec::new(),
            project_trust: Vec::new(),
            removed_project_roots: Vec::new(),
        };

        assert!(migrate_agent_config(&mut config));
        assert_eq!(config.default_model, DEFAULT_AGENT_MODEL);
        assert!(!migrate_agent_config(&mut config));
    }

    #[test]
    fn explicit_agent_model_choices_are_not_migrated() {
        for model in ["kimi-k2-6", "auto:quick", "glm-5-2", "gemma-3-27b"] {
            let mut config = AgentConfig {
                default_project_root: None,
                default_model: model.to_string(),
                mcp_servers: Vec::new(),
                project_trust: Vec::new(),
                removed_project_roots: Vec::new(),
            };

            assert!(!migrate_agent_config(&mut config));
            assert_eq!(config.default_model, model);
        }
    }

    #[test]
    fn startup_root_skips_a_removed_default_project() {
        let removed_dir = std::env::temp_dir().join(format!(
            "maple-agent-removed-default-{}",
            std::process::id()
        ));
        fs::create_dir_all(&removed_dir).unwrap();
        let removed = path_string(&removed_dir.canonicalize().unwrap());
        let config = AgentConfig {
            default_project_root: Some(removed.clone()),
            default_model: DEFAULT_AGENT_MODEL.to_string(),
            mcp_servers: Vec::new(),
            project_trust: Vec::new(),
            removed_project_roots: vec![removed.clone()],
        };

        let resolved = resolve_project_root(None, &config).unwrap();
        assert_ne!(path_string(&resolved), removed);

        let mut visible = config.clone();
        visible.removed_project_roots.clear();
        assert_eq!(
            path_string(&resolve_project_root(None, &visible).unwrap()),
            removed
        );
        let _ = fs::remove_dir_all(&removed_dir);
    }

    #[test]
    fn image_history_payloads_do_not_create_timeline_rows() {
        let message = Message::user()
            .with_id("image-message")
            .with_text("Inspect this image")
            .with_image("aW1hZ2U=", "image/png");

        let items = message_to_timeline_items(&message, false);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "image-message-text");
        assert_eq!(items[0].text.as_deref(), Some("Inspect this image"));
    }

    #[test]
    fn agent_image_messages_keep_the_ui_projection_clean_and_model_routing_truthful() {
        let image = PreparedAgentImage {
            attachment: AgentImageAttachment {
                id: "0123456789abcdef0123456789abcdef".to_string(),
                name: "dialog.png".to_string(),
                mime_type: "image/png".to_string(),
                source: "maple-attachment://0123456789abcdef0123456789abcdef".to_string(),
            },
            base64_data: "aW1hZ2U=".to_string(),
        };

        let nonvision = user_message_with_images(
            "Why is this misaligned?",
            std::slice::from_ref(&image),
            false,
        );
        assert!(
            nonvision
                .as_concat_text()
                .contains(&image.attachment.source)
        );
        assert!(
            !nonvision
                .content
                .iter()
                .any(|content| matches!(content, MessageContent::Image(_)))
        );
        let nonvision_item = message_to_timeline_items(&nonvision, false)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            nonvision_item.text.as_deref(),
            Some("Why is this misaligned?")
        );
        assert_eq!(
            nonvision_item
                .input
                .as_ref()
                .and_then(|input| input.get("imageAttachments"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );

        let vision = user_message_with_images("Why is this misaligned?", &[image], true);
        assert!(!vision.as_concat_text().contains("maple-attachment://"));
        assert!(
            vision
                .content
                .iter()
                .any(|content| matches!(content, MessageContent::Image(_)))
        );
    }

    #[test]
    fn agent_errors_are_bounded_for_the_timeline() {
        let item = error_item("x".repeat(MAX_AGENT_ERROR_CHARS + 100));
        let text = item.text.expect("error should contain a summary");

        assert_eq!(text.chars().count(), MAX_AGENT_ERROR_CHARS + 1);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn terminal_projection_handles_stream_ids_and_idless_collisions() {
        let mut first_chunk = Message::assistant()
            .with_id("stream-message")
            .with_text("Hello");
        first_chunk.created = 100;
        let mut second_chunk = Message::assistant()
            .with_id("stream-message")
            .with_text(" world");
        second_chunk.created = 101;
        let first_items = message_to_timeline_items(&first_chunk, true);
        let second_items = message_to_timeline_items(&second_chunk, true);
        let candidate = update_live_message_candidate(
            Some(live_message_candidate(&first_chunk, &first_items)),
            &second_chunk,
            &second_items,
        );

        let mut persisted = Message::assistant()
            .with_id("stream-message")
            .with_text("Hello world");
        persisted.created = 100;
        let conversation = Conversation::new_unvalidated(vec![persisted]);
        assert!(terminal_message_is_persisted(&conversation, &candidate));

        let mut persisted_reply = Message::assistant().with_text("Persisted reply");
        persisted_reply.created = 200;
        let stored_reply = persisted_reply.clone().with_id("database-id");
        let reply_items = message_to_timeline_items(&persisted_reply, true);
        let reply_candidate = live_message_candidate(&persisted_reply, &reply_items);
        assert!(terminal_message_is_persisted(
            &Conversation::new_unvalidated(vec![stored_reply.clone()]),
            &reply_candidate
        ));

        let mut live_only_notice = Message::assistant().with_text("Transient provider error");
        live_only_notice.created = persisted_reply.created;
        let notice_items = message_to_timeline_items(&live_only_notice, true);
        let notice_candidate =
            update_live_message_candidate(Some(reply_candidate), &live_only_notice, &notice_items);
        assert_eq!(notice_candidate.items.len(), 1);
        assert_eq!(
            notice_candidate.items[0].text.as_deref(),
            Some("Transient provider error")
        );
        assert!(!terminal_message_is_persisted(
            &Conversation::new_unvalidated(vec![stored_reply]),
            &notice_candidate
        ));

        let mut same_id_notice = Message::assistant()
            .with_id("stream-message")
            .with_system_notification(SystemNotificationType::InlineMessage, "Live-only notice");
        same_id_notice.created = 100;
        let same_id_items = message_to_timeline_items(&same_id_notice, true);
        let same_id_candidate =
            update_live_message_candidate(Some(candidate), &same_id_notice, &same_id_items);
        assert_eq!(same_id_candidate.items.len(), 1);
        assert!(!terminal_message_is_persisted(
            &conversation,
            &same_id_candidate
        ));
    }

    #[tokio::test]
    async fn completed_timeline_reuses_session_load_and_retains_only_live_only_message() {
        let session_id = "session";
        let mut live_reply = Message::assistant().with_text("Persisted reply");
        live_reply.created = 300;
        let stored_reply = live_reply.clone().with_id("database-id");
        let persisted_conversation = Conversation::new_unvalidated(vec![stored_reply]);
        let persisted_timeline = conversation_to_timeline_items(&persisted_conversation);
        let reply_items = message_to_timeline_items(&live_reply, true);
        let reply_candidate = live_message_candidate(&live_reply, &reply_items);
        let live_timelines = Arc::new(Mutex::new(HashMap::from([(
            session_id.to_string(),
            test_live_timeline(
                AgentPermissionRouting::Desktop,
                LiveTimeline::Completed(reply_candidate),
            ),
        )])));

        let loaded = overlay_live_timeline(
            &live_timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            &persisted_conversation,
            persisted_timeline.clone(),
        )
        .await;
        assert_eq!(loaded.len(), persisted_timeline.len());
        assert!(!live_timelines.lock().await.contains_key(session_id));

        let mut notice = Message::assistant().with_text("Transient provider error");
        notice.created = live_reply.created;
        let notice_items = message_to_timeline_items(&notice, true);
        let notice_candidate = live_message_candidate(&notice, &notice_items);
        let mut timelines = HashMap::new();

        apply_successful_prompt_outcome(
            &mut timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            &AgentPromptOutcome {
                terminal_message: Some(notice_candidate),
            },
        );
        let live_timelines = Arc::new(Mutex::new(timelines));
        let loaded = overlay_live_timeline(
            &live_timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            &persisted_conversation,
            persisted_timeline,
        )
        .await;
        assert_eq!(
            loaded.last().and_then(|item| item.text.as_deref()),
            Some("Transient provider error")
        );
        assert!(matches!(
            live_timelines.lock().await.get(session_id),
            Some(LiveTimelineEntry {
                routing: AgentPermissionRouting::Desktop,
                timeline: LiveTimeline::Completed(_),
            })
        ));

        let mut timelines = live_timelines.lock().await;
        apply_successful_prompt_outcome(
            &mut timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            &AgentPromptOutcome::default(),
        );
        assert!(!timelines.contains_key(session_id));
    }

    #[tokio::test]
    async fn calling_surface_timeline_does_not_overlay_desktop_session_load() {
        let session_id = "calling-surface-session";
        let persisted_conversation = Conversation::new_unvalidated(vec![
            Message::user()
                .with_id("persisted-user")
                .with_text("Persisted prompt"),
            Message::assistant()
                .with_content(MessageContent::action_required(
                    "persisted-request",
                    "shell".to_string(),
                    serde_json::Map::new(),
                    Some("Run this command?".to_string()),
                ))
                .with_generated_id(),
        ]);
        let persisted = conversation_to_timeline_items(&persisted_conversation);
        let permission = AgentTimelineItem {
            id: "permission-request-1".to_string(),
            item_type: "permission".to_string(),
            role: Some("assistant".to_string()),
            title: Some("shell".to_string()),
            text: Some("Run this command?".to_string()),
            status: Some("pending".to_string()),
            input: Some(json!({ "command": "git status --short" })),
            output: None,
            created_ms: 1,
            merge: "replace".to_string(),
        };
        let live_timelines = Arc::new(Mutex::new(HashMap::from([(
            session_id.to_string(),
            test_live_timeline(
                AgentPermissionRouting::CallingSurface,
                LiveTimeline::Streaming(vec![permission]),
            ),
        )])));

        let mut loaded = overlay_live_timeline(
            &live_timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            &persisted_conversation,
            persisted.clone(),
        )
        .await;
        reconcile_desktop_permission_items(&mut loaded, &HashMap::new(), true);

        assert_eq!(
            loaded
                .iter()
                .find(|item| item.id == "permission-persisted-request")
                .and_then(|item| item.status.as_deref()),
            Some("controlled_externally")
        );
        assert!(!loaded.iter().any(|item| {
            item.item_type == "permission" && item.status.as_deref() == Some("pending")
        }));
        assert_eq!(
            live_timelines.lock().await.get(session_id).unwrap().routing,
            AgentPermissionRouting::CallingSurface
        );
    }

    #[tokio::test]
    async fn permission_status_updates_only_the_owning_surface_timeline() {
        let session_id = "permission-owner-session";
        let permission = AgentTimelineItem {
            id: "permission-request-1".to_string(),
            item_type: "permission".to_string(),
            role: Some("assistant".to_string()),
            title: Some("shell".to_string()),
            text: None,
            status: Some("pending".to_string()),
            input: None,
            output: None,
            created_ms: 1,
            merge: "replace".to_string(),
        };
        let live_timelines = Arc::new(Mutex::new(HashMap::from([(
            session_id.to_string(),
            test_live_timeline(
                AgentPermissionRouting::CallingSurface,
                LiveTimeline::Streaming(vec![permission]),
            ),
        )])));

        assert!(
            update_live_permission_status(
                &live_timelines,
                session_id,
                AgentPermissionRouting::Desktop,
                "request-1",
                "allow_once",
            )
            .await
            .is_none()
        );
        assert_eq!(
            update_live_permission_status(
                &live_timelines,
                session_id,
                AgentPermissionRouting::CallingSurface,
                "request-1",
                "allow_once",
            )
            .await
            .and_then(|item| item.status),
            Some("allow_once".to_string())
        );
    }

    #[test]
    fn calling_surface_terminal_cleanup_cannot_remove_desktop_live_state() {
        let session_id = "terminal-owner-session";
        let desktop_item = error_item("Desktop-only state".to_string());
        let mut timelines = HashMap::from([(
            session_id.to_string(),
            test_live_timeline(
                AgentPermissionRouting::Desktop,
                LiveTimeline::Streaming(vec![desktop_item]),
            ),
        )]);

        apply_successful_prompt_outcome(
            &mut timelines,
            session_id,
            AgentPermissionRouting::CallingSurface,
            &AgentPromptOutcome::default(),
        );
        assert_eq!(
            timelines.get(session_id).unwrap().routing,
            AgentPermissionRouting::Desktop
        );

        timelines.insert(
            session_id.to_string(),
            test_live_timeline(
                AgentPermissionRouting::CallingSurface,
                LiveTimeline::Streaming(Vec::new()),
            ),
        );
        apply_successful_prompt_outcome(
            &mut timelines,
            session_id,
            AgentPermissionRouting::CallingSurface,
            &AgentPromptOutcome::default(),
        );
        assert!(!timelines.contains_key(session_id));
    }

    #[tokio::test]
    async fn failed_prompt_outcome_keeps_only_the_latest_error() {
        let session_id = "failed-session";
        let prior_turn = message_to_timeline_items(
            &Message::user()
                .with_id("prior-user")
                .with_text("Prior turn"),
            false,
        );
        let mut timelines = HashMap::from([(
            session_id.to_string(),
            test_live_timeline(
                AgentPermissionRouting::Desktop,
                LiveTimeline::Streaming(prior_turn),
            ),
        )]);

        apply_failed_prompt_outcome(
            &mut timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            error_item("First failure".to_string()),
        );
        apply_failed_prompt_outcome(
            &mut timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            error_item("Second failure".to_string()),
        );

        let LiveTimeline::Failed(items) = &timelines.get(session_id).unwrap().timeline else {
            panic!("failed run should leave a bounded failed timeline");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text.as_deref(), Some("Second failure"));

        let live_timelines = Arc::new(Mutex::new(timelines));
        let next_user = message_to_timeline_items(
            &Message::user().with_id("next-user").with_text("Retry"),
            false,
        )
        .into_iter()
        .next()
        .unwrap();
        record_timeline_item(
            &live_timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            next_user,
        )
        .await;
        let timelines = live_timelines.lock().await;
        let LiveTimeline::Streaming(items) = &timelines.get(session_id).unwrap().timeline else {
            panic!("a retry should start a fresh streaming timeline");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "next-user-text");
    }

    fn write_test_file(path: &Path) {
        fs::create_dir_all(path.parent().expect("test file should have a parent"))
            .expect("test file parent should be created");
        fs::write(path, b"sentinel").expect("test file should be written");
    }

    fn assistant_tool_message(
        message_id: &str,
        tool_id: &str,
        thinking: &str,
        signature: &str,
    ) -> Message {
        Message::assistant()
            .with_id(message_id)
            .with_thinking(thinking, signature)
            .with_tool_request(
                tool_id,
                Ok(rmcp::model::CallToolRequestParams::new("shell")),
            )
    }

    fn with_usage(mut message: Message) -> Message {
        message.metadata.usage = Some(Box::default());
        message
    }

    fn assistant_redacted_tool_message(
        message_id: &str,
        tool_id: &str,
        redacted_data: &str,
    ) -> Message {
        Message::assistant()
            .with_id(message_id)
            .with_redacted_thinking(redacted_data)
            .with_tool_request(
                tool_id,
                Ok(rmcp::model::CallToolRequestParams::new("shell")),
            )
    }

    fn tool_response_message(message_id: &str, tool_id: &str) -> Message {
        Message::user().with_id(message_id).with_tool_response(
            tool_id,
            Ok(rmcp::model::CallToolResult::success(vec![
                rmcp::model::ContentBlock::text("ok"),
            ])),
        )
    }

    #[test]
    fn load_skill_timeline_card_uses_the_selected_skill_name() {
        let arguments = json!({"name": "release-maple"})
            .as_object()
            .unwrap()
            .clone();
        let request = Message::assistant()
            .with_id("skill-request")
            .with_tool_request(
                "functions.load_skill:1",
                Ok(rmcp::model::CallToolRequestParams::new("load_skill")
                    .with_arguments(arguments.clone())),
            );
        let response = Message::user()
            .with_id("skill-response")
            .with_tool_response(
                "functions.load_skill:1",
                Ok(rmcp::model::CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text("# Loaded Skill: release-maple"),
                ])),
            );

        let request_item = message_to_timeline_items(&request, false)
            .into_iter()
            .find(|item| item.item_type == "tool")
            .unwrap();
        assert_eq!(
            request_item.title.as_deref(),
            Some("Loading skill: release-maple")
        );
        assert_eq!(request_item.input, Some(Value::Object(arguments)));

        let merged = message_to_timeline_items(&response, false)
            .into_iter()
            .fold(vec![request_item], merge_timeline_item);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].title.as_deref(),
            Some("Loaded skill: release-maple")
        );
        assert_eq!(merged[0].status.as_deref(), Some("completed"));
        assert!(merged[0].input.is_some());
        assert!(merged[0].output.is_some());

        let failed_response = Message::user()
            .with_id("skill-failed-response")
            .with_tool_response(
                "functions.load_skill:1",
                Ok(rmcp::model::CallToolResult::error(vec![
                    rmcp::model::ContentBlock::text("Skill 'release-maple' not found"),
                ])),
            );
        let failed = message_to_timeline_items(&failed_response, false)
            .into_iter()
            .fold(
                message_to_timeline_items(&request, false),
                merge_timeline_item,
            );
        assert_eq!(failed.len(), 1);
        assert_eq!(
            failed[0].title.as_deref(),
            Some("Couldn’t load skill: release-maple")
        );
        assert_eq!(failed[0].status.as_deref(), Some("failed"));

        assert_eq!(
            skill_load_title("server__load_skill", &json!({"name": "not-a-maple-skill"})),
            None
        );
    }

    fn timeline_thinking_texts(items: &[AgentTimelineItem]) -> Vec<&str> {
        items
            .iter()
            .filter(|item| item.item_type == "thinking")
            .filter_map(|item| item.text.as_deref())
            .collect()
    }

    fn merge_test_timeline_items(
        mut current: Vec<AgentTimelineItem>,
        incoming: Vec<AgentTimelineItem>,
    ) -> Vec<AgentTimelineItem> {
        for item in incoming {
            current = merge_timeline_item(current, item);
        }
        current
    }

    #[test]
    fn joins_thinking_fragments_within_each_goose_message() {
        let message = Message::assistant()
            .with_id("assistant-1")
            .with_thinking("I can", "")
            .with_thinking(" help.", "")
            .with_text("Done");
        let conversation = Conversation::new_unvalidated(vec![message.clone()]);

        let live = message_to_timeline_items(&message, true);
        let loaded = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&live), vec!["I can help."]);
        assert_eq!(timeline_thinking_texts(&loaded), vec!["I can help."]);
        assert_eq!(
            loaded
                .iter()
                .find(|item| item.item_type == "thinking")
                .map(|item| item.id.as_str()),
            Some("assistant-1-thinking")
        );
    }

    #[test]
    fn stopped_marker_settles_only_unresolved_current_turn_permissions() {
        let resolved_permission = Message::assistant()
            .with_content(MessageContent::action_required(
                "resolved-tool",
                "shell".to_string(),
                serde_json::Map::new(),
                None,
            ))
            .with_generated_id();
        let unresolved_elicitation = Message::assistant()
            .with_content(MessageContent::action_required_elicitation(
                "pending-input",
                "Need more input".to_string(),
                json!({"type": "object"}),
            ))
            .with_generated_id();
        let stopped_notice = Message::assistant()
            .with_system_notification(SystemNotificationType::InlineMessage, "Stopped by user")
            .with_visibility(true, false)
            .with_generated_id();
        let conversation = Conversation::new_unvalidated(vec![
            Message::user().with_text("run tools").with_generated_id(),
            resolved_permission,
            tool_response_message("resolved-response", "resolved-tool"),
            unresolved_elicitation,
            stopped_notice,
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert!(items.iter().any(|item| {
            item.id == "permission-resolved-tool" && item.status.as_deref() == Some("completed")
        }));
        assert!(items.iter().any(|item| {
            item.id == "elicitation-pending-input" && item.status.as_deref() == Some("cancelled")
        }));
    }

    #[test]
    fn persisted_tool_permission_settles_without_stop_notice() {
        let permission = Message::assistant()
            .with_content(MessageContent::action_required(
                "resolved-tool",
                "shell".to_string(),
                serde_json::Map::new(),
                None,
            ))
            .with_generated_id();
        let conversation = Conversation::new_unvalidated(vec![
            Message::user().with_text("run tool").with_generated_id(),
            permission,
            tool_response_message("resolved-response", "resolved-tool"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert!(items.iter().any(|item| {
            item.id == "permission-resolved-tool" && item.status.as_deref() == Some("completed")
        }));
    }

    #[test]
    fn persisted_elicitation_settles_from_agent_only_response() {
        let request = Message::assistant()
            .with_content(MessageContent::action_required_elicitation(
                "resolved-input",
                "Need more input".to_string(),
                json!({"type": "object"}),
            ))
            .with_generated_id();
        let response = Message::user()
            .with_content(MessageContent::action_required_elicitation_response(
                "resolved-input",
                json!({"answer": "yes"}),
                rmcp::model::ElicitationAction::Accept,
            ))
            .agent_only()
            .with_generated_id();
        let conversation = Conversation::new_unvalidated(vec![
            Message::user().with_text("ask me").with_generated_id(),
            request,
            response,
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert!(items.iter().any(|item| {
            item.id == "elicitation-resolved-input" && item.status.as_deref() == Some("completed")
        }));
    }

    #[test]
    fn typed_provider_errors_render_without_exposing_confirmation_bookkeeping() {
        let message = Message::assistant()
            .with_id("assistant-error")
            .with_content(MessageContent::error(
                MessageErrorKind::ContextLengthExceeded,
                "The conversation is too large for this model.",
            ))
            .with_content(MessageContent::action_required_tool_confirmation_response(
                "tool-1",
                Permission::AllowOnce,
            ));

        let items = message_to_timeline_items(&message, false);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "assistant-error-error-0");
        assert_eq!(items[0].item_type, "error");
        assert_eq!(items[0].title.as_deref(), Some("Context limit exceeded"));
        assert_eq!(
            items[0].text.as_deref(),
            Some("The conversation is too large for this model.")
        );
        assert_eq!(items[0].status.as_deref(), Some("failed"));
    }

    #[test]
    fn desktop_permission_reconciliation_preserves_only_desktop_owned_pending_rows() {
        fn permission(id: &str, status: &str) -> AgentTimelineItem {
            AgentTimelineItem {
                id: format!("permission-{id}"),
                item_type: "permission".to_string(),
                role: Some("system".to_string()),
                title: Some("Permission".to_string()),
                text: None,
                status: Some(status.to_string()),
                input: None,
                output: None,
                created_ms: 1,
                merge: "replace".to_string(),
            }
        }

        let original_completed = permission("completed", "completed");
        let mut items = vec![
            permission("desktop", "pending"),
            permission("caller", "pending"),
            permission("orphan", "pending"),
            original_completed.clone(),
        ];
        let routes = HashMap::from([
            ("desktop".to_string(), AgentPermissionRouting::Desktop),
            ("caller".to_string(), AgentPermissionRouting::CallingSurface),
        ]);

        reconcile_desktop_permission_items(&mut items, &routes, false);

        assert_eq!(items[0].status.as_deref(), Some("pending"));
        assert_eq!(items[1].status.as_deref(), Some("controlled_externally"));
        assert_eq!(items[2].status.as_deref(), Some("cancelled"));
        assert_eq!(items[3], original_completed);

        let mut registration_race = vec![permission("not-registered-yet", "pending")];
        reconcile_desktop_permission_items(&mut registration_race, &HashMap::new(), true);
        assert_eq!(
            registration_race[0].status.as_deref(),
            Some("controlled_externally")
        );
    }

    #[test]
    fn hides_tool_reasoning_after_prior_visible_thinking() {
        let surfaced = "Inspect the project before running both commands.";
        let tool_attached = "Reasoning accumulated before the tool request.";
        let conversation = Conversation::new_unvalidated(vec![
            Message::assistant()
                .with_id("surfaced")
                .with_thinking(surfaced, "")
                .with_text("Starting now."),
            assistant_tool_message("request-1", "tool-1", tool_attached, ""),
            tool_response_message("response-1", "tool-1"),
            with_usage(assistant_tool_message(
                "request-2",
                "tool-2",
                tool_attached,
                "",
            )),
            tool_response_message("response-2", "tool-2"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&items), vec![surfaced]);
        assert_eq!(
            items.iter().filter(|item| item.item_type == "tool").count(),
            2
        );
    }

    #[test]
    fn suppresses_replayed_thinking_on_split_tool_requests() {
        let reasoning = "Run both requested commands.";
        let conversation = Conversation::new_unvalidated(vec![
            assistant_tool_message("request-1", "tool-1", reasoning, ""),
            tool_response_message("response-1", "tool-1"),
            with_usage(assistant_tool_message(
                "request-2",
                "tool-2",
                "A later accumulated copy from the same inference.",
                "",
            )),
            tool_response_message("response-2", "tool-2"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&items), vec![reasoning]);
        assert_eq!(
            items.iter().filter(|item| item.item_type == "tool").count(),
            2
        );
    }

    #[test]
    fn usage_boundary_preserves_identical_thinking_in_the_next_inference() {
        let reasoning = "Run the requested command.";
        let conversation = Conversation::new_unvalidated(vec![
            with_usage(assistant_tool_message("request-1", "tool-1", reasoning, "")),
            tool_response_message("response-1", "tool-1"),
            with_usage(assistant_tool_message("request-2", "tool-2", reasoning, "")),
            tool_response_message("response-2", "tool-2"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&items), vec![reasoning, reasoning]);
    }

    #[test]
    fn histories_without_usage_preserve_every_tool_thought() {
        let conversation = Conversation::new_unvalidated(vec![
            assistant_tool_message("request-1", "tool-1", "First thought.", ""),
            tool_response_message("response-1", "tool-1"),
            assistant_tool_message("request-2", "tool-2", "Second thought.", ""),
            tool_response_message("response-2", "tool-2"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(
            timeline_thinking_texts(&items),
            vec!["First thought.", "Second thought."]
        );
    }

    #[test]
    fn preserves_legacy_thinking_text_for_the_rendering_boundary() {
        let reasoning = "Inspect the repository and summarize it.";
        let conversation = Conversation::new_unvalidated(vec![
            Message::assistant()
                .with_id("standalone-reasoning")
                .with_thinking(reasoning, ""),
            Message::assistant()
                .with_id("standalone-period")
                .with_thinking(".", ""),
            assistant_tool_message("request-1", "tool-1", ".", ""),
            tool_response_message("response-1", "tool-1"),
            with_usage(assistant_tool_message("request-2", "tool-2", ".", "")),
            tool_response_message("response-2", "tool-2"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&items), vec![reasoning, "."]);
        assert_eq!(
            items.iter().filter(|item| item.item_type == "tool").count(),
            2
        );
    }

    #[test]
    fn live_thinking_chunks_match_persisted_message_projection() {
        let user = Message::user()
            .with_id("current-user")
            .with_text("Inspect the project.");
        let persisted_conversation = Conversation::new_unvalidated(vec![
            user.clone(),
            Message::assistant()
                .with_id("assistant")
                .with_thinking(". ", "")
                .with_thinking("First", "")
                .with_thinking(" ", "")
                .with_thinking("second", "")
                .with_thinking(".", ""),
        ]);
        let persisted = conversation_to_timeline_items(&persisted_conversation);
        let live_messages = vec![
            user,
            Message::assistant()
                .with_id("live-assistant")
                .with_thinking(". ", ""),
            Message::assistant()
                .with_id("live-assistant")
                .with_thinking("First", ""),
            Message::assistant()
                .with_id("live-assistant")
                .with_thinking(" ", ""),
            Message::assistant()
                .with_id("live-assistant")
                .with_thinking("second", ""),
            Message::assistant()
                .with_id("live-assistant")
                .with_thinking(".", ""),
        ];
        let live = live_messages
            .into_iter()
            .fold(Vec::new(), |items, message| {
                merge_test_timeline_items(items, message_to_timeline_items(&message, true))
            });

        assert_eq!(timeline_thinking_texts(&persisted), vec![". First second."]);
        assert_eq!(timeline_thinking_texts(&live), vec![". First second."]);
    }

    #[test]
    fn suppresses_signed_thinking_replayed_within_one_inference() {
        let conversation = Conversation::new_unvalidated(vec![
            assistant_tool_message("request-1", "tool-1", "Signed reasoning", "signature-a"),
            tool_response_message("response-1", "tool-1"),
            with_usage(assistant_tool_message(
                "request-2",
                "tool-2",
                "Signed reasoning",
                "signature-b",
            )),
            tool_response_message("response-2", "tool-2"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&items), vec!["Signed reasoning"]);
    }

    #[test]
    fn suppresses_redacted_thinking_replayed_within_one_inference() {
        let conversation = Conversation::new_unvalidated(vec![
            assistant_redacted_tool_message("request-1", "tool-1", "opaque-payload-a"),
            tool_response_message("response-1", "tool-1"),
            with_usage(assistant_redacted_tool_message(
                "request-2",
                "tool-2",
                "opaque-payload-b",
            )),
            tool_response_message("response-2", "tool-2"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(
            timeline_thinking_texts(&items),
            vec!["Thinking redacted by provider."]
        );
    }

    #[test]
    fn preserves_reasoning_text_for_the_rendering_boundary() {
        let conversation = Conversation::new_unvalidated(vec![
            Message::assistant()
                .with_id("emoji")
                .with_thinking("🤔", ""),
            Message::assistant()
                .with_id("operator")
                .with_thinking("=>", ""),
            Message::assistant()
                .with_id("ellipsis")
                .with_thinking("…...", ""),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&items), vec!["🤔", "=>", "…..."]);
    }

    #[test]
    fn unsigned_thinking_dedupe_resets_on_the_next_user_turn() {
        let reasoning = "Run the requested command.";
        let conversation = Conversation::new_unvalidated(vec![
            with_usage(assistant_tool_message("request-1", "tool-1", reasoning, "")),
            tool_response_message("response-1", "tool-1"),
            Message::user()
                .with_id("next-turn")
                .with_text("Run it again."),
            with_usage(assistant_tool_message("request-2", "tool-2", reasoning, "")),
            tool_response_message("response-2", "tool-2"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&items), vec![reasoning, reasoning]);
    }

    #[test]
    fn hidden_goose_messages_neither_render_nor_consume_visible_replay() {
        let reasoning = "Inspect the project.";
        let hidden = Message::assistant()
            .with_id("hidden-assistant")
            .with_thinking(reasoning, "")
            .with_text("internal grind details")
            .with_visibility(false, true);
        let conversation = Conversation::new_unvalidated(vec![
            Message::user().with_id("user").with_text("Inspect it."),
            hidden.clone(),
            with_usage(assistant_tool_message(
                "visible-request",
                "tool-1",
                reasoning,
                "",
            )),
            tool_response_message("response", "tool-1"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&items), vec![reasoning]);
        assert!(!items.iter().any(|item| {
            item.id.starts_with("hidden-assistant")
                || item.text.as_deref() == Some("internal grind details")
        }));
        assert!(message_to_timeline_items(&hidden, true).is_empty());
    }

    #[test]
    fn persisted_timeline_enforces_content_audience_boundaries() {
        let audience_text = |text: &str, audience| {
            MessageContent::Text(
                TextContent::new(text)
                    .with_annotations(Annotations::default().with_audience(vec![audience])),
            )
        };

        let mixed_text = Message::assistant()
            .with_id("mixed-text")
            .with_text("visible response")
            .with_content(audience_text("provider-private-state", McpRole::Assistant))
            .with_content(audience_text(" plus visible detail", McpRole::User));
        let persisted_items = conversation_to_timeline_items(&Conversation::new_unvalidated(vec![
            mixed_text.clone(),
        ]));
        let live_items = message_to_timeline_items(&mixed_text.user_visible_content(), true);
        assert_eq!(persisted_items.len(), 1);
        assert_eq!(
            persisted_items[0].text.as_deref(),
            Some("visible response plus visible detail")
        );
        assert!(
            !persisted_items[0]
                .text
                .as_deref()
                .unwrap()
                .contains("provider-private-state")
        );
        assert!(timeline_projection_matches(
            &live_items,
            &persisted_items,
            true
        ));

        let assistant_only = Message::assistant()
            .with_id("assistant-only")
            .with_content(audience_text("provider-private-state", McpRole::Assistant));
        assert!(message_to_timeline_items(&assistant_only, false).is_empty());

        let mixed_tool_result = Message::user().with_tool_response(
            "mixed-tool",
            Ok(rmcp::model::CallToolResult::success(vec![
                rmcp::model::ContentBlock::Text(
                    TextContent::new("visible tool output").with_annotations(
                        Annotations::default().with_audience(vec![McpRole::User]),
                    ),
                ),
                rmcp::model::ContentBlock::Text(
                    TextContent::new("provider-private-tool-state").with_annotations(
                        Annotations::default().with_audience(vec![McpRole::Assistant]),
                    ),
                ),
            ])),
        );
        let tool_items = message_to_timeline_items(&mixed_tool_result, false);
        assert_eq!(tool_items.len(), 1);
        let output = tool_items[0].output.as_ref().unwrap();
        assert_eq!(output["text"], "visible tool output");
        assert_eq!(output["content"].as_array().unwrap().len(), 1);
        assert!(!output.to_string().contains("provider-private-tool-state"));
    }

    #[test]
    fn hidden_usage_boundary_resets_visible_inference_state() {
        let first = "First visible thought.";
        let second = "Second visible thought.";
        let hidden_boundary = with_usage(
            Message::assistant()
                .with_id("hidden-boundary")
                .with_text("internal")
                .with_visibility(false, true),
        );
        let conversation = Conversation::new_unvalidated(vec![
            Message::assistant()
                .with_id("first")
                .with_thinking(first, ""),
            hidden_boundary,
            with_usage(assistant_tool_message("request", "tool", second, "")),
            tool_response_message("response", "tool"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&items), vec![first, second]);
    }

    #[test]
    fn hidden_user_message_still_resets_provider_turn_replay() {
        let reasoning = "Run the requested command.";
        let conversation = Conversation::new_unvalidated(vec![
            with_usage(assistant_tool_message("request-1", "tool-1", reasoning, "")),
            tool_response_message("response-1", "tool-1"),
            Message::user()
                .with_id("hidden-user")
                .with_text("internal retry turn")
                .with_visibility(false, true),
            with_usage(assistant_tool_message("request-2", "tool-2", reasoning, "")),
            tool_response_message("response-2", "tool-2"),
        ]);

        let items = conversation_to_timeline_items(&conversation);

        assert_eq!(timeline_thinking_texts(&items), vec![reasoning, reasoning]);
        assert!(!items.iter().any(|item| item.id.starts_with("hidden-user")));
    }

    #[test]
    fn thinking_projection_is_session_local_and_does_not_mutate_history() {
        let build_conversation = || {
            Conversation::new_unvalidated(vec![
                assistant_tool_message("request-1", "tool-1", "Shared replay", ""),
                tool_response_message("response-1", "tool-1"),
                with_usage(assistant_tool_message(
                    "request-2",
                    "tool-2",
                    "Shared replay",
                    "",
                )),
                tool_response_message("response-2", "tool-2"),
            ])
        };
        let first = build_conversation();
        let second = build_conversation();
        let first_before = first.clone();
        let second_before = second.clone();

        let first_items = conversation_to_timeline_items(&first);
        let second_items = conversation_to_timeline_items(&second);

        assert_eq!(timeline_thinking_texts(&first_items), vec!["Shared replay"]);
        assert_eq!(
            timeline_thinking_texts(&second_items),
            vec!["Shared replay"]
        );
        assert_eq!(first, first_before);
        assert_eq!(second, second_before);
    }

    #[test]
    fn live_overlay_splices_at_the_first_shared_user_boundary() {
        let prior_user = Message::user()
            .with_id("prior-user")
            .with_text("Earlier turn");
        let prior_assistant = Message::assistant()
            .with_id("prior-assistant")
            .with_text("Earlier answer");
        let current_user = Message::user()
            .with_id("current-user")
            .with_text("Current turn");
        let persisted_thought = Message::assistant()
            .with_id("persisted-copy")
            .with_thinking("Persisted provider-history copy", "");
        let live_thought = Message::assistant()
            .with_id("live-thought")
            .with_thinking("Authoritative live thought", "");

        let persisted = [
            message_to_timeline_items(&prior_user, false),
            message_to_timeline_items(&prior_assistant, false),
            message_to_timeline_items(&current_user, false),
            message_to_timeline_items(&persisted_thought, false),
        ]
        .concat();
        let live = [
            message_to_timeline_items(&current_user, true),
            message_to_timeline_items(&live_thought, true),
        ]
        .concat();

        let overlaid = overlay_live_timeline_items(persisted, live);

        assert!(overlaid.iter().any(|item| item.id == "prior-user-text"));
        assert!(
            overlaid
                .iter()
                .any(|item| item.id == "prior-assistant-text")
        );
        assert!(overlaid.iter().any(|item| item.id == "current-user-text"));
        assert!(
            overlaid
                .iter()
                .any(|item| item.id == "live-thought-thinking")
        );
        assert!(
            !overlaid
                .iter()
                .any(|item| item.id == "persisted-copy-thinking")
        );
    }

    #[tokio::test]
    async fn history_replaced_preserves_the_matching_live_user_boundary() {
        let session_id = "history-replaced-boundary";
        let prior_user = Message::user()
            .with_id("prior-user")
            .with_text("Earlier turn");
        let current_user = Message::user()
            .with_id("current-user")
            .with_text("Current turn");
        let hidden_user = Message::user()
            .with_id("hidden-user")
            .with_text("Internal retry turn")
            .with_visibility(false, true);
        let conversation = Conversation::new_unvalidated(vec![
            prior_user,
            current_user,
            tool_response_message("tool-response", "tool-1"),
            hidden_user,
        ]);
        let stale = message_to_timeline_items(
            &Message::assistant()
                .with_id("stale-live")
                .with_thinking("Stale thought", ""),
            true,
        );
        let mut live = stale;
        live.extend(message_to_timeline_items(
            &Message::user()
                .with_id("current-user")
                .with_text("Live current turn"),
            false,
        ));
        let live_timelines = Arc::new(Mutex::new(HashMap::from([(
            session_id.to_string(),
            test_live_timeline(
                AgentPermissionRouting::Desktop,
                LiveTimeline::Streaming(live),
            ),
        )])));

        reseed_live_timeline_after_history_replaced(
            &live_timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            &conversation,
        )
        .await;

        let timelines = live_timelines.lock().await;
        let items = timelines
            .get(session_id)
            .expect("replacement should retain a user boundary")
            .timeline
            .items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "current-user-text");
        assert_eq!(items[0].text.as_deref(), Some("Live current turn"));
        assert!(!items.iter().any(|item| item.id == "stale-live-thinking"));
        assert!(!items.iter().any(|item| item.id == "hidden-user-text"));
    }

    #[tokio::test]
    async fn history_replaced_ignores_user_rows_emptied_by_audience_projection() {
        let session_id = "history-replaced-audience-boundary";
        let current_user = Message::user()
            .with_id("current-user")
            .with_text("Current turn");
        let provider_only_user =
            Message::user()
                .with_id("provider-only-user")
                .with_content(MessageContent::Text(
                    TextContent::new("provider-private-state").with_annotations(
                        Annotations::default().with_audience(vec![McpRole::Assistant]),
                    ),
                ));
        let conversation =
            Conversation::new_unvalidated(vec![current_user.clone(), provider_only_user]);
        let live_timelines = Arc::new(Mutex::new(HashMap::from([(
            session_id.to_string(),
            test_live_timeline(
                AgentPermissionRouting::Desktop,
                LiveTimeline::Streaming(message_to_timeline_items(&current_user, false)),
            ),
        )])));

        reseed_live_timeline_after_history_replaced(
            &live_timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            &conversation,
        )
        .await;

        let timelines = live_timelines.lock().await;
        let items = timelines
            .get(session_id)
            .expect("the latest visible user boundary should survive")
            .timeline
            .items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "current-user-text");
        assert!(
            !items[0]
                .text
                .as_deref()
                .unwrap_or_default()
                .contains("provider-private-state")
        );
    }

    #[tokio::test]
    async fn history_replaced_boundary_prevents_post_compaction_replay_on_reload() {
        let session_id = "post-compaction-reload";
        let prior_user = Message::user()
            .with_id("prior-user")
            .with_text("Earlier turn");
        let prior_assistant = Message::assistant()
            .with_id("prior-assistant")
            .with_text("Earlier answer");
        let current_user = Message::user()
            .with_id("current-user")
            .with_text("Current turn");
        let replacement = Conversation::new_unvalidated(vec![
            prior_user.clone(),
            prior_assistant.clone(),
            current_user.clone(),
        ]);
        let live_timelines = Arc::new(Mutex::new(HashMap::new()));
        reseed_live_timeline_after_history_replaced(
            &live_timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            &replacement,
        )
        .await;

        let live_response = assistant_tool_message(
            "live-provider-response",
            "tool-1",
            "Authoritative live thought",
            "",
        );
        for item in message_to_timeline_items(&live_response, true) {
            record_timeline_item(
                &live_timelines,
                session_id,
                AgentPermissionRouting::Desktop,
                item,
            )
            .await;
        }

        let persisted_conversation = Conversation::new_unvalidated(vec![
            prior_user,
            prior_assistant,
            current_user,
            with_usage(assistant_tool_message(
                "persisted-split-request",
                "tool-1",
                "Persisted provider-history copy",
                "",
            )),
            tool_response_message("persisted-tool-response", "tool-1"),
        ]);
        let persisted = conversation_to_timeline_items(&persisted_conversation);
        assert_eq!(
            timeline_thinking_texts(&persisted),
            vec!["Persisted provider-history copy"]
        );

        let overlaid = overlay_live_timeline(
            &live_timelines,
            session_id,
            AgentPermissionRouting::Desktop,
            &persisted_conversation,
            persisted,
        )
        .await;

        assert!(overlaid.iter().any(|item| item.id == "prior-user-text"));
        assert!(
            overlaid
                .iter()
                .any(|item| item.id == "prior-assistant-text")
        );
        assert_eq!(
            overlaid
                .iter()
                .filter(|item| item.id == "current-user-text")
                .count(),
            1
        );
        assert_eq!(
            timeline_thinking_texts(&overlaid),
            vec!["Authoritative live thought"]
        );
        assert!(
            overlaid
                .iter()
                .any(|item| item.id == "live-provider-response-thinking")
        );
        assert!(
            !overlaid
                .iter()
                .any(|item| item.id == "persisted-split-request-thinking")
        );
        assert_eq!(
            overlaid
                .iter()
                .filter(|item| item.item_type == "tool" && item.id == "tool-1")
                .count(),
            1
        );
    }

    #[test]
    fn clear_history_removes_only_the_target_account_session_store() {
        let test_root = std::env::temp_dir().join(format!(
            "maple-agent-history-clear-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let app_config_dir = test_root.join("app-config");
        let agent_root = app_config_dir.join("agent");
        let account_dir = agent_root.join("accounts/target");
        let other_account_dir = agent_root.join("accounts/other");
        let removed = [account_dir.join("goose/data/session.db")];
        for path in &removed {
            write_test_file(path);
        }

        let preserved = [
            account_dir.join("config.json"),
            account_dir.join("recent_roots.json"),
            account_dir.join("goose/config/permissions.json"),
            other_account_dir.join("goose/data/session.db"),
            agent_root.join("goose-runtime/config/config.yaml"),
            app_config_dir.join("proxy_config.json"),
        ];
        for path in &preserved {
            write_test_file(path);
        }

        clear_agent_history(&account_dir).expect("Agent history should be cleared");

        for path in removed {
            assert!(!path.exists(), "history remained at {}", path.display());
        }
        for path in preserved {
            assert!(path.exists(), "configuration removed at {}", path.display());
        }

        clear_agent_history(&account_dir).expect("clearing missing history should be idempotent");
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn offline_session_managers_reopen_only_their_account_data() {
        let test_root = std::env::temp_dir().join(format!(
            "maple-agent-offline-sessions-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let project_dir = test_root.join("project");
        let account_a = test_root.join("accounts/a");
        let account_b = test_root.join("accounts/b");
        fs::create_dir_all(&project_dir).expect("project directory should be created");

        let manager_a = session_manager_for_account_dir(&account_a)
            .expect("account A session manager should open");
        let manager_b = session_manager_for_account_dir(&account_b)
            .expect("account B session manager should open");
        let session_a = manager_a
            .create_session(
                project_dir.clone(),
                "Account A chat".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .expect("account A session should be created");
        let session_b = manager_b
            .create_session(
                project_dir.clone(),
                "Account B chat".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .expect("account B session should be created");
        let account_b_only_session = manager_b
            .create_session(
                project_dir,
                "Account B second chat".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .expect("account B second session should be created");
        drop(manager_a);
        drop(manager_b);

        let reopened_a = session_manager_for_account_dir(&account_a)
            .expect("account A session manager should reopen");
        let reopened_b = session_manager_for_account_dir(&account_b)
            .expect("account B session manager should reopen");
        let loaded_a = reopened_a
            .get_session(&session_a.id, true)
            .await
            .expect("account A session should reload");
        let loaded_b = reopened_b
            .get_session(&session_b.id, true)
            .await
            .expect("account B session should reload");
        assert_eq!(loaded_a.name, "Account A chat");
        assert_eq!(loaded_b.name, "Account B chat");
        assert!(
            reopened_a
                .get_session(&account_b_only_session.id, true)
                .await
                .is_err()
        );

        reopened_a
            .delete_session(&session_a.id)
            .await
            .expect("account A session should be deleted");
        assert!(reopened_a.list_all_sessions().await.unwrap().is_empty());
        assert_eq!(reopened_b.list_all_sessions().await.unwrap().len(), 2);

        drop(reopened_a);
        drop(reopened_b);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn deletes_only_target_session_runtime_state() {
        let test_root = std::env::temp_dir().join(format!(
            "maple-agent-session-delete-flow-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let data_dir = test_root.join("goose-data");
        let project_dir = test_root.join("project");
        fs::create_dir_all(&data_dir).expect("Goose data directory should be created");
        fs::create_dir_all(&project_dir).expect("project directory should be created");

        let session_manager = SessionManager::new(data_dir);
        let target = session_manager
            .create_session(
                project_dir.clone(),
                "Target chat".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .expect("target session should be created");
        let survivor = session_manager
            .create_session(
                project_dir,
                "Surviving chat".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .expect("surviving session should be created");

        let live_timelines = Arc::new(Mutex::new(HashMap::from([
            (
                target.id.clone(),
                test_live_timeline(
                    AgentPermissionRouting::Desktop,
                    LiveTimeline::Streaming(Vec::new()),
                ),
            ),
            (
                survivor.id.clone(),
                test_live_timeline(
                    AgentPermissionRouting::Desktop,
                    LiveTimeline::Streaming(Vec::new()),
                ),
            ),
        ])));
        let pending_permissions = Arc::new(Mutex::new(HashMap::from([
            (
                (target.id.clone(), "target-request".to_string()),
                test_pending_permission(
                    "target-run",
                    AgentPermissionRouting::Desktop,
                    "target-request",
                ),
            ),
            (
                (survivor.id.clone(), "survivor-request".to_string()),
                test_pending_permission(
                    "survivor-run",
                    AgentPermissionRouting::Desktop,
                    "survivor-request",
                ),
            ),
        ])));
        let web_tool_state = WebToolState::default();
        let provenance_cancel = CancellationToken::new();
        web_tool_state
            .record_search_urls(
                &target.id,
                ["https://example.com/target"],
                &provenance_cancel,
            )
            .await;
        web_tool_state
            .record_search_urls(
                &survivor.id,
                ["https://example.com/survivor"],
                &provenance_cancel,
            )
            .await;
        store_session_system_prompt(&target.id, Some("target persona".to_string()));
        store_session_system_prompt(&survivor.id, Some("survivor persona".to_string()));
        delete_persisted_agent_session(
            &session_manager,
            &pending_permissions,
            &live_timelines,
            &Arc::new(Mutex::new(HashMap::new())),
            Some(&web_tool_state),
            &target.id,
        )
        .await
        .expect("target session deletion should succeed");

        assert!(
            session_manager
                .get_session(&target.id, false)
                .await
                .is_err()
        );
        assert_eq!(session_system_prompt(&target.id), None);
        assert_eq!(
            session_system_prompt(&survivor.id).as_deref(),
            Some("survivor persona")
        );
        store_session_system_prompt(&survivor.id, None);
        assert!(
            session_manager
                .get_session(&survivor.id, false)
                .await
                .is_ok()
        );
        assert!(!live_timelines.lock().await.contains_key(&target.id));
        assert!(live_timelines.lock().await.contains_key(&survivor.id));
        let permissions = pending_permissions.lock().await;
        assert!(
            !permissions
                .keys()
                .any(|(session_id, _)| session_id == &target.id)
        );
        assert!(
            permissions
                .keys()
                .any(|(session_id, _)| session_id == &survivor.id)
        );
        drop(permissions);
        assert!(
            !web_tool_state
                .contains_search_url(&target.id, "https://example.com/target")
                .await
        );
        assert!(
            web_tool_state
                .contains_search_url(&survivor.id, "https://example.com/survivor")
                .await
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn stopped_turn_keeps_goose_history_and_discards_speculative_live_state() {
        let test_root = std::env::temp_dir().join(format!(
            "maple-agent-stopped-turn-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let data_dir = test_root.join("goose-data");
        let project_dir = test_root.join("project");
        fs::create_dir_all(&data_dir).expect("Goose data directory should be created");
        fs::create_dir_all(&project_dir).expect("project directory should be created");

        let session_manager = SessionManager::new(data_dir);
        let session = session_manager
            .create_session(
                project_dir.clone(),
                "Retained cancellation".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .expect("test session should be created");
        let prior_user = Message::user()
            .with_text("keep prior prompt")
            .with_generated_id();
        let prior_assistant = Message::assistant()
            .with_text("keep prior response")
            .with_generated_id();
        let stopped_user = Message::user()
            .with_text("keep this stopped prompt")
            .with_generated_id();
        let completed_tool_request =
            assistant_tool_message("completed-request", "completed-tool", "", "");
        let completed_tool_response = tool_response_message("completed-response", "completed-tool");
        let declined_tool_request =
            assistant_tool_message("declined-request", "declined-tool", "", "");
        let declined_tool_response = Message::user()
            .with_id("declined-response")
            .with_tool_response(
                "declined-tool",
                Ok(rmcp::model::CallToolResult::error(vec![
                    rmcp::model::ContentBlock::text(
                        "The user has declined to run this tool. DO NOT attempt again.",
                    ),
                ])),
            );
        let cancelled_tool_request =
            assistant_tool_message("cancelled-request", "cancelled-tool", "", "");
        let cancelled_tool_placeholder = Message::user().with_generated_id();
        let pending_elicitation = Message::assistant()
            .with_content(MessageContent::action_required_elicitation(
                "stopped-input",
                "Need more input".to_string(),
                json!({"type": "object"}),
            ))
            .with_generated_id();
        for message in [
            &prior_user,
            &prior_assistant,
            &stopped_user,
            &completed_tool_request,
            &completed_tool_response,
            &declined_tool_request,
            &declined_tool_response,
            &cancelled_tool_request,
            &cancelled_tool_placeholder,
            &pending_elicitation,
        ] {
            session_manager
                .add_message(&session.id, message)
                .await
                .expect("Goose history should be persisted");
        }

        let web_tool_state = WebToolState::default();
        let provenance_cancel = CancellationToken::new();
        web_tool_state
            .record_search_urls(
                &session.id,
                ["https://example.com/completed-search"],
                &provenance_cancel,
            )
            .await;

        let live_timelines = Arc::new(Mutex::new(HashMap::from([(
            session.id.clone(),
            test_live_timeline(
                AgentPermissionRouting::Desktop,
                LiveTimeline::Streaming(vec![error_item("speculative partial event".to_string())]),
            ),
        )])));
        finalize_cancelled_agent_turn(
            &session_manager,
            &live_timelines,
            &web_tool_state,
            &session.id,
            AgentPermissionRouting::Desktop,
            &stopped_user,
            &HashSet::from(["declined-tool".to_string()]),
        )
        .await
        .expect("stopped turn should settle");

        let reloaded = session_manager
            .get_session(&session.id, true)
            .await
            .expect("stopped session should reload");
        let conversation = reloaded
            .conversation
            .as_ref()
            .expect("stopped session should have a conversation");
        assert_eq!(&conversation.messages()[..2], [prior_user, prior_assistant]);
        assert_eq!(
            &conversation.messages()[2..5],
            [
                stopped_user,
                completed_tool_request,
                completed_tool_response,
            ]
        );
        assert_eq!(reloaded.name, "Retained cancellation");
        assert_eq!(conversation.len(), 7);

        let stopped_notice = conversation
            .last()
            .expect("stopped notice should be stored");
        assert!(stopped_notice.is_user_visible());
        assert!(!stopped_notice.is_agent_visible());
        assert!(matches!(
            stopped_notice.content.as_slice(),
            [MessageContent::SystemNotification(notification)]
                if notification.msg == "Stopped by user"
        ));
        assert!(!live_timelines.lock().await.contains_key(&session.id));
        assert!(
            !web_tool_state
                .contains_search_url(&session.id, "https://example.com/completed-search")
                .await
        );

        let timeline = conversation_to_timeline_items(conversation);
        assert!(timeline.iter().any(|item| {
            item.id == "completed-tool" && item.status.as_deref() == Some("completed")
        }));
        assert!(!conversation.messages().iter().any(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, MessageContent::ToolRequest(request) if request.id == "cancelled-tool"))
        }));
        assert!(!conversation.messages().iter().any(|message| {
            message.content.iter().any(|content| {
                matches!(content, MessageContent::ToolRequest(request) if request.id == "declined-tool")
                    || matches!(content, MessageContent::ToolResponse(response) if response.id == "declined-tool")
            })
        }));
        assert!(!timeline.iter().any(|item| item.id == "cancelled-tool"));
        assert!(!timeline.iter().any(|item| item.id == "declined-tool"));
        assert!(timeline.iter().any(|item| {
            item.item_type == "system" && item.text.as_deref() == Some("Stopped by user")
        }));
        assert!(timeline.iter().any(|item| {
            item.id == "elicitation-stopped-input" && item.status.as_deref() == Some("cancelled")
        }));
        assert!(
            !timeline
                .iter()
                .any(|item| { item.text.as_deref() == Some("speculative partial event") })
        );

        let first_turn_session = session_manager
            .create_session(
                project_dir,
                "Retained first prompt".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .expect("first-turn session should be created");
        let first_turn_user = Message::user()
            .with_text("stop before Goose starts")
            .with_generated_id();
        live_timelines.lock().await.insert(
            first_turn_session.id.clone(),
            test_live_timeline(
                AgentPermissionRouting::Desktop,
                LiveTimeline::Streaming(vec![error_item("optimistic first turn".to_string())]),
            ),
        );
        finalize_cancelled_agent_turn(
            &session_manager,
            &live_timelines,
            &web_tool_state,
            &first_turn_session.id,
            AgentPermissionRouting::Desktop,
            &first_turn_user,
            &HashSet::new(),
        )
        .await
        .expect("first prompt should be retained");

        let first_turn_reloaded = session_manager
            .get_session(&first_turn_session.id, true)
            .await
            .expect("first-turn session should reload");
        let first_turn_conversation = first_turn_reloaded
            .conversation
            .expect("first-turn conversation should be stored");
        assert_eq!(first_turn_reloaded.name, "Retained first prompt");
        assert_eq!(first_turn_conversation.len(), 2);
        assert_eq!(first_turn_conversation.first(), Some(&first_turn_user));
        assert!(
            first_turn_conversation
                .last()
                .is_some_and(|message| !message.is_agent_visible())
        );
        assert!(
            !live_timelines
                .lock()
                .await
                .contains_key(&first_turn_session.id)
        );

        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn run_event_streams_are_ordered_isolated_and_host_policy_controls_projection() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let dispatcher = AgentEventDispatcher::new(sink.clone());
        let (first, mut first_events) = AgentRunEventPublisher::new(
            dispatcher.clone(),
            "session-1".to_string(),
            "run-1".to_string(),
            AgentHostEventPolicy::Publish,
        );
        let (second, mut second_events) = AgentRunEventPublisher::new(
            dispatcher.clone(),
            "session-2".to_string(),
            "run-2".to_string(),
            AgentHostEventPolicy::Publish,
        );
        let (external, mut external_events) = AgentRunEventPublisher::new(
            dispatcher,
            "session-3".to_string(),
            "run-3".to_string(),
            AgentHostEventPolicy::Suppress,
        );

        first.publish(AgentRunEvent::Started).await;
        second
            .publish(AgentRunEvent::SetupWarning("setup warning".to_string()))
            .await;
        first
            .publish(AgentRunEvent::Finished(AgentRunTerminal::Completed))
            .await;
        second
            .publish(AgentRunEvent::Finished(AgentRunTerminal::Failed))
            .await;
        external.publish(AgentRunEvent::Started).await;

        assert!(matches!(
            first_events.recv().await,
            Some(AgentRunEvent::Started)
        ));
        assert!(matches!(
            first_events.recv().await,
            Some(AgentRunEvent::Finished(AgentRunTerminal::Completed))
        ));
        assert!(first_events.try_recv().is_err());

        assert!(matches!(
            second_events.recv().await,
            Some(AgentRunEvent::SetupWarning(message)) if message == "setup warning"
        ));
        assert!(matches!(
            second_events.recv().await,
            Some(AgentRunEvent::Finished(AgentRunTerminal::Failed))
        ));
        assert!(second_events.try_recv().is_err());
        assert!(matches!(
            external_events.recv().await,
            Some(AgentRunEvent::Started)
        ));

        let emitted = sink
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(emitted.len(), 4);
        assert!(matches!(
            &emitted[0],
            AgentServiceEvent::Run { session_id, run_id, event: AgentRunEvent::Started }
                if session_id == "session-1" && run_id == "run-1"
        ));
        assert!(matches!(
            &emitted[1],
            AgentServiceEvent::Run {
                session_id,
                run_id,
                event: AgentRunEvent::SetupWarning(message),
            } if session_id == "session-2" && run_id == "run-2" && message == "setup warning"
        ));
        assert!(matches!(
            &emitted[2],
            AgentServiceEvent::Run {
                event: AgentRunEvent::Finished(AgentRunTerminal::Completed),
                ..
            }
        ));
        assert!(matches!(
            &emitted[3],
            AgentServiceEvent::Run {
                event: AgentRunEvent::Finished(AgentRunTerminal::Failed),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn lagged_run_event_consumer_never_backpressures_the_agent() {
        let (publisher, events) = AgentRunEventPublisher::new(
            AgentEventDispatcher::new(Arc::new(NoopAgentEventSink)),
            "session-1".to_string(),
            "run-1".to_string(),
            AgentHostEventPolicy::Suppress,
        );
        let overflowed = publisher.overflow_flag();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            for _ in 0..(AGENT_RUN_EVENT_CAPACITY + 32) {
                publisher.publish(AgentRunEvent::Started).await;
            }
            publisher
                .publish(AgentRunEvent::Finished(AgentRunTerminal::Completed))
                .await;
        })
        .await
        .expect("a full protocol queue must not block the Agent run");

        assert_eq!(events.len(), AGENT_RUN_EVENT_CAPACITY);
        assert!(overflowed.load(Ordering::Acquire));
    }

    #[test]
    fn stale_tool_context_identity_cannot_remove_its_replacement() {
        let original = SharedAgentToolContext::new(
            AgentToolContextSpec::try_new(
                BTreeMap::from([("TOKEN".to_string(), "original".to_string())]),
                BTreeSet::from(["TOKEN".to_string()]),
                true,
            )
            .unwrap(),
        );
        let replacement = SharedAgentToolContext::new(
            AgentToolContextSpec::try_new(
                BTreeMap::from([("TOKEN".to_string(), "replacement".to_string())]),
                BTreeSet::from(["TOKEN".to_string()]),
                true,
            )
            .unwrap(),
        );
        let mut contexts = HashMap::from([(
            "session-1".to_string(),
            InstalledAgentToolContext {
                installation_id: 2,
                context: replacement.clone(),
                owner: AgentToolContextOwner::Leased,
                python: None,
            },
        )]);

        assert!(take_matching_tool_context(&mut contexts, "session-1", 1, &original).is_none());
        assert_eq!(
            contexts["session-1"].context.snapshot().values["TOKEN"],
            "replacement"
        );

        let removed = take_matching_tool_context(&mut contexts, "session-1", 2, &replacement)
            .expect("the exact replacement lease should remove its context");
        assert!(removed.context.ptr_eq(&replacement));
        assert!(contexts.is_empty());
    }

    #[test]
    fn leased_tool_context_requires_the_exact_surface_capability() {
        let leased = SharedAgentToolContext::new(
            AgentToolContextSpec::try_new(
                BTreeMap::from([("TOKEN".to_string(), "leased-secret".to_string())]),
                BTreeSet::from(["TOKEN".to_string()]),
                true,
            )
            .unwrap(),
        );
        let mut contexts = HashMap::from([(
            "session-1".to_string(),
            InstalledAgentToolContext {
                installation_id: 7,
                context: leased.clone(),
                owner: AgentToolContextOwner::Leased,
                python: None,
            },
        )]);
        let access = AgentToolContextAccess {
            account_scope: Arc::from("account-1"),
            session_id: Arc::from("session-1"),
            installation_id: 7,
            context: leased.clone(),
        };

        assert!(
            resolve_session_tool_context(
                &mut contexts,
                "account-1",
                "session-1",
                None,
                &AgentToolContextSpec::default(),
            )
            .is_err()
        );
        assert!(
            resolve_session_tool_context(
                &mut contexts,
                "account-1",
                "session-1",
                Some(&access),
                &AgentToolContextSpec::default(),
            )
            .unwrap()
            .ptr_eq(&leased)
        );

        leased.revoke();
        let error = match resolve_session_tool_context(
            &mut contexts,
            "account-1",
            "session-1",
            None,
            &AgentToolContextSpec::default(),
        ) {
            Err(error) => error,
            Ok(_) => panic!("a revoked lease must remain authoritative until explicit release"),
        };
        assert!(error.contains("controlled by another Agent surface"));
        assert!(contexts["session-1"].context.ptr_eq(&leased));
        assert_eq!(contexts["session-1"].owner, AgentToolContextOwner::Leased);
    }

    #[test]
    fn uncommitted_tool_context_installation_revokes_synchronously() {
        let context = SharedAgentToolContext::new(
            AgentToolContextSpec::try_new(
                BTreeMap::from([("TOKEN".to_string(), "secret".to_string())]),
                BTreeSet::from(["TOKEN".to_string()]),
                true,
            )
            .unwrap(),
        );
        {
            let _pending = PendingAgentToolContextInstallation::new(context.clone());
        }
        assert!(context.is_revoked());
        assert!(context.snapshot().values.is_empty());
    }

    #[tokio::test]
    async fn external_surface_setup_cancellation_interrupts_later_awaits() {
        let context = SharedAgentToolContext::new(
            AgentToolContextSpec::try_new(
                BTreeMap::from([("TOKEN".to_string(), "secret".to_string())]),
                BTreeSet::from(["TOKEN".to_string()]),
                true,
            )
            .unwrap(),
        );
        let cancellation = CancellationToken::new();
        let entered = Arc::new(tokio::sync::Notify::new());
        let blocker = Arc::new(tokio::sync::Notify::new());
        let task_context = context.clone();
        let task_cancellation = cancellation.clone();
        let task_entered = Arc::clone(&entered);
        let task_blocker = Arc::clone(&blocker);
        let setup = tokio::spawn(async move {
            run_external_surface_setup(
                &task_cancellation,
                &task_context,
                "surface closed",
                async move {
                    task_entered.notify_one();
                    task_blocker.notified().await;
                    Ok::<_, String>(())
                },
            )
            .await
        });

        entered.notified().await;
        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), setup)
            .await
            .expect("surface cancellation must not wait for a stalled setup phase")
            .unwrap()
            .unwrap_err();

        assert_eq!(error, "surface closed");
        assert!(context.is_revoked());
        assert!(context.snapshot().values.is_empty());
    }

    #[tokio::test]
    async fn cancelled_tool_context_release_retries_after_session_lifecycle_fence() {
        let (test_root, service, session_manager, project_root, account_scope) =
            tool_context_cleanup_test_context("cancelled-tool-context-release").await;
        let session = session_manager
            .create_session(
                project_root,
                "Cancelled lease release".to_string(),
                SessionType::Acp,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let context = SharedAgentToolContext::new(
            AgentToolContextSpec::try_new(
                BTreeMap::from([("TOKEN".to_string(), "lease-secret".to_string())]),
                BTreeSet::from(["TOKEN".to_string()]),
                true,
            )
            .unwrap(),
        );
        let installation_id = next_tool_context_installation_id();
        let access = AgentToolContextAccess {
            account_scope,
            session_id: Arc::from(session.id.as_str()),
            installation_id,
            context: context.clone(),
        };
        service
            .inner
            .lock()
            .await
            .as_mut()
            .unwrap()
            .session_tool_contexts
            .insert(
                session.id.clone(),
                InstalledAgentToolContext {
                    installation_id,
                    context: context.clone(),
                    owner: AgentToolContextOwner::Leased,
                    python: None,
                },
            );
        let lease = AgentToolContextLease {
            service: service.clone(),
            access: access.clone(),
            created_cleanup: None,
            discard_created_on_drop: false,
            cleanup_started: false,
        };

        let session_lifecycle_guard = service.session_lifecycle.lock().await;
        let release_task = tokio::spawn(lease.release());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !context.is_revoked() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lease release should revoke before waiting for the lifecycle fence");
        release_task.abort();
        assert!(release_task.await.unwrap_err().is_cancelled());

        let exact_lease_remains_fenced = service
            .inner
            .lock()
            .await
            .as_ref()
            .unwrap()
            .session_tool_contexts
            .get(&session.id)
            .is_some_and(|installed| {
                installed.installation_id == installation_id
                    && installed.context.ptr_eq(&context)
                    && installed.owner == AgentToolContextOwner::Leased
            });
        assert!(exact_lease_remains_fenced);

        drop(session_lifecycle_guard);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let removed = !service
                    .inner
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .session_tool_contexts
                    .contains_key(&session.id);
                if removed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Drop should retry exact lease cleanup after cancellation");

        drop(service);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn dropped_armed_pending_lease_cleans_registry_after_lifecycle_fence() {
        let (test_root, service, session_manager, project_root, account_scope) =
            tool_context_cleanup_test_context("dropped-pending-lease").await;
        let session = session_manager
            .create_session(
                project_root,
                "Pending attached lease".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let context = SharedAgentToolContext::new(AgentToolContextSpec::default());
        let installation_id = next_tool_context_installation_id();
        let access = AgentToolContextAccess {
            account_scope,
            session_id: Arc::from(session.id.as_str()),
            installation_id,
            context: context.clone(),
        };
        service
            .inner
            .lock()
            .await
            .as_mut()
            .unwrap()
            .session_tool_contexts
            .insert(
                session.id.clone(),
                InstalledAgentToolContext {
                    installation_id,
                    context: context.clone(),
                    owner: AgentToolContextOwner::Leased,
                    python: None,
                },
            );
        let mut pending = PendingAgentToolContextInstallation::new(context.clone());
        pending.arm_lease_cleanup(service.clone(), access);

        let session_lifecycle_guard = service.session_lifecycle.lock().await;
        drop(pending);
        assert!(context.is_revoked());
        assert!(
            service
                .inner
                .lock()
                .await
                .as_ref()
                .unwrap()
                .session_tool_contexts
                .contains_key(&session.id)
        );

        drop(session_lifecycle_guard);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let removed = !service
                    .inner
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .session_tool_contexts
                    .contains_key(&session.id);
                if removed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping an armed pending installation should clean its exact lease");
        assert!(
            session_manager
                .get_session(&session.id, false)
                .await
                .is_ok()
        );

        drop(service);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn dropped_pending_created_cleanup_deletes_only_untouched_provisional_session() {
        let (test_root, service, session_manager, project_root, account_scope) =
            tool_context_cleanup_test_context("dropped-pending-created").await;
        let agent_manager = Arc::clone(&service.inner.lock().await.as_ref().unwrap().agent_manager);
        let untouched = session_manager
            .create_session(
                project_root.clone(),
                "Untouched provisional task".to_string(),
                SessionType::Acp,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let modified = session_manager
            .create_session(
                project_root,
                "Modified provisional task".to_string(),
                SessionType::Acp,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        session_manager
            .add_message(
                &modified.id,
                &Message::user()
                    .with_text("Preserve this concurrent change")
                    .with_generated_id(),
            )
            .await
            .unwrap();

        let untouched_context = SharedAgentToolContext::new(AgentToolContextSpec::default());
        let untouched_installation_id = next_tool_context_installation_id();
        let untouched_access = AgentToolContextAccess {
            account_scope: Arc::clone(&account_scope),
            session_id: Arc::from(untouched.id.as_str()),
            installation_id: untouched_installation_id,
            context: untouched_context.clone(),
        };
        let modified_context = SharedAgentToolContext::new(AgentToolContextSpec::default());
        let modified_installation_id = next_tool_context_installation_id();
        let modified_access = AgentToolContextAccess {
            account_scope,
            session_id: Arc::from(modified.id.as_str()),
            installation_id: modified_installation_id,
            context: modified_context.clone(),
        };
        {
            let mut runtime = service.inner.lock().await;
            let contexts = &mut runtime.as_mut().unwrap().session_tool_contexts;
            contexts.insert(
                untouched.id.clone(),
                InstalledAgentToolContext {
                    installation_id: untouched_installation_id,
                    context: untouched_context.clone(),
                    owner: AgentToolContextOwner::Leased,
                    python: None,
                },
            );
            contexts.insert(
                modified.id.clone(),
                InstalledAgentToolContext {
                    installation_id: modified_installation_id,
                    context: modified_context.clone(),
                    owner: AgentToolContextOwner::Leased,
                    python: None,
                },
            );
        }
        let mut untouched_pending =
            PendingAgentToolContextInstallation::new(untouched_context.clone());
        untouched_pending.arm_created_cleanup(
            service.clone(),
            untouched_access,
            Arc::clone(&agent_manager),
            Arc::clone(&session_manager),
            untouched.clone(),
        );
        let mut modified_pending =
            PendingAgentToolContextInstallation::new(modified_context.clone());
        modified_pending.arm_created_cleanup(
            service.clone(),
            modified_access,
            agent_manager,
            Arc::clone(&session_manager),
            modified.clone(),
        );

        let session_lifecycle_guard = service.session_lifecycle.lock().await;
        drop(untouched_pending);
        drop(modified_pending);
        assert!(untouched_context.is_revoked());
        assert!(modified_context.is_revoked());
        assert_eq!(
            service
                .inner
                .lock()
                .await
                .as_ref()
                .unwrap()
                .session_tool_contexts
                .len(),
            2
        );

        drop(session_lifecycle_guard);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let contexts_removed = service
                    .inner
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .session_tool_contexts
                    .is_empty();
                let untouched_deleted = session_manager
                    .get_session(&untouched.id, false)
                    .await
                    .is_err();
                if contexts_removed && untouched_deleted {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("created cleanup should remove the untouched provisional task");

        let preserved = session_manager
            .get_session(&modified.id, true)
            .await
            .expect("cleanup must preserve a provisional task changed by another owner");
        assert_eq!(preserved.message_count, 1);
        assert!(preserved.conversation.is_some_and(|conversation| {
            conversation
                .messages()
                .iter()
                .any(|message| message.as_concat_text() == "Preserve this concurrent change")
        }));

        drop(service);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn dropped_pending_created_cleanup_survives_runtime_removal() {
        let (test_root, service, session_manager, project_root, account_scope) =
            tool_context_cleanup_test_context("dropped-pending-created-runtime-removal").await;
        let agent_manager = Arc::clone(&service.inner.lock().await.as_ref().unwrap().agent_manager);
        let provisional = session_manager
            .create_session(
                project_root,
                "Runtime-stopped provisional task".to_string(),
                SessionType::Acp,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let context = SharedAgentToolContext::new(AgentToolContextSpec::default());
        let installation_id = next_tool_context_installation_id();
        let access = AgentToolContextAccess {
            account_scope,
            session_id: Arc::from(provisional.id.as_str()),
            installation_id,
            context: context.clone(),
        };
        service
            .inner
            .lock()
            .await
            .as_mut()
            .unwrap()
            .session_tool_contexts
            .insert(
                provisional.id.clone(),
                InstalledAgentToolContext {
                    installation_id,
                    context: context.clone(),
                    owner: AgentToolContextOwner::Leased,
                    python: None,
                },
            );
        let mut pending = PendingAgentToolContextInstallation::new(context.clone());
        pending.arm_created_cleanup(
            service.clone(),
            access,
            agent_manager,
            Arc::clone(&session_manager),
            provisional.clone(),
        );

        // Runtime shutdown drains the in-memory registry before the abandoned
        // handshake's Drop cleanup can run. The captured account-scoped
        // managers must still remove only the untouched provisional row.
        let removed_runtime = service.inner.lock().await.take();
        drop(removed_runtime);
        drop(pending);
        assert!(context.is_revoked());

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if session_manager
                    .get_session(&provisional.id, false)
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("captured managers should clean the provisional row after runtime removal");

        drop(service);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn exported_account_paths_match_the_on_disk_layout() {
        let paths =
            AgentPathLayout::from_app_roots(PathBuf::from("/config"), PathBuf::from("/data"));
        let user_id = "exported-account-paths-user";
        let scope = account_scope(user_id).unwrap();
        let account_dir = PathBuf::from("/data")
            .join("agent")
            .join("accounts")
            .join(&scope);

        assert_eq!(
            account_local_data_dir(&paths, user_id).unwrap(),
            account_dir
        );
        assert_eq!(
            account_sessions_db_path(&paths, user_id).unwrap(),
            account_dir
                .join("goose")
                .join("data")
                .join("sessions")
                .join("sessions.db")
        );
        assert_eq!(
            account_tool_summaries_db_path(&paths, user_id).unwrap(),
            account_dir.join("tool_summaries.db")
        );
    }

    #[test]
    fn exported_sessions_db_path_is_the_one_the_runtime_writes() {
        let test_root = recent_roots_test_dir("exported-sessions-db-path");
        let paths = AgentPathLayout::from_app_roots(
            test_root.join("app-config"),
            test_root.join("app-local-data"),
        );
        let user_id = "exported-sessions-db-path-user";

        let history_dir = account_history_dir(&paths, user_id).unwrap();
        let db_path = account_sessions_db_path(&paths, user_id).unwrap();

        assert_eq!(db_path.parent().unwrap().parent().unwrap(), history_dir);
    }

    #[test]
    fn account_scopes_are_deterministic_isolated_and_opaque() {
        let first = account_scope("user-123").expect("account ID should be valid");
        assert_eq!(first, account_scope(" user-123 ").unwrap());
        assert_ne!(first, account_scope("user-456").unwrap());
        assert_eq!(first.len(), 64);
        assert!(!first.contains("user-123"));
    }

    #[test]
    fn rejects_wrong_runtime_account_scope() {
        let first = account_scope("first-user").unwrap();
        let second = account_scope("second-user").unwrap();
        assert!(ensure_account_scope(&first, &first).is_ok());
        assert!(ensure_account_scope(&first, &second).is_err());
    }

    #[test]
    fn user_provided_session_title_is_visible_single_line_and_unicode_safe() {
        assert_eq!(
            normalize_user_provided_session_title("\u{2003}Renamed 🙂\u{2003}").unwrap(),
            "Renamed 🙂"
        );
        assert_eq!(
            normalize_user_provided_session_title(" \n\t ").unwrap_err(),
            "Agent task title cannot be empty"
        );
        for invalid in [
            "First\nSecond",
            "First\rSecond",
            "First\tSecond",
            "First\0Second",
            "First\u{2028}Second",
            "First\u{2029}Second",
        ] {
            assert_eq!(
                normalize_user_provided_session_title(invalid).unwrap_err(),
                "Agent task title must be a single line without control characters"
            );
        }
        for invisible in ["\u{200b}", "\u{200d}\u{fe0f}"] {
            assert_eq!(
                normalize_user_provided_session_title(invisible).unwrap_err(),
                "Agent task title must contain visible characters"
            );
        }
        for valid in [
            "日本語のタスク",
            "مهمة عربية",
            "می\u{200c}روم",
            "👩\u{200d}💻 plan",
        ] {
            assert_eq!(normalize_user_provided_session_title(valid).unwrap(), valid);
        }

        let maximum = "🙂".repeat(MAX_AGENT_SESSION_TITLE_CHARS);
        assert_eq!(
            normalize_user_provided_session_title(&maximum).unwrap(),
            maximum
        );
        assert_eq!(
            normalize_user_provided_session_title(&"🙂".repeat(MAX_AGENT_SESSION_TITLE_CHARS + 1))
                .unwrap_err(),
            format!("Agent task title must be {MAX_AGENT_SESSION_TITLE_CHARS} characters or fewer")
        );
    }

    #[tokio::test]
    async fn session_title_lifecycles_are_account_and_task_scoped() {
        let lifecycles = Arc::new(Mutex::new(HashMap::new()));
        let first = resolve_session_title_lifecycle(&lifecycles, "account-a", "task-a").await;
        let same = resolve_session_title_lifecycle(&lifecycles, "account-a", "task-a").await;
        let other_task = resolve_session_title_lifecycle(&lifecycles, "account-a", "task-b").await;
        let other_account =
            resolve_session_title_lifecycle(&lifecycles, "account-b", "task-a").await;

        assert!(Arc::ptr_eq(&first, &same));
        assert!(!Arc::ptr_eq(&first, &other_task));
        assert!(!Arc::ptr_eq(&first, &other_account));
    }

    #[tokio::test]
    async fn unchanged_new_task_rename_preserves_automatic_title_eligibility() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) =
            agent_service_test_context("unchanged-new-task-rename", sink.clone());
        let user_id = "unchanged-new-task-rename-user";
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = account_session_manager(&paths, user_id).unwrap();
        let session = session_manager
            .create_session(
                project_root,
                DEFAULT_AGENT_SESSION_TITLE.to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();

        let summary = state
            .handle_for_user(user_id)
            .await
            .unwrap()
            .rename_session(
                crate::maple_api::test_maple_api_session(user_id),
                AgentRenameSessionRequest {
                    session_id: session.id.clone(),
                    title: format!("  {DEFAULT_AGENT_SESSION_TITLE}  "),
                },
            )
            .await
            .unwrap();

        assert_eq!(summary.title, DEFAULT_AGENT_SESSION_TITLE);
        let persisted = session_manager
            .get_session(&session.id, false)
            .await
            .unwrap();
        assert!(!persisted.user_set_name);
        assert!(should_name_session_from_prompt(&persisted));
        assert!(
            sink.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );

        drop(state);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn only_the_users_own_tasks_reach_the_task_list() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) = agent_service_test_context("subagent-sessions", sink);
        let user = "subagent-list-user";
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let manager = account_session_manager(&paths, user).unwrap();
        manager
            .create_session(
                project_root.clone(),
                "Real task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        // Goose gives every delegated task a session of its own, and makes
        // sessions for other work of its own. None of them is a task.
        for (title, session_type) in [
            ("Delegated task", SessionType::SubAgent),
            ("Scheduled run", SessionType::Scheduled),
            ("Goose internals", SessionType::Hidden),
            ("Goose gateway", SessionType::Gateway),
            ("Goose terminal", SessionType::Terminal),
        ] {
            manager
                .create_session(
                    project_root.clone(),
                    title.to_string(),
                    session_type,
                    GooseMode::Auto,
                )
                .await
                .unwrap();
        }

        let handle = state.handle_for_user(user).await.unwrap();
        let listed = handle.list_sessions(None).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|task| task.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Real task"]
        );

        drop(handle);
        drop(state);
        drop(manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn rename_session_is_account_scoped_persistent_and_authoritative() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) = agent_service_test_context("rename-session", sink.clone());
        let first_user = "rename-user-a";
        let second_user = "rename-user-b";
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let first_manager = account_session_manager(&paths, first_user).unwrap();
        let second_manager = account_session_manager(&paths, second_user).unwrap();
        let first_session = first_manager
            .create_session(
                project_root.clone(),
                "Original task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let second_session = second_manager
            .create_session(
                project_root.clone(),
                "Other account task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        first_manager
            .update(&first_session.id)
            .model_config(ModelConfig::new("preserved-model").with_context_limit(Some(42_424)))
            .goose_mode(GooseMode::Chat)
            .apply()
            .await
            .unwrap();
        first_manager
            .add_message(
                &first_session.id,
                &Message::user()
                    .with_text("Preserve this message")
                    .with_generated_id(),
            )
            .await
            .unwrap();
        let before = first_manager
            .get_session(&first_session.id, true)
            .await
            .unwrap();
        let handle = state.handle_for_user(first_user).await.unwrap();
        let api_session = crate::maple_api::test_maple_api_session(first_user);
        assert_eq!(first_session.id, second_session.id);

        let summary = handle
            .rename_session(
                api_session,
                AgentRenameSessionRequest {
                    session_id: format!(" {} ", first_session.id),
                    title: " \u{2003}Renamed task 🙂\u{2003} ".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(summary.id, first_session.id);
        assert_eq!(summary.title, "Renamed task 🙂");
        assert_eq!(summary.project_root, path_string(&project_root));
        assert_eq!(summary.message_count, 1);
        assert_eq!(summary.model.as_deref(), Some("preserved-model"));
        assert_eq!(summary.mode, "chat");

        let persisted = first_manager
            .get_session(&first_session.id, true)
            .await
            .unwrap();
        assert_eq!(persisted.name, "Renamed task 🙂");
        assert!(persisted.user_set_name);
        assert_eq!(
            persisted_session_except_title(&persisted),
            persisted_session_except_title(&before)
        );
        let isolated = second_manager
            .get_session(&second_session.id, false)
            .await
            .unwrap();
        assert_eq!(isolated.name, "Other account task");
        assert!(!isolated.user_set_name);

        let events = sink
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentServiceEvent::SessionUpdated {
                session_id,
                run_id,
                session,
            } => {
                assert_eq!(session_id, &first_session.id);
                assert!(run_id.is_none());
                assert_eq!(session.title, "Renamed task 🙂");
            }
            event => panic!("unexpected rename event: {event:?}"),
        }

        drop(handle);
        drop(state);
        drop(first_manager);
        drop(second_manager);
        let restarted = MapleAgentService::new(MapleAgentHostResources::new(
            paths.clone(),
            Arc::new(NoopAgentEventSink),
            AgentToolContextSpec::default(),
            "You are a test agent.".to_string(),
        ));
        let restarted_handle = restarted.handle_for_user(first_user).await.unwrap();
        let listed = restarted_handle.list_sessions(None).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "Renamed task 🙂");
        let loaded = restarted_handle
            .load_session(first_session.id.clone())
            .await
            .unwrap();
        assert_eq!(loaded.session.title, "Renamed task 🙂");
        assert_eq!(loaded.session.message_count, 1);

        drop(restarted_handle);
        drop(restarted);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn stale_handle_cannot_rename_or_emit_an_event() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) = agent_service_test_context("stale-rename", sink.clone());
        let user_id = "stale-rename-user";
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = account_session_manager(&paths, user_id).unwrap();
        let session = session_manager
            .create_session(
                project_root,
                "Original task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let stale_handle = state.handle_for_user(user_id).await.unwrap();
        let api_session = crate::maple_api::test_maple_api_session(user_id);
        advance_account_generation(&state, &account_scope(user_id).unwrap()).await;

        assert_eq!(
            stale_handle
                .rename_session(
                    api_session,
                    AgentRenameSessionRequest {
                        session_id: session.id.clone(),
                        title: "Rejected rename".to_string(),
                    },
                )
                .await
                .unwrap_err(),
            "Agent Mode data changed while this operation was waiting"
        );
        let persisted = session_manager
            .get_session(&session.id, false)
            .await
            .unwrap();
        assert_eq!(persisted.name, "Original task");
        assert!(!persisted.user_set_name);
        assert!(
            sink.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );

        drop(stale_handle);
        drop(state);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn invalidated_auth_cannot_rename_after_waiting_for_session_lifecycle() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) =
            agent_service_test_context("invalidated-auth-rename", sink.clone());
        let user_id = "invalidated-auth-rename-user";
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = account_session_manager(&paths, user_id).unwrap();
        let session = session_manager
            .create_session(
                project_root,
                "Original task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let handle = state.handle_for_user(user_id).await.unwrap();
        let api_session = crate::maple_api::test_maple_api_session(user_id);
        let session_lifecycle_guard = state.session_lifecycle.lock().await;
        let rename_api_session = Arc::clone(&api_session);
        let rename_session_id = session.id.clone();
        let rename_task = tokio::spawn(async move {
            handle
                .rename_session(
                    rename_api_session,
                    AgentRenameSessionRequest {
                        session_id: rename_session_id,
                        title: "Rejected rename".to_string(),
                    },
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state.runtime_lifecycle.try_lock().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the rename should wait for the session lifecycle");

        api_session.invalidate_for_test().await;
        drop(session_lifecycle_guard);

        assert_eq!(
            rename_task.await.unwrap().unwrap_err(),
            "Maple API authentication is no longer active"
        );
        let persisted = session_manager
            .get_session(&session.id, false)
            .await
            .unwrap();
        assert_eq!(persisted.name, "Original task");
        assert!(!persisted.user_set_name);
        assert!(
            sink.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );

        drop(state);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    /// The ask_user tool registers pending questions in the process-global
    /// broker while answers arrive through the service. Both ends must share
    /// one map or the tool blocks forever and the UI sees "no longer
    /// waiting".
    #[tokio::test]
    async fn answer_question_round_trips_through_the_global_broker() {
        use std::sync::Mutex as StdMutex;
        #[derive(Default)]
        struct CapturingSink(StdMutex<Vec<AgentServiceEvent>>);
        impl AgentEventSink for CapturingSink {
            fn emit(&self, event: &AgentServiceEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }
        // The service constructor installs its own broker as the process
        // global; parallel tests replace the global, so answer through the
        // service's instance.
        let sink = Arc::new(CapturingSink::default());
        let state = MapleAgentService::new(MapleAgentHostResources::new(
            AgentPathLayout::from_app_roots(
                PathBuf::from("unused-config-root"),
                PathBuf::from("unused-local-root"),
            ),
            sink.clone(),
            AgentToolContextSpec::default(),
            "You are a test agent.".to_string(),
        ));
        let broker = state.question_broker();
        let one_question = |id: &str, text: &str| AgentQuestion {
            id: id.to_string(),
            header: "Question".to_string(),
            question: text.to_string(),
            options: Vec::new(),
        };
        let ask = {
            let broker = broker.clone();
            let questions = vec![one_question("continue", "Continue?")];
            tokio::spawn(async move { broker.ask("s-roundtrip", questions).await })
        };
        let request_id = loop {
            let found = sink.0.lock().unwrap().iter().find_map(|event| match event {
                AgentServiceEvent::Question { request_id, .. } => Some(request_id.clone()),
                _ => None,
            });
            if let Some(request_id) = found {
                break request_id;
            }
            assert!(
                !ask.is_finished(),
                "ask returned before the question event fired"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        assert!(state.answer_question(&request_id, "yes".into()).await);
        assert_eq!(ask.await.unwrap(), "yes");

        // A follow-up question must be answerable through the same path.
        let ask2 = {
            let broker = broker.clone();
            let questions = vec![one_question("next", "And now?")];
            tokio::spawn(async move { broker.ask("s-roundtrip", questions).await })
        };
        let request_id2 = loop {
            let found = sink
                .0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find_map(|event| match event {
                    AgentServiceEvent::Question { request_id, .. } => Some(request_id.clone()),
                    _ => None,
                });
            if let Some(latest) = found
                && latest != request_id
            {
                break latest;
            }
            assert!(!ask2.is_finished(), "ask2 returned early");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        assert!(state.answer_question(&request_id2, "again".into()).await);
        assert_eq!(ask2.await.unwrap(), "again");
    }

    #[tokio::test]
    async fn rejects_operations_captured_before_account_clear() {
        let state = MapleAgentService::new(MapleAgentHostResources::new(
            AgentPathLayout::from_app_roots(
                PathBuf::from("unused-config-root"),
                PathBuf::from("unused-local-root"),
            ),
            Arc::new(NoopAgentEventSink),
            AgentToolContextSpec::default(),
            "You are a test agent.".to_string(),
        ));
        let stale_handle = state.handle_for_user("user-to-clear").await.unwrap();
        let scope = account_scope("user-to-clear").unwrap();

        advance_account_generation(&state, &scope).await;
        let current_handle = state.handle_for_user("user-to-clear").await.unwrap();

        assert!(stale_handle.verify_generation().await.is_err());
        assert!(current_handle.verify_generation().await.is_ok());
    }

    #[tokio::test]
    async fn service_drain_rejects_new_work_and_failed_update_can_reopen_it() {
        let state = MapleAgentService::new(MapleAgentHostResources::new(
            AgentPathLayout::from_app_roots(
                PathBuf::from("unused-config-root"),
                PathBuf::from("unused-local-root"),
            ),
            Arc::new(NoopAgentEventSink),
            AgentToolContextSpec::default(),
            "You are a test agent.".to_string(),
        ));
        let handle = state.handle_for_user("user-during-shutdown").await.unwrap();

        assert!(state.ensure_accepting_new_work().is_ok());
        assert!(handle.ensure_accepting_new_work().is_ok());

        state.begin_draining();
        assert_eq!(
            state.ensure_accepting_new_work().unwrap_err(),
            AGENT_SERVICE_DRAINING_ERROR
        );
        assert_eq!(
            handle.ensure_accepting_new_work().unwrap_err(),
            AGENT_SERVICE_DRAINING_ERROR
        );

        state.reopen_after_failed_shutdown();
        assert!(state.ensure_accepting_new_work().is_ok());
        assert!(handle.ensure_accepting_new_work().is_ok());
    }

    #[test]
    fn run_ids_are_unique() {
        let ids = (0..10_000)
            .map(|_| next_run_id())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 10_000);
    }

    #[tokio::test]
    async fn forced_task_shutdown_joins_aborted_task() {
        struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel();
        let task_dropped = Arc::clone(&dropped);
        let task = tokio::spawn(async move {
            let _drop_flag = DropFlag(task_dropped);
            let _ = started_tx.send(());
            futures_util::future::pending::<()>().await;
        });
        started_rx.await.unwrap();

        join_agent_tasks(vec![task], std::time::Duration::from_millis(1)).await;

        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn session_title_collapses_whitespace_and_bounds_unicode() {
        assert_eq!(
            session_title_from_prompt("  inspect\n\tthis   repo  "),
            "inspect this repo"
        );

        let title = session_title_from_prompt(&"🙂 ".repeat(100));
        assert!(title.chars().count() <= MAX_AGENT_SESSION_TITLE_CHARS);
        assert!(title.ends_with('…'));
        assert!(!title.contains("  "));
    }

    #[test]
    fn generated_session_title_is_sanitized_and_bounded() {
        assert_eq!(
            normalize_generated_session_title(
                "<think>ignore me</think>  \"Friendly   Check-In\"\nextra"
            ),
            Some("Friendly Check-In".to_string())
        );
        assert_eq!(
            normalize_generated_session_title("<analysis>ignore me</analysis>"),
            None
        );
        assert!(
            normalize_generated_session_title(&"word ".repeat(100))
                .is_some_and(|title| title.chars().count() <= MAX_AGENT_SESSION_TITLE_CHARS)
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_agents_md_link_is_replaced_instead_of_written_through() {
        use std::os::unix::fs::symlink;
        let root =
            std::env::temp_dir().join(format!("maple-agent-agents-md-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("home/.agents/AGENTS.md");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "global hints").unwrap();
        let target_dir = root.join("goose/.agents");
        fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("AGENTS.md");
        let stale = root.join("old-home/.agents/AGENTS.md");
        symlink(&stale, &target).unwrap();
        assert!(!target.exists(), "fixture link must dangle");

        link_agents_md_into(&source, &target_dir);

        assert_eq!(fs::read_link(&target).unwrap(), source);
        assert_eq!(fs::read_to_string(&target).unwrap(), "global hints");
        assert!(
            !stale.exists(),
            "nothing may be written through the stale link"
        );

        // A link that already resolves is left alone.
        link_agents_md_into(&source, &target_dir);
        assert_eq!(fs::read_link(&target).unwrap(), source);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn malformed_tool_request_rows_keep_a_stable_request_derived_id() {
        let request = goose::conversation::message::ToolRequest {
            id: "call-7".to_string(),
            tool_call: Err(rmcp::model::ErrorData::invalid_params("bad json", None)),
            metadata: None,
            tool_meta: None,
        };
        let first = tool_request_item(&request, 42);
        let second = tool_request_item(&request, 42);
        assert_eq!(first.id, "call-7-parse-error");
        assert_eq!(first.id, second.id);
        assert_eq!(first.created_ms, 42);
        assert_eq!(first.item_type, "error");
    }

    #[test]
    fn subagent_session_ids_stay_out_of_the_transcript() {
        // Collecting a background subagent is named by session ID, which
        // means nothing to the user.
        assert_eq!(
            descriptive_tool_title("load", &json!({ "source": "20260831_4" })).as_deref(),
            Some("Subagent result")
        );
        // A recipe or agent file is named by something the user chose.
        assert_eq!(
            descriptive_tool_title("load", &json!({ "source": "reviewer" })).as_deref(),
            Some("Load: reviewer")
        );
        assert_eq!(
            descriptive_tool_title("delegate", &json!({ "instructions": "Review the parser" }))
                .as_deref(),
            Some("Subagent: Review the parser")
        );

        assert!(looks_like_session_id("20260831_4"));
        assert!(!looks_like_session_id("2026083_4"));
        assert!(!looks_like_session_id("20260831_4_1"));
        assert!(!looks_like_session_id("reviewer"));

        // The two results Goose writes with an ID in them.
        assert_eq!(
            subagent_text_without_ids(
                "Task 20260831_4 started in background: \"Build the release\"\n\
                 Continue with other work. When you need the result, use load(source: \"20260831_4\")."
            )
            .as_deref(),
            Some("Started in the background: Build the release")
        );
        assert_eq!(
            subagent_text_without_ids(
                "# Background Task Result: 20260831_4\n\n**Task:** Build\n\n## Output\n\nDone"
            )
            .as_deref(),
            Some("# Background Task Result\n\n**Task:** Build\n\n## Output\n\nDone")
        );
        assert_eq!(
            subagent_text_without_ids(
                "# Background Task Status: 20260831_4\n\n**Status:** ⏳ Running"
            )
            .as_deref(),
            Some("# Background Task Status\n\n**Status:** ⏳ Running")
        );
        // Any other tool result is left exactly as it came.
        assert_eq!(subagent_text_without_ids("Task list: 3 open"), None);
        assert_eq!(subagent_text_without_ids("# Background Task Result"), None);
    }

    #[test]
    fn descriptive_tool_title_prefers_the_file_over_editor_subcommands() {
        assert_eq!(
            descriptive_tool_title(
                "developer__text_editor",
                &json!({"command": "view", "path": "src/main.rs"})
            )
            .as_deref(),
            Some("Editor: src/main.rs")
        );
        assert_eq!(
            descriptive_tool_title(
                "developer__shell",
                &json!({"command": "ls -la", "path": "/tmp"})
            )
            .as_deref(),
            Some("Terminal: ls -la")
        );
        assert_eq!(
            descriptive_tool_title("developer__shell", &json!({"path": "/tmp"})).as_deref(),
            Some("Terminal: /tmp")
        );
        assert_eq!(descriptive_tool_title("developer__shell", &json!({})), None);
    }

    #[test]
    fn startup_sweep_only_removes_aged_unprompted_acp_tasks() {
        let now = chrono::Utc::now();
        let fresh = Session {
            session_type: SessionType::Acp,
            updated_at: now - chrono::Duration::seconds(30),
            ..Session::default()
        };
        assert!(is_unprompted_acp_session(&fresh));
        assert!(
            !is_stale_unprompted_acp_session(&fresh, now),
            "a concurrent ACP process may still be about to prompt this row"
        );

        let aged = Session {
            updated_at: now - UNPROMPTED_ACP_SESSION_SWEEP_AGE - chrono::Duration::seconds(1),
            ..fresh.clone()
        };
        assert!(is_stale_unprompted_acp_session(&aged, now));

        let prompted = Session {
            message_count: 1,
            ..aged.clone()
        };
        assert!(!is_stale_unprompted_acp_session(&prompted, now));
    }

    #[test]
    fn semantic_title_generation_is_only_for_pristine_default_tasks() {
        let mut session = Session {
            name: DEFAULT_AGENT_SESSION_TITLE.to_string(),
            ..Session::default()
        };
        assert!(should_name_session_from_prompt(&session));

        session.name = "Buzz ACP".to_string();
        assert!(!should_name_session_from_prompt(&session));

        session.name = DEFAULT_AGENT_SESSION_TITLE.to_string();
        session.user_set_name = true;
        assert!(!should_name_session_from_prompt(&session));

        session.user_set_name = false;
        session.message_count = 1;
        assert!(!should_name_session_from_prompt(&session));
    }

    #[tokio::test]
    async fn fast_reply_finishes_before_delayed_semantic_title_updates_session() {
        let test_root = recent_roots_test_dir("delayed-semantic-session-title");
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(SessionManager::new(test_root.join("sessions")));
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root,
                DEFAULT_AGENT_SESSION_TITLE.to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        session_manager
            .update(&session.id)
            .system_generated_name("hey how are you?".to_string())
            .apply()
            .await
            .unwrap();

        let title_started = Arc::new(tokio::sync::Notify::new());
        let title_release = Arc::new(tokio::sync::Notify::new());
        let title_cancel_token = CancellationToken::new();
        let provider = Arc::new(DelayedSessionTitleProvider {
            title_started: Arc::clone(&title_started),
            title_release: Arc::clone(&title_release),
            title_cancel_token: title_cancel_token.clone(),
        });
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));
        agent
            .update_provider(provider, ModelConfig::new(DEFAULT_AGENT_MODEL), &session.id)
            .await
            .unwrap();

        let sink = Arc::new(RecordingAgentEventSink::default());
        let dispatcher = AgentEventDispatcher::new(sink.clone());
        let (events, _run_events) = AgentRunEventPublisher::new(
            dispatcher.clone(),
            session.id.clone(),
            "fast-reply-run".to_string(),
            AgentHostEventPolicy::Suppress,
        );
        let session_lifecycle = Arc::new(Mutex::new(()));
        let session_title_lifecycle = Arc::new(Mutex::new(()));
        let (title_start, title_start_rx) = oneshot::channel();
        let (_title_settled, title_settled_rx) = oneshot::channel();
        let title_task = tokio::spawn(run_agent_session_title_task(AgentSessionTitleJob {
            start: title_start_rx,
            settled: title_settled_rx,
            session_manager: Arc::clone(&session_manager),
            agent: Arc::clone(&agent),
            session_lifecycle: Arc::clone(&session_lifecycle),
            session_title_lifecycle: Arc::clone(&session_title_lifecycle),
            session_id: session.id.clone(),
            first_prompt: "hey how are you?".to_string(),
            expected_fallback_title: "hey how are you?".to_string(),
            cancel_token: title_cancel_token,
            dispatcher,
            host_events: AgentHostEventPolicy::Publish,
        }));
        let prompt = "hey how are you?";
        let prompt_result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            run_agent_prompt(AgentPromptRun {
                events,
                agent: Arc::clone(&agent),
                session_manager: Arc::clone(&session_manager),
                session_title_lifecycle,
                live_timelines: Arc::new(Mutex::new(HashMap::new())),
                subagents: Arc::new(Mutex::new(HashMap::new())),
                subagent_host: None,
                session_id: session.id.clone(),
                user_message: Message::user().with_text(prompt).with_generated_id(),
                permission_modes: Arc::new(Mutex::new(HashMap::new())),
                web_tool_state: Arc::new(WebToolState::default()),
                web_permission_context: WebPermissionContext::from_user_prompt(prompt),
                cancel_token: CancellationToken::new(),
                session_title_start: Some(title_start),
                pending_permissions: Arc::new(Mutex::new(HashMap::new())),
                issued_permission_ids: Arc::new(Mutex::new(HashSet::new())),
                cancelled_permission_ids: Arc::new(Mutex::new(HashSet::new())),
                run_id: "fast-reply-run".to_string(),
                permission_routing: AgentPermissionRouting::Desktop,
                steered_unacked: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .await
        .expect("the conversational reply must not wait for its title")
        .expect("the conversational reply should succeed");
        assert!(prompt_result.terminal_message.is_some());
        tokio::time::timeout(std::time::Duration::from_secs(1), title_started.notified())
            .await
            .expect("the independent title request should start");
        tokio::time::sleep(std::time::Duration::from_millis(550)).await;
        assert!(!title_task.is_finished());
        assert_eq!(
            session_manager
                .get_session(&session.id, false)
                .await
                .unwrap()
                .name,
            prompt
        );

        title_release.notify_one();
        title_task.await.unwrap();

        assert_eq!(
            session_manager
                .get_session(&session.id, false)
                .await
                .unwrap()
                .name,
            "Friendly Check-In"
        );
        let emitted = sink
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(emitted.iter().any(|event| {
            matches!(
                event,
                AgentServiceEvent::SessionUpdated {
                    session_id,
                    run_id: None,
                    session,
                } if session_id == &session.id && session.title == "Friendly Check-In"
            )
        }));

        drop(emitted);
        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn rename_waits_for_initial_run_summary_before_publishing_new_title() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) =
            agent_service_test_context("rename-during-run-summary", sink.clone());
        let user_id = "rename-during-run-summary-user";
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(account_session_manager(&paths, user_id).unwrap());
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root,
                "Fallback task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));
        agent
            .update_provider(
                Arc::new(SessionTitleTestProvider {
                    calls: Arc::new(AtomicUsize::new(0)),
                    response: "Done",
                }),
                ModelConfig::new(DEFAULT_AGENT_MODEL),
                &session.id,
            )
            .await
            .unwrap();
        let handle = state.handle_for_user(user_id).await.unwrap();
        let api_session = crate::maple_api::test_maple_api_session(user_id);
        let (events, _run_events) = AgentRunEventPublisher::new(
            AgentEventDispatcher::new(sink.clone()),
            session.id.clone(),
            "rename-during-run-summary".to_string(),
            AgentHostEventPolicy::Publish,
        );
        let event_order = Arc::clone(&events.order);
        let event_order_guard = event_order.lock().await;
        let session_title_lifecycle = resolve_session_title_lifecycle(
            &state.session_title_lifecycles,
            &account_scope(user_id).unwrap(),
            &session.id,
        )
        .await;
        let prompt = "Start the task";
        let prompt_task = tokio::spawn(run_agent_prompt(AgentPromptRun {
            events,
            agent: Arc::clone(&agent),
            session_manager: Arc::clone(&session_manager),
            session_title_lifecycle: Arc::clone(&session_title_lifecycle),
            live_timelines: Arc::new(Mutex::new(HashMap::new())),
            subagents: Arc::new(Mutex::new(HashMap::new())),
            subagent_host: None,
            session_id: session.id.clone(),
            user_message: Message::user().with_text(prompt).with_generated_id(),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            web_tool_state: Arc::new(WebToolState::default()),
            web_permission_context: WebPermissionContext::from_user_prompt(prompt),
            cancel_token: CancellationToken::new(),
            session_title_start: None,
            pending_permissions: Arc::new(Mutex::new(HashMap::new())),
            issued_permission_ids: Arc::new(Mutex::new(HashSet::new())),
            cancelled_permission_ids: Arc::new(Mutex::new(HashSet::new())),
            run_id: "rename-during-run-summary".to_string(),
            permission_routing: AgentPermissionRouting::Desktop,
            steered_unacked: Arc::new(Mutex::new(Vec::new())),
        }));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if session_title_lifecycle.try_lock().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the run should hold its title lifecycle while publishing its summary");
        assert!(state.session_lifecycle.try_lock().is_ok());

        let rename_task = tokio::spawn(async move {
            handle
                .rename_session(
                    api_session,
                    AgentRenameSessionRequest {
                        session_id: session.id,
                        title: "Manual title".to_string(),
                    },
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state.runtime_lifecycle.try_lock().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the rename should wait behind the run summary");
        assert!(!rename_task.is_finished());

        drop(event_order_guard);
        prompt_task.await.unwrap().unwrap();
        let renamed = rename_task.await.unwrap().unwrap();
        assert_eq!(renamed.title, "Manual title");

        let emitted_titles = sink
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|event| match event {
                AgentServiceEvent::Run {
                    event: AgentRunEvent::SessionUpdated(session),
                    ..
                }
                | AgentServiceEvent::SessionUpdated { session, .. } => Some(session.title.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(emitted_titles, vec!["Fallback task", "Manual title"]);
        let persisted = session_manager
            .get_session(&renamed.id, false)
            .await
            .unwrap();
        assert_eq!(persisted.name, "Manual title");
        assert!(persisted.user_set_name);

        drop(agent);
        drop(session_manager);
        drop(state);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn initial_reply_poll_does_not_wait_for_unrelated_session_lifecycle() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) =
            agent_service_test_context("reply-poll-session-isolation", sink.clone());
        let user_id = "reply-poll-session-isolation-user";
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(account_session_manager(&paths, user_id).unwrap());
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root,
                "Fallback task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));
        agent
            .update_provider(
                Arc::new(SessionTitleTestProvider {
                    calls: Arc::clone(&calls),
                    response: "Done",
                }),
                ModelConfig::new(DEFAULT_AGENT_MODEL),
                &session.id,
            )
            .await
            .unwrap();
        let (events, _run_events) = AgentRunEventPublisher::new(
            AgentEventDispatcher::new(sink),
            session.id.clone(),
            "reply-poll-session-isolation".to_string(),
            AgentHostEventPolicy::Suppress,
        );
        let session_title_lifecycle = resolve_session_title_lifecycle(
            &state.session_title_lifecycles,
            &account_scope(user_id).unwrap(),
            &session.id,
        )
        .await;
        let unrelated_session_lifecycle_guard = state.session_lifecycle.lock().await;
        let prompt = "Start the task";
        let prompt_task = tokio::spawn(run_agent_prompt(AgentPromptRun {
            events,
            agent: Arc::clone(&agent),
            session_manager: Arc::clone(&session_manager),
            session_title_lifecycle,
            live_timelines: Arc::new(Mutex::new(HashMap::new())),
            subagents: Arc::new(Mutex::new(HashMap::new())),
            subagent_host: None,
            session_id: session.id.clone(),
            user_message: Message::user().with_text(prompt).with_generated_id(),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            web_tool_state: Arc::new(WebToolState::default()),
            web_permission_context: WebPermissionContext::from_user_prompt(prompt),
            cancel_token: CancellationToken::new(),
            session_title_start: None,
            pending_permissions: Arc::new(Mutex::new(HashMap::new())),
            issued_permission_ids: Arc::new(Mutex::new(HashSet::new())),
            cancelled_permission_ids: Arc::new(Mutex::new(HashSet::new())),
            run_id: "reply-poll-session-isolation".to_string(),
            permission_routing: AgentPermissionRouting::Desktop,
            steered_unacked: Arc::new(Mutex::new(Vec::new())),
        }));

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), prompt_task)
            .await
            .expect("an unrelated lifecycle holder must not block the first provider poll")
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(outcome.terminal_message.is_some());

        drop(unrelated_session_lifecycle_guard);
        drop(agent);
        drop(session_manager);
        drop(state);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn permission_mode_summary_waits_for_semantic_title_event() {
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) =
            agent_service_test_context("permission-mode-title-ordering", sink.clone());
        let user_id = "permission-mode-title-ordering-user";
        let account_scope = account_scope(user_id).unwrap();
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(account_session_manager(&paths, user_id).unwrap());
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root.clone(),
                "Fallback task".to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            Arc::clone(&permission_manager),
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));
        let agent_manager = Arc::new(
            AgentManager::new(
                GooseAgentConfig::new(
                    Arc::clone(&session_manager),
                    permission_manager,
                    None,
                    GooseMode::SmartApprove,
                    true,
                    GoosePlatform::GooseDesktop,
                ),
                Some(2),
            )
            .await
            .unwrap(),
        );
        let (run_events, _run_events_rx) = AgentRunEventPublisher::new(
            AgentEventDispatcher::new(sink.clone()),
            session.id.clone(),
            "permission-mode-title-ordering".to_string(),
            AgentHostEventPolicy::Suppress,
        );
        let run_id = "permission-mode-title-ordering".to_string();
        *state.inner.lock().await = Some(AgentRuntime {
            agent_manager,
            session_manager: Arc::clone(&session_manager),
            maple_api_session: crate::maple_api::test_maple_api_session(user_id),
            active_runs: HashMap::from([(
                run_id,
                ActiveAgentRun {
                    agent,
                    permission_routing: AgentPermissionRouting::Desktop,
                    token: CancellationToken::new(),
                    tool_context: SharedAgentToolContext::new(AgentToolContextSpec::default()),
                    session_id: session.id.clone(),
                    events: run_events,
                    cancelled_permission_ids: Arc::new(Mutex::new(HashSet::new())),
                    accepting_queue: Arc::new(AtomicBool::new(true)),
                    steered_unacked: Arc::new(Mutex::new(Vec::new())),
                    task_handle: tokio::spawn(async {}),
                },
            )]),
            session_title_tasks: HashMap::new(),
            session_tool_contexts: HashMap::new(),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            web_tool_state: Arc::new(WebToolState::default()),
            project_root,
            model: DEFAULT_AGENT_MODEL.to_string(),
            mode: DEFAULT_GOOSE_MODE.to_string(),
            account_scope: account_scope.clone(),
            lifetime: CancellationToken::new(),
        });

        let session_title_lifecycle = resolve_session_title_lifecycle(
            &state.session_title_lifecycles,
            &account_scope,
            &session.id,
        )
        .await;
        let session_title_lifecycle_guard = session_title_lifecycle.lock().await;
        let handle = state.handle_for_user(user_id).await.unwrap();
        let mode_session_id = session.id.clone();
        let mode_task = tokio::spawn(async move {
            handle
                .set_permission_mode(AgentPermissionModeRequest {
                    session_id: mode_session_id,
                    mode: GooseMode::Auto.to_string(),
                })
                .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if session_manager
                    .get_session(&session.id, false)
                    .await
                    .is_ok_and(|persisted| persisted.goose_mode == GooseMode::Auto)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the permission mode should persist before its summary waits");
        assert!(!mode_task.is_finished());

        session_manager
            .update(&session.id)
            .system_generated_name("Semantic title".to_string())
            .apply()
            .await
            .unwrap();
        let semantic_session = session_manager
            .get_session(&session.id, false)
            .await
            .unwrap();
        emit_agent_event(
            &state.host.events,
            AgentServiceEvent::SessionUpdated {
                session_id: session.id.clone(),
                run_id: None,
                session: session_summary(&semantic_session),
            },
        );
        drop(session_title_lifecycle_guard);
        mode_task.await.unwrap().unwrap();

        let emitted_summaries = sink
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|event| match event {
                AgentServiceEvent::SessionUpdated { session, .. } => {
                    Some((session.title.clone(), session.mode.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            emitted_summaries,
            vec![
                ("Semantic title".to_string(), "auto".to_string()),
                ("Semantic title".to_string(), "auto".to_string()),
            ]
        );

        let runtime = state.inner.lock().await.take();
        drop(runtime);
        drop(session_manager);
        drop(state);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn abandoned_first_send_restores_new_task_for_retry() {
        let test_root = recent_roots_test_dir("abandoned-session-title");
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(SessionManager::new(test_root.join("sessions")));
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root,
                DEFAULT_AGENT_SESSION_TITLE.to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        session_manager
            .update(&session.id)
            .system_generated_name("hey how are you?".to_string())
            .apply()
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));
        agent
            .update_provider(
                Arc::new(SessionTitleTestProvider {
                    calls: Arc::clone(&calls),
                    response: "Friendly Check-In",
                }),
                ModelConfig::new(DEFAULT_AGENT_MODEL),
                &session.id,
            )
            .await
            .unwrap();

        let sink = Arc::new(RecordingAgentEventSink::default());
        let (start, start_rx) = oneshot::channel();
        let (settled, settled_rx) = oneshot::channel();
        let title_task = tokio::spawn(run_agent_session_title_task(AgentSessionTitleJob {
            start: start_rx,
            settled: settled_rx,
            session_manager: Arc::clone(&session_manager),
            agent: Arc::clone(&agent),
            session_lifecycle: Arc::new(Mutex::new(())),
            session_title_lifecycle: Arc::new(Mutex::new(())),
            session_id: session.id.clone(),
            first_prompt: "hey how are you?".to_string(),
            expected_fallback_title: "hey how are you?".to_string(),
            cancel_token: CancellationToken::new(),
            dispatcher: AgentEventDispatcher::new(sink.clone()),
            host_events: AgentHostEventPolicy::Publish,
        }));
        drop(start);
        settled.send(()).unwrap();
        title_task.await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            session_manager
                .get_session(&session.id, false)
                .await
                .unwrap()
                .name,
            DEFAULT_AGENT_SESSION_TITLE
        );
        assert!(
            sink.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|event| matches!(
                    event,
                    AgentServiceEvent::SessionUpdated { session, .. }
                        if session.title == DEFAULT_AGENT_SESSION_TITLE
                ))
        );

        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn aborted_first_send_restores_new_task_without_settling() {
        let test_root = recent_roots_test_dir("aborted-session-title");
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(SessionManager::new(test_root.join("sessions")));
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root,
                DEFAULT_AGENT_SESSION_TITLE.to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        session_manager
            .update(&session.id)
            .system_generated_name("hey how are you?".to_string())
            .apply()
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));
        agent
            .update_provider(
                Arc::new(SessionTitleTestProvider {
                    calls: Arc::clone(&calls),
                    response: "Friendly Check-In",
                }),
                ModelConfig::new(DEFAULT_AGENT_MODEL),
                &session.id,
            )
            .await
            .unwrap();

        let sink = Arc::new(RecordingAgentEventSink::default());
        let (start, start_rx) = oneshot::channel();
        let (settled, settled_rx) = oneshot::channel();
        let title_task = tokio::spawn(run_agent_session_title_task(AgentSessionTitleJob {
            start: start_rx,
            settled: settled_rx,
            session_manager: Arc::clone(&session_manager),
            agent: Arc::clone(&agent),
            session_lifecycle: Arc::new(Mutex::new(())),
            session_title_lifecycle: Arc::new(Mutex::new(())),
            session_id: session.id.clone(),
            first_prompt: "hey how are you?".to_string(),
            expected_fallback_title: "hey how are you?".to_string(),
            cancel_token: CancellationToken::new(),
            dispatcher: AgentEventDispatcher::new(sink.clone()),
            host_events: AgentHostEventPolicy::Publish,
        }));
        drop(start);
        // The run task was aborted before it could settle: the sender is
        // dropped, never sent. The fallback title must still be restored.
        drop(settled);
        title_task.await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            session_manager
                .get_session(&session.id, false)
                .await
                .unwrap()
                .name,
            DEFAULT_AGENT_SESSION_TITLE
        );
        assert!(
            sink.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|event| matches!(
                    event,
                    AgentServiceEvent::SessionUpdated { session, .. }
                        if session.title == DEFAULT_AGENT_SESSION_TITLE
                ))
        );

        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn persisted_first_message_is_titled_when_reply_fails_before_stream_start() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(SessionTitleTestProvider {
            calls: Arc::clone(&calls),
            response: "Friendly Check-In",
        });
        let (test_root, session_manager, agent, session) =
            session_title_test_context("persisted-message-closed-title-start", provider).await;
        let agent = Arc::new(agent);
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (start, start_rx) = oneshot::channel();
        let (settled, settled_rx) = oneshot::channel();
        let title_task = tokio::spawn(run_agent_session_title_task(AgentSessionTitleJob {
            start: start_rx,
            settled: settled_rx,
            session_manager: Arc::clone(&session_manager),
            agent: Arc::clone(&agent),
            session_lifecycle: Arc::new(Mutex::new(())),
            session_title_lifecycle: Arc::new(Mutex::new(())),
            session_id: session.id.clone(),
            first_prompt: "hey how are you?".to_string(),
            expected_fallback_title: "Explain reactive titles".to_string(),
            cancel_token: CancellationToken::new(),
            dispatcher: AgentEventDispatcher::new(sink.clone()),
            host_events: AgentHostEventPolicy::Publish,
        }));
        drop(start);
        settled.send(()).unwrap();
        title_task.await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            session_manager
                .get_session(&session.id, false)
                .await
                .unwrap()
                .name,
            "Friendly Check-In"
        );
        assert!(
            sink.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|event| matches!(
                    event,
                    AgentServiceEvent::SessionUpdated { session, .. }
                        if session.title == "Friendly Check-In"
                ))
        );

        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn cancel_before_goose_persists_prompt_settles_before_semantic_title() {
        let test_root = recent_roots_test_dir("cancel-before-title-start");
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = Arc::new(SessionManager::new(test_root.join("sessions")));
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root,
                DEFAULT_AGENT_SESSION_TITLE.to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        session_manager
            .update(&session.id)
            .system_generated_name("hey how are you?".to_string())
            .apply()
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = Arc::new(Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        )));
        agent
            .update_provider(
                Arc::new(SessionTitleTestProvider {
                    calls: Arc::clone(&calls),
                    response: "Friendly Check-In",
                }),
                ModelConfig::new(DEFAULT_AGENT_MODEL),
                &session.id,
            )
            .await
            .unwrap();

        let session_lifecycle = Arc::new(Mutex::new(()));
        let session_lifecycle_guard = session_lifecycle.lock().await;
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (start, start_rx) = oneshot::channel();
        let (settled, settled_rx) = oneshot::channel();
        let title_task = tokio::spawn(run_agent_session_title_task(AgentSessionTitleJob {
            start: start_rx,
            settled: settled_rx,
            session_manager: Arc::clone(&session_manager),
            agent: Arc::clone(&agent),
            session_lifecycle: Arc::clone(&session_lifecycle),
            session_title_lifecycle: Arc::new(Mutex::new(())),
            session_id: session.id.clone(),
            first_prompt: "hey how are you?".to_string(),
            expected_fallback_title: "hey how are you?".to_string(),
            cancel_token: CancellationToken::new(),
            dispatcher: AgentEventDispatcher::new(sink.clone()),
            host_events: AgentHostEventPolicy::Publish,
        }));
        drop(start);

        let user_message = Message::user()
            .with_text("hey how are you?")
            .with_generated_id();
        finalize_cancelled_agent_turn(
            session_manager.as_ref(),
            &Arc::new(Mutex::new(HashMap::new())),
            &WebToolState::default(),
            &session.id,
            AgentPermissionRouting::Desktop,
            &user_message,
            &HashSet::new(),
        )
        .await
        .unwrap();
        settled.send(()).unwrap();
        drop(session_lifecycle_guard);
        title_task.await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let reloaded = session_manager
            .get_session(&session.id, true)
            .await
            .unwrap();
        assert_eq!(reloaded.name, "Friendly Check-In");
        assert!(reloaded.conversation.as_ref().is_some_and(|conversation| {
            conversation
                .messages()
                .iter()
                .any(|message| message.id == user_message.id)
        }));
        assert!(
            sink.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|event| matches!(
                    event,
                    AgentServiceEvent::SessionUpdated { session, .. }
                        if session.title == "Friendly Check-In"
                ))
        );

        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn tracked_independent_session_title_is_cancelled_and_drained_without_late_update() {
        let title_started = Arc::new(tokio::sync::Notify::new());
        let title_release = Arc::new(tokio::sync::Notify::new());
        let title_cancel_token = CancellationToken::new();
        let provider = Arc::new(DelayedSessionTitleProvider {
            title_started: Arc::clone(&title_started),
            title_release,
            title_cancel_token: title_cancel_token.clone(),
        });
        let (test_root, session_manager, agent, session) =
            session_title_test_context("cancelled-independent-session-title", provider).await;
        let agent = Arc::new(agent);
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (start, start_rx) = oneshot::channel();
        let (_settled, settled_rx) = oneshot::channel();
        let title_task = tokio::spawn(run_agent_session_title_task(AgentSessionTitleJob {
            start: start_rx,
            settled: settled_rx,
            session_manager: Arc::clone(&session_manager),
            agent: Arc::clone(&agent),
            session_lifecycle: Arc::new(Mutex::new(())),
            session_title_lifecycle: Arc::new(Mutex::new(())),
            session_id: session.id.clone(),
            first_prompt: "Explain how reactive Agent titles work".to_string(),
            expected_fallback_title: "Explain reactive titles".to_string(),
            cancel_token: title_cancel_token.clone(),
            dispatcher: AgentEventDispatcher::new(sink.clone()),
            host_events: AgentHostEventPolicy::Publish,
        }));

        start.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), title_started.notified())
            .await
            .expect("the title request should start before cancellation");
        let tracked_title_task = ActiveAgentSessionTitleTask {
            run_id: "cancelled-independent-session-title-run".to_string(),
            token: title_cancel_token,
            task_handle: title_task,
        };
        tracked_title_task.token.cancel();
        // Runtime shutdown takes the same token + handle pair from its title
        // registry and joins it before dropping the account-scoped runtime.
        join_agent_tasks(
            vec![tracked_title_task.task_handle],
            std::time::Duration::from_secs(1),
        )
        .await;

        assert_eq!(
            session_manager
                .get_session(&session.id, false)
                .await
                .unwrap()
                .name,
            "Explain reactive titles"
        );
        assert!(
            sink.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );

        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn session_deletion_drains_independent_title_before_removing_persisted_session() {
        let title_started = Arc::new(tokio::sync::Notify::new());
        let title_release = Arc::new(tokio::sync::Notify::new());
        let title_cancel_token = CancellationToken::new();
        let provider = Arc::new(DelayedSessionTitleProvider {
            title_started: Arc::clone(&title_started),
            title_release: Arc::clone(&title_release),
            title_cancel_token: title_cancel_token.clone(),
        });
        let (test_root, session_manager, agent, session) =
            session_title_test_context("delete-independent-session-title", provider).await;
        let agent = Arc::new(agent);
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (start, start_rx) = oneshot::channel();
        let (_settled, settled_rx) = oneshot::channel();
        let title_task = tokio::spawn(run_agent_session_title_task(AgentSessionTitleJob {
            start: start_rx,
            settled: settled_rx,
            session_manager: Arc::clone(&session_manager),
            agent: Arc::clone(&agent),
            session_lifecycle: Arc::new(Mutex::new(())),
            session_title_lifecycle: Arc::new(Mutex::new(())),
            session_id: session.id.clone(),
            first_prompt: "Explain how reactive Agent titles work".to_string(),
            expected_fallback_title: "Explain reactive titles".to_string(),
            cancel_token: title_cancel_token.clone(),
            dispatcher: AgentEventDispatcher::new(sink.clone()),
            host_events: AgentHostEventPolicy::Publish,
        }));

        start.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), title_started.notified())
            .await
            .expect("the title request should start before deletion");

        let tracked_title_task = ActiveAgentSessionTitleTask {
            run_id: "delete-independent-session-title-run".to_string(),
            token: title_cancel_token,
            task_handle: title_task,
        };
        tracked_title_task.token.cancel();
        // Session deletion performs this drain while holding its lifecycle
        // guard, before it removes the Goose row and Maple-owned session state.
        join_agent_tasks(
            vec![tracked_title_task.task_handle],
            std::time::Duration::from_secs(1),
        )
        .await;

        let pending_permissions = Arc::new(Mutex::new(HashMap::new()));
        let live_timelines = Arc::new(Mutex::new(HashMap::new()));
        delete_persisted_agent_session(
            session_manager.as_ref(),
            &pending_permissions,
            &live_timelines,
            &Arc::new(Mutex::new(HashMap::new())),
            None,
            &session.id,
        )
        .await
        .expect("the session should be deleted after its title task drains");

        // Releasing the fake provider cannot produce a late write or event: the
        // task was joined before deletion returned.
        title_release.notify_waiters();
        tokio::task::yield_now().await;
        assert!(
            session_manager
                .get_session(&session.id, false)
                .await
                .is_err()
        );
        assert!(
            sink.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );

        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn manual_title_set_during_generation_wins_over_semantic_title() {
        let title_started = Arc::new(tokio::sync::Notify::new());
        let title_release = Arc::new(tokio::sync::Notify::new());
        let title_cancel_token = CancellationToken::new();
        let provider = Arc::new(DelayedSessionTitleProvider {
            title_started: Arc::clone(&title_started),
            title_release: Arc::clone(&title_release),
            title_cancel_token: title_cancel_token.clone(),
        });
        let sink = Arc::new(RecordingAgentEventSink::default());
        let (test_root, paths, state) =
            agent_service_test_context("manual-title-wins", sink.clone());
        let user_id = "manual-title-user";
        let project_root = test_root.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let session_manager = account_session_manager(&paths, user_id).unwrap();
        let permission_manager = Arc::new(PermissionManager::new(test_root.join("permissions")));
        let session = session_manager
            .create_session(
                project_root,
                DEFAULT_AGENT_SESSION_TITLE.to_string(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let agent = Agent::with_config(GooseAgentConfig::new(
            Arc::clone(&session_manager),
            permission_manager,
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        ));
        agent
            .update_provider(provider, ModelConfig::new(DEFAULT_AGENT_MODEL), &session.id)
            .await
            .unwrap();
        session_manager
            .update(&session.id)
            .system_generated_name("Explain reactive titles".to_string())
            .apply()
            .await
            .unwrap();
        session_manager
            .add_message(
                &session.id,
                &Message::user().with_text("Explain how reactive Agent titles work"),
            )
            .await
            .unwrap();
        let agent = Arc::new(agent);
        let generation_session_manager = Arc::clone(&session_manager);
        let generation_agent = Arc::clone(&agent);
        let generation_session_id = session.id.clone();
        let generation_session_lifecycle = Arc::clone(&state.session_lifecycle);
        let generation_session_title_lifecycle = resolve_session_title_lifecycle(
            &state.session_title_lifecycles,
            &account_scope(user_id).unwrap(),
            &session.id,
        )
        .await;
        let generation = tokio::spawn(async move {
            generate_agent_session_title(AgentSessionTitleGeneration {
                session_manager: generation_session_manager.as_ref(),
                agent: generation_agent.as_ref(),
                session_lifecycle: &generation_session_lifecycle,
                session_title_lifecycle: &generation_session_title_lifecycle,
                session_id: &generation_session_id,
                first_prompt: "Explain how reactive Agent titles work",
                expected_fallback_title: "Explain reactive titles",
                cancel_token: &title_cancel_token,
                event_target: None,
            })
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), title_started.notified())
            .await
            .expect("title inference should start");
        let summary = state
            .handle_for_user(user_id)
            .await
            .unwrap()
            .rename_session(
                crate::maple_api::test_maple_api_session(user_id),
                AgentRenameSessionRequest {
                    session_id: session.id.clone(),
                    title: " My Manual Title ".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(summary.title, "My Manual Title");
        title_release.notify_one();

        assert!(generation.await.unwrap().unwrap().is_none());
        let reloaded = session_manager
            .get_session(&session.id, false)
            .await
            .unwrap();
        assert_eq!(reloaded.name, "My Manual Title");
        assert!(reloaded.user_set_name);
        let events = sink
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentServiceEvent::SessionUpdated {
                session_id,
                run_id: None,
                session,
            } if session_id == &reloaded.id && session.title == "My Manual Title"
        ));

        drop(agent);
        drop(session_manager);
        drop(state);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn session_title_uses_bounded_non_thinking_llama_request() {
        let capture = Arc::new(std::sync::Mutex::new(None));
        let provider = Arc::new(CapturingSessionTitleProvider {
            capture: Arc::clone(&capture),
        });
        let (test_root, session_manager, agent, session) =
            session_title_test_context("bounded-llama-session-title", provider).await;
        let first_prompt = format!("{}TAIL", "a".repeat(SESSION_TITLE_MAX_INPUT_CHARS));

        let summary = generate_agent_session_title(AgentSessionTitleGeneration {
            session_manager: session_manager.as_ref(),
            agent: &agent,
            session_lifecycle: &Arc::new(Mutex::new(())),
            session_title_lifecycle: &Arc::new(Mutex::new(())),
            session_id: &session.id,
            first_prompt: &first_prompt,
            expected_fallback_title: "Explain reactive titles",
            cancel_token: &CancellationToken::new(),
            event_target: None,
        })
        .await
        .unwrap()
        .expect("the captured title should be persisted");

        assert_eq!(summary.title, "Bounded Llama Title");
        let capture = capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("the provider should capture one title request");
        assert_eq!(capture.model_name, SESSION_TITLE_MODEL);
        assert_eq!(capture.temperature, Some(SESSION_TITLE_TEMPERATURE));
        assert_eq!(capture.max_tokens, Some(SESSION_TITLE_MAX_TOKENS));
        assert_eq!(capture.reasoning, Some(false));
        assert!(!capture.request_params_present);
        assert_eq!(capture.system, SESSION_TITLE_SYSTEM_PROMPT);
        assert!(
            capture
                .user
                .contains(&"a".repeat(SESSION_TITLE_MAX_INPUT_CHARS))
        );
        assert!(!capture.user.contains("TAIL"));
        assert_eq!(capture.tool_count, 0);

        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn llama_generated_session_title_is_persisted_for_maple() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(SessionTitleTestProvider {
            calls: Arc::clone(&calls),
            response: "Reactive Agent Titles",
        });
        let (test_root, session_manager, agent, session) =
            session_title_test_context("generated-session-title", provider).await;

        let summary = generate_agent_session_title(AgentSessionTitleGeneration {
            session_manager: session_manager.as_ref(),
            agent: &agent,
            session_lifecycle: &Arc::new(Mutex::new(())),
            session_title_lifecycle: &Arc::new(Mutex::new(())),
            session_id: &session.id,
            first_prompt: "Explain how reactive Agent titles work",
            expected_fallback_title: "Explain reactive titles",
            cancel_token: &CancellationToken::new(),
            event_target: None,
        })
        .await
        .unwrap()
        .expect("the first user message should generate a title");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(summary.title, "Reactive Agent Titles");
        assert_eq!(
            session_manager
                .get_session(&session.id, false)
                .await
                .unwrap()
                .name,
            "Reactive Agent Titles"
        );

        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn empty_generated_session_title_keeps_maple_fallback() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(SessionTitleTestProvider {
            calls: Arc::clone(&calls),
            response: "<think>discarded</think>   ",
        });
        let (test_root, session_manager, agent, session) =
            session_title_test_context("empty-session-title", provider).await;

        let summary = generate_agent_session_title(AgentSessionTitleGeneration {
            session_manager: session_manager.as_ref(),
            agent: &agent,
            session_lifecycle: &Arc::new(Mutex::new(())),
            session_title_lifecycle: &Arc::new(Mutex::new(())),
            session_id: &session.id,
            first_prompt: "Explain how reactive Agent titles work",
            expected_fallback_title: "Explain reactive titles",
            cancel_token: &CancellationToken::new(),
            event_target: None,
        })
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(summary.is_none());
        assert_eq!(
            session_manager
                .get_session(&session.id, false)
                .await
                .unwrap()
                .name,
            "Explain reactive titles"
        );

        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[tokio::test]
    async fn timed_out_session_title_generation_is_cancelled_drained_and_keeps_fallback() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let title_cancel_token = CancellationToken::new();
        let provider = Arc::new(BlockingSessionTitleProvider {
            cancel_token: title_cancel_token.clone(),
            cancelled: Arc::clone(&cancelled),
        });
        let (test_root, session_manager, agent, session) =
            session_title_test_context("timed-out-session-title", provider).await;

        let error = generate_agent_session_title(AgentSessionTitleGeneration {
            session_manager: session_manager.as_ref(),
            agent: &agent,
            session_lifecycle: &Arc::new(Mutex::new(())),
            session_title_lifecycle: &Arc::new(Mutex::new(())),
            session_id: &session.id,
            first_prompt: "Explain how reactive Agent titles work",
            expected_fallback_title: "Explain reactive titles",
            cancel_token: &title_cancel_token,
            event_target: None,
        })
        .await
        .unwrap_err();

        assert!(error.contains("timed out"));
        assert!(cancelled.load(Ordering::SeqCst));
        assert_eq!(
            session_manager
                .get_session(&session.id, false)
                .await
                .unwrap()
                .name,
            "Explain reactive titles"
        );

        drop(agent);
        drop(session_manager);
        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn permission_extraction_rejects_empty_and_conflicting_ids() {
        let status_arguments = serde_json::Map::from_iter([(
            "command".to_string(),
            Value::String("git status --short".to_string()),
        )]);
        let push_arguments = serde_json::Map::from_iter([(
            "command".to_string(),
            Value::String("git push".to_string()),
        )]);
        let message = Message::assistant()
            .with_content(MessageContent::action_required(
                "request-1",
                "shell".to_string(),
                status_arguments,
                None,
            ))
            .with_content(MessageContent::action_required(
                "request-1",
                "shell".to_string(),
                push_arguments,
                None,
            ))
            .with_content(MessageContent::action_required(
                "",
                "shell".to_string(),
                serde_json::Map::new(),
                None,
            ));

        let extracted = tool_permission_requests(&message);

        assert!(extracted.requests.is_empty());
        assert_eq!(
            extracted.conflicting_ids,
            HashSet::from(["request-1".to_string(), String::new()])
        );
    }

    #[test]
    fn permission_extraction_rejects_identical_duplicate_ids() {
        let arguments = serde_json::Map::from_iter([(
            "command".to_string(),
            Value::String("git status --short".to_string()),
        )]);
        let message = Message::assistant()
            .with_content(MessageContent::action_required(
                "request-1",
                "shell".to_string(),
                arguments.clone(),
                None,
            ))
            .with_content(MessageContent::action_required(
                "request-1",
                "shell".to_string(),
                arguments,
                None,
            ));

        let extracted = tool_permission_requests(&message);

        assert!(extracted.requests.is_empty());
        assert_eq!(
            extracted.conflicting_ids,
            HashSet::from(["request-1".to_string()])
        );
    }

    #[tokio::test]
    async fn cancelled_permission_is_not_registered() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let issued = Arc::new(Mutex::new(HashSet::new()));
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();

        assert_eq!(
            register_pending_permission(
                &pending,
                &issued,
                "session-1",
                "run-1",
                AgentPermissionRouting::Desktop,
                test_permission_request("request-1"),
                &cancel_token,
            )
            .await,
            PendingPermissionRegistration::Rejected
        );
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn pending_permissions_are_taken_only_for_the_exact_run() {
        let pending = Arc::new(Mutex::new(HashMap::from([
            (
                ("session-1".to_string(), "request-1".to_string()),
                test_pending_permission("run-1", AgentPermissionRouting::Desktop, "request-1"),
            ),
            (
                ("session-1".to_string(), "request-2".to_string()),
                test_pending_permission(
                    "run-2",
                    AgentPermissionRouting::CallingSurface,
                    "request-2",
                ),
            ),
        ])));

        let selected = take_pending_permissions_for_runs(&pending, &["run-1".to_string()]).await;

        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected[0].0,
            ("session-1".to_string(), "request-1".to_string())
        );
        assert_eq!(selected[0].1.run_id, "run-1");
        let remaining = pending.lock().await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining.values().next().unwrap().run_id, "run-2");
    }

    #[tokio::test]
    async fn conflicting_permission_registration_invalidates_the_stale_capability() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let issued = Arc::new(Mutex::new(HashSet::new()));
        let cancel_token = CancellationToken::new();
        let original = test_permission_request("request-1");
        assert_eq!(
            register_pending_permission(
                &pending,
                &issued,
                "session-1",
                "run-1",
                AgentPermissionRouting::CallingSurface,
                original.clone(),
                &cancel_token,
            )
            .await,
            PendingPermissionRegistration::Registered
        );
        assert_eq!(
            register_pending_permission(
                &pending,
                &issued,
                "session-1",
                "run-1",
                AgentPermissionRouting::CallingSurface,
                original,
                &cancel_token,
            )
            .await,
            PendingPermissionRegistration::Existing
        );

        let mut conflicting = test_permission_request("request-1");
        conflicting
            .arguments
            .insert("command".to_string(), Value::String("git push".to_string()));
        assert_eq!(
            register_pending_permission(
                &pending,
                &issued,
                "session-1",
                "run-1",
                AgentPermissionRouting::CallingSurface,
                conflicting,
                &cancel_token,
            )
            .await,
            PendingPermissionRegistration::Rejected
        );
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn resolved_permission_ids_cannot_be_reissued_within_a_run() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let issued = Arc::new(Mutex::new(HashSet::new()));
        let cancel_token = CancellationToken::new();
        assert_eq!(
            register_pending_permission(
                &pending,
                &issued,
                "session-1",
                "run-1",
                AgentPermissionRouting::Desktop,
                test_permission_request("request-1"),
                &cancel_token,
            )
            .await,
            PendingPermissionRegistration::Registered
        );
        assert_eq!(
            take_pending_permissions_for_runs(&pending, &["run-1".to_string()])
                .await
                .len(),
            1
        );

        let mut reused = test_permission_request("request-1");
        reused
            .arguments
            .insert("command".to_string(), Value::String("git push".to_string()));
        assert_eq!(
            register_pending_permission(
                &pending,
                &issued,
                "session-1",
                "run-1",
                AgentPermissionRouting::Desktop,
                reused,
                &cancel_token,
            )
            .await,
            PendingPermissionRegistration::Rejected
        );
        assert!(pending.lock().await.is_empty());
    }

    #[test]
    fn coalesces_tool_request_and_response_for_loaded_sessions() {
        let request = AgentTimelineItem {
            id: "functions.shell:7".to_string(),
            item_type: "tool".to_string(),
            role: Some("assistant".to_string()),
            title: Some("shell".to_string()),
            text: Some("listing project root".to_string()),
            status: Some("running".to_string()),
            input: Some(json!({ "command": "ls -la" })),
            output: None,
            created_ms: 1000,
            merge: "replace".to_string(),
        };
        let response = AgentTimelineItem {
            id: "functions.shell:7".to_string(),
            item_type: "tool".to_string(),
            role: Some("assistant".to_string()),
            title: None,
            text: None,
            status: Some("completed".to_string()),
            input: None,
            output: Some(json!({ "text": "ok" })),
            created_ms: 2000,
            merge: "replace".to_string(),
        };

        let items = coalesce_timeline_items(vec![request, response]);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "functions.shell:7");
        assert_eq!(items[0].title.as_deref(), Some("shell"));
        assert_eq!(items[0].text.as_deref(), Some("listing project root"));
        assert_eq!(items[0].status.as_deref(), Some("completed"));
        assert_eq!(items[0].input, Some(json!({ "command": "ls -la" })));
        assert_eq!(items[0].output, Some(json!({ "text": "ok" })));
    }

    #[test]
    fn tool_error_preserves_request_title_for_provider_generated_id() {
        let id = "chatcmpl-tool-123";
        let request = AgentTimelineItem {
            id: id.to_string(),
            item_type: "tool".to_string(),
            role: Some("assistant".to_string()),
            title: Some("shell".to_string()),
            text: None,
            status: Some("running".to_string()),
            input: Some(json!({ "command": "false" })),
            output: None,
            created_ms: 1000,
            merge: "replace".to_string(),
        };
        let response = goose::conversation::message::ToolResponse {
            id: id.to_string(),
            tool_result: Ok(rmcp::model::CallToolResult::error(vec![
                rmcp::model::ContentBlock::text("command failed"),
            ])),
            metadata: None,
        };
        let response = tool_response_item(&response, 2000);
        assert_eq!(response.status.as_deref(), Some("failed"));
        assert!(response.title.is_none());

        let merged = coalesce_timeline_items(vec![request, response]);
        assert_eq!(merged[0].title.as_deref(), Some("shell"));
        assert_eq!(merged[0].status.as_deref(), Some("failed"));
    }
    #[test]
    fn action_required_permission_cards_carry_descriptive_titles() {
        let edit_arguments: JsonObject = serde_json::from_value(serde_json::json!({
            "path": "/tmp/notes.md",
            "edits": [{ "oldText": "foo", "newText": "bar" }]
        }))
        .unwrap();
        let edit =
            MessageContent::action_required("request-1", "edit".to_string(), edit_arguments, None);
        let MessageContent::ActionRequired(action) = edit else {
            unreachable!("action_required builds an ActionRequired message");
        };
        let item = super::timeline::action_required_item(&action, 1)
            .expect("a tool confirmation becomes a permission card");
        assert_eq!(
            item.title.as_deref(),
            Some("edit: /tmp/notes.md"),
            "the card must say what is being edited"
        );
        assert_eq!(item.status.as_deref(), Some("pending"));
        assert_eq!(item.item_type, "permission");

        let shell_arguments: JsonObject =
            serde_json::from_value(serde_json::json!({ "command": "ls -la" })).unwrap();
        let bare = MessageContent::action_required(
            "request-2",
            "developer__shell".to_string(),
            shell_arguments,
            None,
        );
        let MessageContent::ActionRequired(action) = bare else {
            unreachable!();
        };
        let item = super::timeline::action_required_item(&action, 2)
            .expect("a tool confirmation becomes a permission card");
        assert_eq!(item.title.as_deref(), Some("Terminal: ls -la"));
    }

    #[test]
    fn system_notification_omits_structured_data_and_bounds_message() {
        let notification = SystemNotificationContent {
            notification_type: SystemNotificationType::InlineMessage,
            msg: "x".repeat(600),
            data: Some(json!({ "raw": "must-not-render" })),
        };

        let item = system_notification_item("message", 0, &notification, 1000);

        assert_eq!(item.title.as_deref(), Some("Agent notice"));
        assert_eq!(item.text.as_ref().unwrap().chars().count(), 501);
        assert!(item.text.as_ref().unwrap().ends_with('…'));
        assert!(item.output.is_none());
    }

    #[test]
    fn progress_notification_has_stable_title() {
        let notification = SystemNotificationContent {
            notification_type: SystemNotificationType::ProgressMessage,
            msg: "Loading...".to_string(),
            data: None,
        };

        let item = system_notification_item("message", 0, &notification, 1000);

        assert_eq!(item.title.as_deref(), Some("Progress"));
        assert_eq!(item.text.as_deref(), Some("Loading..."));
    }

    #[test]
    fn timeline_text_is_bounded_by_characters() {
        assert_eq!(bounded_timeline_text("éclair", 2), "éc…");
        assert_eq!(bounded_timeline_text("short", 10), "short");
    }
}
