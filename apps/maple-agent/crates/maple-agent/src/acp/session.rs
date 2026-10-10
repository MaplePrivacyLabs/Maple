//! ACP connection and session state: what one connection holds, what one
//! session owns, and the checks a session must pass before it exists.

use super::config::{AgentAcpConfig, AgentAcpStats};
use super::convert::acp_session_config_options;
use super::transport::AcpOutboundTracker;
use crate::agent::{
    AgentMcpKeyValue, AgentRunCancellation, AgentRuntimeHandle, AgentSessionSummary,
    AgentToolContextLease, AgentToolContextSpec, AgentTransientMcpServer,
    AgentTransientMcpTransport, SENSITIVE_BRIDGE_ENV,
};

use agent_client_protocol::JsonRpcNotification;
use agent_client_protocol::schema::v1::{McpServer, SessionConfigOption, SessionId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

pub(super) const ACP_TRANSIENT_MCP_TIMEOUT_SECONDS: u64 = 30;
pub(super) const ALLOWED_BRIDGE_ENV: [&str; 6] = [
    "BUZZ_RELAY_URL",
    "BUZZ_PRIVATE_KEY",
    "BUZZ_AUTH_TAG",
    "BUZZ_API_TOKEN",
    "BUZZ_ACP_DISPLAY_NAME",
    "PATH",
];
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "_maple/bridge/hello")]
pub(super) struct BridgeHelloNotification {
    pub(super) environment: HashMap<String, String>,
}

/// The (shared) runtime start `session/new` and `session/load` wait on, so
/// the ACP handshake can answer before the heavy runtime boots. Driving it
/// through `Shared` means the first session request starts the runtime once
/// while later requests await the same start.
pub(super) type SharedRuntimeStart = futures_util::future::Shared<
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>,
>;

/// A start gate that is already satisfied; used when the runtime is known
/// to be running (tests, desktop-owned runs).
#[cfg(test)]
pub(super) fn completed_runtime_start() -> SharedRuntimeStart {
    futures_util::FutureExt::shared(Box::pin(std::future::ready(Ok(()))))
}

pub(super) struct AcpConnectionContext {
    pub(super) agent: AgentRuntimeHandle,
    pub(super) runtime_start: SharedRuntimeStart,
    pub(super) config: Arc<RwLock<AgentAcpConfig>>,
    pub(super) stats: Arc<AgentAcpStats>,
    pub(super) bridge_environment: Mutex<HashMap<String, String>>,
    pub(super) sessions: Mutex<HashMap<String, AcpSession>>,
    pub(super) session_operations: Mutex<HashMap<String, Arc<AcpSessionOperation>>>,
    pub(super) closing_sessions: Mutex<HashSet<String>>,
    pub(super) prompt_states: Mutex<HashMap<String, AcpPromptState>>,
    pub(super) background_tasks: Mutex<tokio::task::JoinSet<()>>,
    pub(super) finalization: Mutex<()>,
    pub(super) lifetime: CancellationToken,
    pub(super) closed: AtomicBool,
    pub(super) has_credentials: AtomicBool,
    pub(super) client_supports_form_elicitation: AtomicBool,
    pub(super) outbound: Arc<AcpOutboundTracker>,
}

pub(super) struct AcpSession {
    pub(super) lease: Option<AgentToolContextLease>,
    pub(super) model: String,
    pub(super) available_models: Vec<String>,
    /// Cached context window of `model` for `usage_update` notifications.
    /// `None` until resolved (or after a model switch invalidates it).
    pub(super) context_limit: Option<u64>,
    /// The session title last advertised to the caller; semantic-title
    /// updates are only sent when the title moves past this.
    pub(super) advertised_title: Option<String>,
    pub(super) message_count: usize,
    pub(super) created_here: bool,
    pub(super) prompted: bool,
    pub(super) project_root: PathBuf,
    pub(super) project_trust_decision: Option<bool>,
}

pub(super) struct UnpublishedAcpSession {
    pub(super) lease: Option<AgentToolContextLease>,
    pub(super) published: bool,
}

