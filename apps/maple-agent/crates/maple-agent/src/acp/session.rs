//! ACP connection and session state: what one connection holds, what one
//! session owns, and the checks a session must pass before it exists.

use super::SharedRuntimeStart;
use super::config::{AgentAcpConfig, AgentAcpStats};
use super::convert::acp_config_options;
use super::observer::ObserverMailbox;
use super::transport::AcpOutboundTracker;
#[cfg(test)]
use crate::agent::AgentSessionSummary;
use crate::agent::{
    AgentMcpKeyValue, AgentMcpServer, AgentMcpTransport, AgentRuntimeHandle, AgentSurfaceLease,
    CatalogEntry,
};

use agent_client_protocol::schema::v1::{McpServer, SessionConfigOption, SessionId};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

pub(super) const ACP_SESSION_MCP_TIMEOUT_SECONDS: u64 = 30;

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
    pub(super) sessions: Mutex<HashMap<String, AcpSession>>,
    pub(super) session_operations: Mutex<HashMap<String, Arc<AcpSessionOperation>>>,
    pub(super) observer_slots: Arc<tokio::sync::Semaphore>,
    pub(super) elicitation_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    pub(super) observer_dispatches: std::sync::atomic::AtomicUsize,
    pub(super) prompt_states: Mutex<HashMap<String, AcpPromptState>>,
    pub(super) owned_runs: Mutex<HashMap<String, String>>,
    pub(super) observing: AtomicBool,
    pub(super) eliciting: Mutex<HashSet<String>>,
    pub(super) question_responder: Mutex<Option<crate::agent::QuestionResponder>>,
    pub(super) background_tasks: Mutex<tokio::task::JoinSet<()>>,
    pub(super) finalization: Mutex<()>,
    pub(super) lifetime: CancellationToken,
    pub(super) closed: AtomicBool,
    pub(super) client_supports_form_elicitation: AtomicBool,
    pub(super) outbound: Arc<AcpOutboundTracker>,
}

pub(super) struct AcpSession {
    pub(super) lease: Option<AgentSurfaceLease>,
    pub(super) model: String,
    pub(super) available_models: Vec<String>,
    /// What the catalog says about `model`: its vision and context window
    /// for each turn and for `usage_update`. `None` until the catalog is
    /// read, and again after a model switch.
    pub(super) catalog: Option<CatalogEntry>,
    pub(super) thinking: Option<crate::agent::AgentThinkingState>,
    pub(super) projection: super::convert::AcpProjection,
    /// The session title last advertised to the caller; semantic-title
    /// updates are only sent when the title moves past this.
    pub(super) advertised_title: Option<String>,
    pub(super) created_here: bool,
    pub(super) prompted: bool,
    pub(super) project_root: PathBuf,
    pub(super) project_trust_decision: Option<bool>,
}

/// A session's lease until the session is published to the connection.
/// Dropped before that, it discards the task it created.
pub(super) struct UnpublishedAcpSession {
    pub(super) lease: Option<AgentSurfaceLease>,
    pub(super) published: bool,
}

impl UnpublishedAcpSession {
    pub(super) fn new(lease: AgentSurfaceLease) -> Self {
        Self {
            lease: Some(lease),
            published: false,
        }
    }

    pub(super) fn publish(mut self) -> AgentSurfaceLease {
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
    /// Live observers wait for initial history replay, never for a whole turn.
    pub(super) replay_gate: Arc<Mutex<()>>,
    pub(super) observer: ObserverMailbox,
    pub(super) replaying: AtomicBool,
    /// Replacement events can wait in the connection's broadcast receiver
    /// before this operation is registered. Only a newer epoch invalidates it.
    pub(super) history_epoch: i64,
    pub(super) cancellation: CancellationToken,
}

impl AcpSessionOperation {
    pub(super) fn new(connection_lifetime: &CancellationToken, history_epoch: i64) -> Arc<Self> {
        Arc::new(Self {
            gate: Arc::new(Mutex::new(())),
            replay_gate: Arc::new(Mutex::new(())),
            observer: ObserverMailbox::new(),
            replaying: AtomicBool::new(true),
            history_epoch,
            cancellation: connection_lifetime.child_token(),
        })
    }
}

impl AcpSession {
    pub(super) fn config_options(&self) -> Vec<SessionConfigOption> {
        let mut options = acp_config_options(&self.model, &self.available_models);
        if let Some(state) = &self.thinking {
            super::convert::add_thinking_option(&mut options, state);
        }
        options
    }
}

/// A session's prompt in progress. Its cancellation stops the prompt from
/// its admission on, and the run once it started.
pub(super) struct AcpPromptState {
    pub(super) cancellation: CancellationToken,
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

/// The caller's MCP servers for a session: HTTP servers, which run beside the
/// task's own. Stdio servers are refused before anything starts: they would
/// run caller-supplied native code here.
pub(super) fn prepare_session_mcp(
    servers: &[McpServer],
) -> Result<Vec<AgentMcpServer>, agent_client_protocol::Error> {
    let mut session_servers = Vec::new();
    for server in servers {
        match server {
            McpServer::Stdio(server) => {
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
                session_servers.push(AgentMcpServer {
                    name: server.name.clone(),
                    description: "ACP session MCP server".to_string(),
                    enabled: true,
                    timeout_seconds: ACP_SESSION_MCP_TIMEOUT_SECONDS,
                    transport: AgentMcpTransport::StreamableHttp {
                        url: server.url.clone(),
                        environment: Vec::new(),
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
    Ok(session_servers)
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

/// The task a caller asked to load. Every task loads: all of them run with
/// every tool call allowed.
#[cfg(test)]
pub(super) fn find_acp_session<'a>(
    sessions: &'a [AgentSessionSummary],
    session_id: &str,
) -> Result<&'a AgentSessionSummary, String> {
    sessions
        .iter()
        .find(|session| session.id == session_id)
        .ok_or_else(|| {
            "The requested Maple Work task does not exist in the supplied project directory"
                .to_string()
        })
}
