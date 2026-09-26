//! The host boundary: everything a client drives on a host.
//!
//! A host runs the agent runtime. A client is the desktop app. Every app
//! instance is its own local host, and it can also be a client of remote
//! hosts. The [`HostBackend`] trait is the surface a client calls; the
//! [`HostEvent`] stream is what a host pushes back. [`LocalHostBackend`]
//! implements the trait in process over [`crate::agent::AgentRuntimeHandle`].
//! A remote implementation speaks the same trait over the wire, so the UI
//! never branches on where a host runs.
//!
//! Account-level concerns (sign-in, billing, audio) are not part of this
//! surface: they use the client's own OpenSecret session and stay local.

pub mod local;

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::agent::{
    AgentCreateSessionRequest, AgentDesktopQueueSnapshot, AgentEventSink, AgentIntegration,
    AgentMcpServer, AgentProjectRootRegistration, AgentProjectTrustStatus, AgentRuntimeStatus,
    AgentSendMessageRequest, AgentServiceEvent, AgentSessionDetail, AgentSessionIntegrationKind,
    AgentSessionMcpServer, AgentSessionSummary, AgentSlashCommand, AgentStartRequest,
    AgentSubagent, AgentTaskState, RecentProjectRoot, SideQuestionTurn,
};

pub use local::{LegacySessionDefaults, LocalHostAuth, LocalHostBackend};

/// Identifies a host on the client. The local host is [`HostId::local`];
/// a remote host is identified by its static public key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(String);

impl HostId {
    /// The id every client uses for its own in-process host.
    pub const LOCAL: &'static str = "local";

    pub fn local() -> Self {
        Self(Self::LOCAL.to_string())
    }

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_local(&self) -> bool {
        self.0 == Self::LOCAL
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything a host pushes to its clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HostEvent {
    /// An event from the agent runtime. Boxed: it is far larger than the
    /// other variants, and every event is cloned once per subscriber.
    Service(Box<AgentServiceEvent>),
    /// The git branch of a watched project root, sent when a watch starts
    /// and whenever `HEAD` changes. `None` when the root is not a checkout.
    ProjectBranch {
        project_root: String,
        branch: Option<String>,
    },
    /// Events may have been missed: a remote connection saw a gap in the
    /// host's sequence, or reconnected. The client re-reads the task list
    /// and reloads any task it shows. The local host never sends this.
    Resync,
}

/// Opening system prompt text used when a host has none saved.
pub const DEFAULT_HARNESS_INSTRUCTIONS: &str =
    "You are a general-purpose AI agent called Maple, created by Maple AI.
You run in the Maple app's Agent Mode; users know you simply as Maple.";

// The permission policy names are the runtime's; the host speaks them on
// the wire unchanged.
pub use crate::agent::{PERMISSION_MODE_AUTO, PERMISSION_MODE_SMART_APPROVE};

/// Defaults a host applies to the tasks its clients create. Stored in the
/// host's per-account config, so two hosts can differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionDefaults {
    /// `smart_approve` or `auto`.
    pub permission_mode: String,
    /// Whether new tasks can use the web tools.
    pub web_enabled: bool,
    /// Opening system prompt text. Empty means
    /// [`DEFAULT_HARNESS_INSTRUCTIONS`].
    pub harness_instructions: String,
    /// The account's saved default model, if any. Read-only here: the
    /// chat screen saves it through [`HostBackend::save_default_model`],
    /// and [`HostBackend::set_session_defaults`] leaves it alone so a
    /// stale settings snapshot cannot put an old model back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

impl Default for HostSessionDefaults {
    fn default() -> Self {
        Self {
            permission_mode: PERMISSION_MODE_SMART_APPROVE.to_string(),
            web_enabled: true,
            harness_instructions: String::new(),
            default_model: None,
        }
    }
}

impl HostSessionDefaults {
    /// The harness instructions to hand the runtime: the saved text, or
    /// the default when nothing is saved.
    pub fn effective_harness_instructions(&self) -> String {
        effective_harness_instructions(&self.harness_instructions)
    }
}

/// The harness instructions for a saved value: the text, or the default
/// when it is blank.
pub fn effective_harness_instructions(saved: &str) -> String {
    let saved = saved.trim();
    if saved.is_empty() {
        DEFAULT_HARNESS_INSTRUCTIONS.to_string()
    } else {
        saved.to_string()
    }
}

/// Everything a client can show for a host before any network call: the
/// saved project root, the task list, the recent roots, the newest task's
/// transcript, and the session defaults. Read in one call so it all lands
/// before a runtime start takes the lifecycle lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostBootstrap {
    pub project_root: Option<String>,
    pub sessions: Vec<AgentSessionSummary>,
    pub recent_roots: Vec<String>,
    pub latest: Option<AgentSessionDetail>,
    pub session_defaults: HostSessionDefaults,
}

/// Context window use for one task, from the host's usage ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub tokens: i64,
    pub limit: i64,
}