impl UnpublishedAcpSession {
    pub(super) fn new(lease: AgentToolContextLease) -> Self {
        Self {
            lease: Some(lease),
            published: false,
        }
    }

    pub(super) fn publish(mut self) -> AgentToolContextLease {
        self.published = true;
        self.lease
            .take()
            .expect("an unpublished ACP session must still own its lease")
    }
}

impl Drop for UnpublishedAcpSession {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        let Some(lease) = self.lease.take() else {
            return;
        };
        lease.revoke();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                lease.discard_created_if_untouched().await;
            });
        }
    }
}

pub(super) struct AcpSessionOperation {
    pub(super) gate: Arc<Mutex<()>>,
    pub(super) cancellation: CancellationToken,
}

pub(super) fn close_registration_may_be_released(
    cancellation_completed: bool,
    operation_drained: bool,
    cleanup_completed: bool,
) -> bool {
    // A timed-out load may already have passed its final core cancellation
    // check. Keep both its exact operation registration and the closing
    // tombstone until connection teardown so it can never publish a lease
    // after session/close has returned. The same fence stays in place while
    // run cancellation or exact-match lease cleanup is still settling.
    cancellation_completed && operation_drained && cleanup_completed
}

impl AcpSessionOperation {
    pub(super) fn new(connection_lifetime: &CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            gate: Arc::new(Mutex::new(())),
            cancellation: connection_lifetime.child_token(),
        })
    }
}

impl AcpSession {
    pub(super) fn config_options(&self) -> Vec<SessionConfigOption> {
        acp_session_config_options(&self.model, &self.available_models, self.message_count)
    }
}

pub(super) enum AcpPromptState {
    Starting {
        cancellation: CancellationToken,
    },
    Running {
        cancellation: CancellationToken,
        run_cancellation: Box<AgentRunCancellation>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AcpProjectTrustResolution {
    Continue,
    Cancelled,
}
pub(super) fn ensure_allowed_project_root(
    cwd: &Path,
    allowed_roots: &[String],
) -> Result<PathBuf, String> {
    if !cwd.is_absolute() {
        return Err("ACP session cwd must be an absolute path".to_string());
    }
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("Failed to resolve ACP session cwd: {error}"))?;
    if allowed_roots.is_empty() {
        return Ok(cwd);
    }
    for root in allowed_roots {
        if let Ok(root) = Path::new(root).canonicalize()
            && cwd.starts_with(root)
        {
            return Ok(cwd);
        }
    }
    Err("ACP session cwd is outside the configured project roots".to_string())
}

pub(super) fn prepare_session_mcp(
    bridge_environment: &HashMap<String, String>,
    servers: &[McpServer],
) -> Result<(HashMap<String, String>, Vec<AgentTransientMcpServer>), agent_client_protocol::Error> {
    let mut environment = bridge_environment.clone();
    let mut transient = Vec::new();
    for server in servers {
        match server {
            McpServer::Stdio(server) => {
                let command = server.command.as_path();
                let is_buzz_dev_mcp = server.name == "buzz-dev-mcp"
                    && command
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name == "buzz-dev-mcp" || name == "buzz-dev-mcp.exe")
                    && server.args.is_empty();
                if is_buzz_dev_mcp {
                    if !command.is_absolute() {
                        return Err(agent_client_protocol::Error::invalid_params()
                            .data("Buzz buzz-dev-mcp command must be absolute"));
                    }
                    let metadata = std::fs::metadata(command).map_err(|_| {
                        agent_client_protocol::Error::invalid_params()
                            .data("Buzz buzz-dev-mcp command does not exist")
                    })?;
                    if !metadata.is_file() {
                        return Err(agent_client_protocol::Error::invalid_params()
                            .data("Buzz buzz-dev-mcp command is not a file"));
                    }
                    #[cfg(unix)]
                    if metadata.permissions().mode() & 0o111 == 0 {
                        return Err(agent_client_protocol::Error::invalid_params()
                            .data("Buzz buzz-dev-mcp command is not executable"));
                    }
                    // Preserve the historical Buzz adapter: its exact MCP
                    // definition contributes only the allowlisted shell env.
                    for variable in &server.env {
                        if !ALLOWED_BRIDGE_ENV.contains(&variable.name.as_str()) {
                            continue;
                        }
                        if let Some(existing) = environment.get(&variable.name) {
                            if existing != &variable.value {
                                return Err(agent_client_protocol::Error::invalid_params().data(
                                    format!(
                                        "Conflicting ACP environment value for {}",
                                        variable.name
                                    ),
                                ));
                            }
                        } else {
                            environment.insert(variable.name.clone(), variable.value.clone());
                        }
                    }
                    continue;
                }
                return Err(agent_client_protocol::Error::invalid_params().data(format!(
                    "Transient stdio MCP server '{}' is disabled because it would execute caller-supplied native code on this machine",
                    server.name
                )));
            }
            McpServer::Http(server) => {
                let mut headers = Vec::new();
                let mut header_names = HashSet::new();
                for header in &server.headers {
                    if !header_names.insert(header.name.to_ascii_lowercase()) {
                        return Err(agent_client_protocol::Error::invalid_params().data(format!(
                            "Duplicate HTTP header in MCP server '{}'",
                            server.name
                        )));
                    }
                    headers.push(AgentMcpKeyValue {
                        key: header.name.clone(),
                        value: header.value.clone(),
                    });
                }
                transient.push(AgentTransientMcpServer {
                    name: server.name.clone(),
                    description: "ACP session MCP server".to_string(),
                    timeout_seconds: ACP_TRANSIENT_MCP_TIMEOUT_SECONDS,
                    transport: AgentTransientMcpTransport::StreamableHttp {
                        url: server.url.clone(),
                        headers,
                    },
                });
            }
            McpServer::Sse(_) => {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("Maple ACP does not support legacy SSE MCP servers"));
            }
            _ => {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("Maple ACP does not support this MCP transport"));
            }
        }
    }
    Ok((environment, transient))
}

pub(super) fn filter_bridge_environment(
    environment: HashMap<String, String>,
) -> HashMap<String, String> {
    environment
        .into_iter()
        .filter(|(key, value)| {
            ALLOWED_BRIDGE_ENV.contains(&key.as_str())
                && !value.contains('\0')
                && value.len() <= 16 * 1024
        })
        .collect()
}

pub(super) fn bridge_tool_context_spec(
    environment: &HashMap<String, String>,
) -> Result<AgentToolContextSpec, String> {
    let values = environment
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    let scrub_from_parent = SENSITIVE_BRIDGE_ENV
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let ephemeral = SENSITIVE_BRIDGE_ENV
        .iter()
        .any(|key| environment.contains_key(*key));
    AgentToolContextSpec::try_new(values, scrub_from_parent, ephemeral)
}

pub(super) fn has_buzz_credentials(environment: &HashMap<String, String>) -> bool {
    environment
        .get("BUZZ_RELAY_URL")
        .is_some_and(|value| !value.is_empty())
        && environment
            .get("BUZZ_PRIVATE_KEY")
            .is_some_and(|value| !value.is_empty())
}

pub(super) fn canonical_session_id(
    session_id: &SessionId,
) -> Result<String, agent_client_protocol::Error> {
    canonical_session_id_text(&session_id.0)
}

pub(super) fn canonical_session_id_text(
    session_id: &str,
) -> Result<String, agent_client_protocol::Error> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("Maple ACP requires a non-empty session ID"));
    }
    Ok(session_id.to_string())
}

/// The persisted task a caller asked to load. Every task loads, whatever
/// mode it was saved in: all of them run with every tool call allowed.
pub(super) fn find_acp_session<'a>(
    sessions: &'a [AgentSessionSummary],
    session_id: &str,
) -> Result<&'a AgentSessionSummary, String> {
    sessions
        .iter()
        .find(|session| session.id == session_id)
        .ok_or_else(|| {
            "The requested Maple Agent task does not exist in the supplied project directory"
                .to_string()
        })
}