/// One directory a typed root could mean; see
/// [`HostBackend::suggest_directories`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorySuggestion {
    /// Absolute path.
    pub path: String,
    /// Last path component, for display.
    pub name: String,
}

/// One aggregated usage row: per session or per model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub label: String,
    pub sessions: u64,
    pub turns: u64,
    pub total_tokens: i64,
    pub cost: f64,
}

/// The account's usage ledger, aggregated; see
/// [`HostBackend::usage_summary`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    pub totals: UsageRow,
    pub by_model: Vec<UsageRow>,
    pub by_session: Vec<UsageRow>,
}

/// Fans one host's events out to every subscriber. The runtime's event
/// sink for the local host; a server projects the same stream to its
/// sockets. Subscribers that dropped their receiver are pruned on the
/// next publish.
#[derive(Default)]
pub struct HostEventHub {
    subscribers: Mutex<Vec<mpsc::UnboundedSender<HostEvent>>>,
}

impl HostEventHub {
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(tx);
        rx
    }

    pub fn publish(&self, event: HostEvent) {
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
    }

    #[cfg(test)]
    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl AgentEventSink for HostEventHub {
    fn emit(&self, event: &AgentServiceEvent) {
        self.publish(HostEvent::Service(Box::new(event.clone())));
    }
}

/// What a client drives on one host. Every method is scoped to the one
/// account the host is signed in as.
#[async_trait]
pub trait HostBackend: Send + Sync + 'static {
    fn id(&self) -> &HostId;

    /// A fresh stream of this host's events. Several subscribers may be
    /// live at once; each receives every event.
    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent>;

    // ---- Runtime -----------------------------------------------------------

    async fn bootstrap(&self) -> Result<HostBootstrap, String>;
    async fn start_runtime(
        &self,
        request: Option<AgentStartRequest>,
    ) -> Result<AgentRuntimeStatus, String>;
    async fn stop_runtime(&self) -> Result<AgentRuntimeStatus, String>;

    // ---- Projects ----------------------------------------------------------

    async fn recent_project_roots(&self) -> Result<Vec<RecentProjectRoot>, String>;
    async fn select_project_root(
        &self,
        path: String,
    ) -> Result<AgentProjectRootRegistration, String>;
    async fn remove_project_root(
        &self,
        path: String,
        fallback: Option<String>,
    ) -> Result<(), String>;
    /// Directories on the host that complete `query`, for a typed root.
    async fn suggest_directories(&self, query: String) -> Result<Vec<DirectorySuggestion>, String>;
    /// Start reporting the git branch of `path` through
    /// [`HostEvent::ProjectBranch`]; the first report follows at once.
    async fn watch_project_root(&self, path: String) -> Result<(), String>;
    async fn unwatch_project_root(&self, path: String) -> Result<(), String>;
    async fn project_trust(&self, path: String) -> Result<AgentProjectTrustStatus, String>;
    async fn set_project_trust(
        &self,
        path: String,
        trusted: bool,
    ) -> Result<AgentProjectTrustStatus, String>;

    // ---- Sessions ----------------------------------------------------------

    async fn list_sessions(
        &self,
        project_root: Option<String>,
    ) -> Result<Vec<AgentSessionSummary>, String>;
    async fn create_session(
        &self,
        request: Option<AgentCreateSessionRequest>,
    ) -> Result<AgentSessionDetail, String>;
    async fn load_session(&self, session_id: String) -> Result<AgentSessionDetail, String>;
    async fn rename_session(
        &self,
        session_id: String,
        title: String,
    ) -> Result<AgentSessionSummary, String>;
    /// Move a task between active, settled, and archived. The runtime
    /// owns the state and answers with the updated record.
    async fn set_session_state(
        &self,
        session_id: String,
        state: AgentTaskState,
    ) -> Result<AgentSessionSummary, String>;
    /// Delete a task for good.
    async fn delete_session(&self, session_id: String) -> Result<(), String>;
    async fn compact_session(&self, session_id: String) -> Result<(), String>;
    async fn session_subagents(&self, session_id: String) -> Result<Vec<AgentSubagent>, String>;
    async fn cancel_external_agent(
        &self,
        session_id: String,
        agent_id: String,
    ) -> Result<(), String>;
    async fn set_permission_mode(&self, session_id: String, mode: String) -> Result<(), String>;
    async fn set_session_web_enabled(
        &self,
        session_id: String,
        enabled: bool,
    ) -> Result<AgentSessionSummary, String>;
    async fn context_usage(
        &self,
        session_id: String,
        model: Option<String>,
    ) -> Result<Option<ContextUsage>, String>;
    async fn read_image_attachment(
        &self,
        session_id: String,
        attachment_id: String,
    ) -> Result<Vec<u8>, String>;

    // ---- Messages and runs -------------------------------------------------

    /// Returns the run id.
    async fn send_message(&self, request: AgentSendMessageRequest) -> Result<String, String>;
    async fn cancel_run(&self, run_id: String) -> Result<(), String>;
    async fn cancel_queued_message(
        &self,
        session_id: String,
        queue_id: String,
    ) -> Result<AgentDesktopQueueSnapshot, String>;
    async fn begin_queued_message_edit(
        &self,
        session_id: String,
        queue_id: String,
    ) -> Result<(), String>;
    async fn end_queued_message_edit(
        &self,
        session_id: String,
        queue_id: String,
    ) -> Result<(), String>;
    /// Returns false when no question was pending.
    async fn answer_question(&self, request_id: String, answer: String) -> Result<bool, String>;
    async fn permission_respond(
        &self,
        session_id: String,
        request_id: String,
        allow: bool,
    ) -> Result<(), String>;
    async fn ask_side_question(
        &self,
        session_id: String,
        request_id: String,
        prior: Vec<SideQuestionTurn>,
        question: String,
    ) -> Result<(), String>;

    // ---- Summaries ---------------------------------------------------------

    async fn summarize_tool_call(
        &self,
        session_id: String,
        tool_name: String,
        input: Option<serde_json::Value>,
        output_text: String,
    ) -> Result<Option<String>, String>;
    async fn summarize_thinking(
        &self,
        session_id: String,
        thinking_text: String,
    ) -> Result<Option<String>, String>;
    /// Stored summaries for one session, keyed by timeline item id.
    async fn tool_summaries(&self, session_id: String) -> Result<HashMap<String, String>, String>;
    async fn store_tool_summary(
        &self,
        session_id: String,
        item_id: String,
        summary: String,
    ) -> Result<(), String>;

    // ---- Models and skills -------------------------------------------------

    async fn available_model_ids(&self) -> Result<Vec<String>, String>;
    async fn model_supports_vision(&self, model: String) -> Result<Option<bool>, String>;
    async fn list_slash_commands(
        &self,
        working_dir: Option<String>,
    ) -> Result<Vec<AgentSlashCommand>, String>;
    async fn resolve_slash_command(
        &self,
        working_dir: Option<String>,
        command: String,
        args: String,
    ) -> Result<Option<String>, String>;

    // ---- Integrations and MCP ----------------------------------------------

    async fn list_session_mcp_servers(
        &self,
        session_id: String,
    ) -> Result<Vec<AgentSessionMcpServer>, String>;
    async fn set_session_mcp_server_enabled(
        &self,
        session_id: String,
        name: String,
        kind: AgentSessionIntegrationKind,
        enabled: bool,
    ) -> Result<Vec<AgentSessionMcpServer>, String>;
    async fn list_mcp_servers(&self) -> Result<Vec<AgentMcpServer>, String>;
    async fn save_mcp_servers(
        &self,
        servers: Vec<AgentMcpServer>,
    ) -> Result<Vec<AgentMcpServer>, String>;
    async fn list_integrations(&self) -> Result<Vec<AgentIntegration>, String>;
    async fn set_integration_enabled(
        &self,
        id: String,
        enabled: bool,
    ) -> Result<Vec<AgentIntegration>, String>;
    /// Persist a curated integration after its permission flow ran on the
    /// host. The flow itself is a local capability the client starts from
    /// its own UI thread; a remote client cannot run it.
    async fn setup_integration(&self, id: String) -> Result<Vec<AgentIntegration>, String>;

    // ---- Host configuration ------------------------------------------------

    async fn session_defaults(&self) -> Result<HostSessionDefaults, String>;
    async fn set_session_defaults(&self, defaults: HostSessionDefaults) -> Result<(), String>;
    async fn save_default_model(&self, model: String) -> Result<(), String>;
    async fn usage_summary(&self) -> Result<UsageSummary, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_delivers_to_every_live_subscriber_and_prunes_dropped_ones() {
        let hub = HostEventHub::default();
        let mut first = hub.subscribe();
        let second = hub.subscribe();
        drop(second);
        hub.publish(HostEvent::ProjectBranch {
            project_root: "/p".to_string(),
            branch: Some("main".to_string()),
        });
        assert!(matches!(
            first.try_recv(),
            Ok(HostEvent::ProjectBranch { branch: Some(branch), .. }) if branch == "main"
        ));
        assert_eq!(hub.subscriber_count(), 1);
    }

    #[test]
    fn host_ids_round_trip_and_know_local() {
        assert!(HostId::local().is_local());
        assert!(!HostId::new("abc").is_local());
        let json = serde_json::to_string(&HostId::new("abc")).unwrap();
        assert_eq!(json, "\"abc\"");
        assert_eq!(
            serde_json::from_str::<HostId>(&json).unwrap().as_str(),
            "abc"
        );
    }

    #[test]
    fn blank_harness_instructions_mean_the_default() {
        assert_eq!(
            effective_harness_instructions("  \n"),
            DEFAULT_HARNESS_INSTRUCTIONS
        );
        assert_eq!(effective_harness_instructions(" custom "), "custom");
        let defaults = HostSessionDefaults::default();
        assert_eq!(defaults.permission_mode, PERMISSION_MODE_SMART_APPROVE);
        assert!(defaults.web_enabled);
    }
}
