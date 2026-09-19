//! External coding agents a task can hand work to.
//!
//! A task delegates through `agent_start`, `agent_send`, `agent_status`,
//! `agent_cancel`, and `list_agent_providers`. Each external agent is one
//! child process owned by the Maple session that started it. Goose stays
//! the engine; the external agent runs under its own configuration, its
//! approval requests come to the user through Maple's own permission card,
//! and its progress streams into the transcript row of the tool call that
//! started the turn.
//!
//! Codex uses its app-server; Claude Code uses Goose’s native SDK protocol implementation.
//! Both feed the same activity, permission, and lifecycle host.

mod app_server;
pub(crate) mod claude;
pub(crate) mod codex;
#[cfg(test)]
mod tests;

use super::developer_tools::{
    ArmedShellChild, build_external_agent_command, spawn_contained, text_result,
};
use super::image_mediation::error_result;
use super::tool_context::AgentToolContextSnapshot;
use super::*;
use app_server::{AgentClient, AppServerClient, RequestMethod, ServerMessage};
use codex::{CodexEvent, CodexItem, CodexServerRequest};
use goose::conversation::message::SystemNotificationContent;
use std::fmt::Write as _;
use std::process::Stdio;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;

pub(crate) const AGENT_START_TOOL: &str = "agent_start";
pub(crate) const AGENT_SEND_TOOL: &str = "agent_send";
pub(crate) const AGENT_STATUS_TOOL: &str = "agent_status";
pub(crate) const AGENT_CANCEL_TOOL: &str = "agent_cancel";
pub(crate) const LIST_AGENT_PROVIDERS_TOOL: &str = "list_agent_providers";
/// Every tool this module adds, in catalog order.
pub(crate) const EXTERNAL_AGENT_TOOLS: [&str; 5] = [
    AGENT_START_TOOL,
    AGENT_SEND_TOOL,
    AGENT_STATUS_TOOL,
    AGENT_CANCEL_TOOL,
    LIST_AGENT_PROVIDERS_TOOL,
];
/// The key under which a tool result and a persisted notice carry the
/// activity payload the transcript renders.
pub const ACTIVITY_KEY: &str = "mapleExternalAgent";
const NOTICE_ROW_KEY: &str = "rowId";
const NOTICE_RESULT_KEY: &str = "resultText";

const MAX_AGENTS_PER_SESSION: usize = 4;
/// What the model reads back: the agent's last message, cut like Paseo cuts it.
const MAX_RESULT_TEXT_CHARS: usize = 4_000;
const MAX_ACTIVITY_COMMANDS: usize = 20;
const MAX_ACTIVITY_FILE_CHANGES: usize = 40;
const MAX_ACTIVITY_TODOS: usize = 20;
const MAX_ACTIVITY_COMMAND_CHARS: usize = 200;
const MAX_PROMPT_LABEL_CHARS: usize = MAX_AGENT_SESSION_TITLE_CHARS;
/// Streamed text repaints the transcript row at most this often.
const LIVE_ROW_INTERVAL: Duration = Duration::from_millis(150);
/// How long a cancelled blocking call waits for the agent to confirm the
/// interrupt before it reports back to the model.
const INTERRUPT_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const INTERRUPT_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// What one external agent has done, bounded so it fits a transcript row
/// and a persisted notice.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalAgentActivity {
    pub provider: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// `running`, `completed`, `failed`, `cancelled`, or `idle`.
    pub status: String,
    /// The agent's messages in the current turn, tail-kept.
    pub text: String,
    #[serde(default)]
    pub commands: Vec<ActivityCommand>,
    #[serde(default)]
    pub file_changes: Vec<ActivityFileChange>,
    #[serde(default)]
    pub todos: Vec<ActivityTodo>,
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What the agent is waiting on the user for, while it waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_permission: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityCommand {
    pub id: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// `running`, `completed`, or `failed`.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityFileChange {
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityTodo {
    pub text: String,
    pub completed: bool,
}

impl ExternalAgentActivity {
    fn record_command_started(&mut self, id: String, command: String) {
        let command = bounded_timeline_text(&command, MAX_ACTIVITY_COMMAND_CHARS);
        if let Some(existing) = self.commands.iter_mut().find(|entry| entry.id == id) {
            existing.command = command;
            return;
        }
        self.commands.push(ActivityCommand {
            id,
            command,
            exit_code: None,
            status: "running".to_string(),
        });
        let overflow = self.commands.len().saturating_sub(MAX_ACTIVITY_COMMANDS);
        if overflow > 0 {
            self.commands.drain(..overflow);
        }
    }

    fn record_command_finished(
        &mut self,
        id: &str,
        command: String,
        exit_code: Option<i64>,
        status: Option<String>,
    ) {
        let status = match (status.as_deref(), exit_code) {
            (Some("failed" | "error"), _) => "failed",
            (_, Some(code)) if code != 0 => "failed",
            _ => "completed",
        }
        .to_string();
        match self.commands.iter_mut().find(|entry| entry.id == id) {
            Some(existing) => {
                existing.exit_code = exit_code;
                existing.status = status;
            }
            None => {
                self.record_command_started(id.to_string(), command);
                if let Some(existing) = self.commands.last_mut() {
                    existing.exit_code = exit_code;
                    existing.status = status;
                }
            }
        }
    }

    fn record_file_changes(&mut self, changes: Vec<codex::FileChangeEntry>) {
        for change in changes {
            if let Some(existing) = self
                .file_changes
                .iter_mut()
                .find(|entry| entry.path == change.path)
            {
                existing.kind = change.kind;
                continue;
            }
            self.file_changes.push(ActivityFileChange {
                path: change.path,
                kind: change.kind,
            });
        }
        let overflow = self
            .file_changes
            .len()
            .saturating_sub(MAX_ACTIVITY_FILE_CHANGES);
        if overflow > 0 {
            self.file_changes.drain(..overflow);
        }
    }

    fn record_todos(&mut self, items: Vec<codex::TodoEntry>) {
        self.todos = items
            .into_iter()
            .take(MAX_ACTIVITY_TODOS)
            .map(|item| ActivityTodo {
                text: item.text,
                completed: item.completed,
            })
            .collect();
    }

    fn begin_turn(&mut self) {
        self.status = "running".to_string();
        self.text.clear();
        self.error = None;
        self.pending_permission = None;
        self.turns = self.turns.saturating_add(1);
    }
}

/// The model-facing summary of one agent's state.
fn render_activity(activity: &ExternalAgentActivity, guidance: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Status: {}", activity.status);
    let _ = writeln!(out, "Provider: {}", activity.provider);
    let _ = writeln!(out, "Agent ID: {}", activity.agent_id);
    if let Some(thread_id) = &activity.thread_id {
        let _ = writeln!(out, "Thread ID: {thread_id}");
    }
    if let Some(error) = &activity.error {
        let _ = writeln!(out, "Error: {error}");
    }
    if let Some(pending) = &activity.pending_permission {
        let _ = writeln!(out, "Waiting for the user to decide: {pending}");
    }
    if !activity.file_changes.is_empty() {
        let paths = activity
            .file_changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "Files changed ({}): {paths}",
            activity.file_changes.len()
        );
    }
    if !activity.commands.is_empty() {
        let failed = activity
            .commands
            .iter()
            .filter(|command| command.status == "failed")
            .count();
        let _ = writeln!(
            out,
            "Commands run: {} ({failed} failed)",
            activity.commands.len()
        );
    }
    out.push_str("\n<agent-response>\n");
    out.push_str(&bounded_timeline_text(
        activity.text.trim(),
        MAX_RESULT_TEXT_CHARS,
    ));
    out.push_str("\n</agent-response>\n\n");
    out.push_str(guidance);
    out
}

fn completion_guidance(activity: &ExternalAgentActivity) -> String {
    match activity.status.as_str() {
        "cancelled" => format!(
            "The turn was interrupted. Call {AGENT_SEND_TOOL} with this agent ID to continue the same thread."
        ),
        "failed" => format!(
            "The agent failed. Read its error, then call {AGENT_SEND_TOOL} with this agent ID to continue, or start a new agent."
        ),
        _ => format!(
            "Read the changed files yourself before relying on them. To give this agent more instructions with its context intact, call {AGENT_SEND_TOOL} with this agent ID."
        ),
    }
}

fn background_guidance() -> String {
    format!(
        "The agent works in the background. Maple will deliver its result when it finishes; continue with other work and do not poll {AGENT_STATUS_TOOL}. Use {AGENT_STATUS_TOOL} only when the user asks how it is going."
    )
}

/// What the transcript calls a call of one of these tools.
pub(super) fn tool_title(name: &str) -> Option<&'static str> {
    Some(match name {
        AGENT_START_TOOL => "External agent: start",
        AGENT_SEND_TOOL => "External agent: continue",
        AGENT_STATUS_TOOL => "External agent: status",
        AGENT_CANCEL_TOOL => "External agent: stop",
        LIST_AGENT_PROVIDERS_TOOL => "External agent: providers",
        _ => return None,
    })
}

/// The row status the desktop draws for a decision; its permission rows
/// know `completed`, `denied`, and `cancelled`.
fn decision_row_status(decision: AgentPermissionDecision) -> &'static str {
    match decision {
        AgentPermissionDecision::AllowOnce => "completed",
        AgentPermissionDecision::DenyOnce => "denied",
        AgentPermissionDecision::Cancel => "cancelled",
    }
}

/// Title of a row for a turn Maple started itself.
const SYNTHETIC_TURN_TITLE: &str = "External agent: answer delivered";

fn external_run_id(agent_id: &str) -> String {
    format!("external-{agent_id}")
}

fn subagent_row_id(agent_id: &str) -> String {
    format!("external-agent-{agent_id}")
}

/// What Maple needs from the runtime to host external agents.
#[derive(Clone)]
pub(super) struct ExternalAgentHost {
    pub(super) runtime: AgentRuntimeHandle,
    pub(super) service: MapleAgentService,
    pub(super) session_manager: Arc<SessionManager>,
    pub(super) permission_modes: SessionPermissionModes,
    pub(super) project_root: PathBuf,
    /// The runtime's lifetime; every agent's token derives from it.
    pub(super) lifetime: CancellationToken,
}

/// One tool call's view of its caller.
pub(crate) struct ExternalAgentCall {
    pub(crate) session_id: String,
    pub(crate) working_dir: Option<PathBuf>,
    /// The timeline row of the tool call, which the turn's progress joins.
    pub(crate) row_id: Option<String>,
    pub(crate) login_path: Option<String>,
    pub(crate) tool_context: AgentToolContextSnapshot,
    pub(crate) cancel_token: CancellationToken,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentStartParams {
    pub(crate) provider: String,
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) background: bool,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentSendParams {
    pub(crate) provider: String,
    pub(crate) agent_id: String,
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) background: bool,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentRefParams {
    pub(crate) provider: String,
    pub(crate) agent_id: String,
}

#[derive(Default)]
struct SessionAgents {
    agents: HashMap<String, Arc<ExternalAgent>>,
}

/// The external agents of every session of the running runtime.
pub(crate) struct ExternalAgentRegistry {
    host: ExternalAgentHost,
    sessions: Mutex<HashMap<String, SessionAgents>>,
    /// One-shot permission IDs, separate from Goose's so the two can
    /// never collide.
    issued_permission_ids: IssuedPermissionIds,
    next_agent: AtomicU64,
}

impl ExternalAgentRegistry {
    pub(super) fn new(host: ExternalAgentHost) -> Self {
        Self {
            host,
            sessions: Mutex::new(HashMap::new()),
            issued_permission_ids: Arc::new(Mutex::new(HashSet::new())),
            next_agent: AtomicU64::new(1),
        }
    }

    fn require_provider(provider: &str) -> Result<(), String> {
        if matches!(provider.trim(), codex::PROVIDER_ID | claude::PROVIDER_ID) {
            Ok(())
        } else {
            Err(format!(
                "Unknown agent provider '{}'. Call {LIST_AGENT_PROVIDERS_TOOL} to see what is installed.",
                provider.trim()
            ))
        }
    }

    async fn agent(&self, session_id: &str, agent_id: &str) -> Option<Arc<ExternalAgent>> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|session| session.agents.get(agent_id.trim()))
            .cloned()
    }

    fn resolve_cwd(
        &self,
        call: &ExternalAgentCall,
        requested: Option<&str>,
    ) -> Result<PathBuf, String> {
        let base = call
            .working_dir
            .clone()
            .unwrap_or_else(|| self.host.project_root.clone());
        let root = base.canonicalize().unwrap_or(base);
        let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(root);
        };
        let candidate = root.join(requested);
        let candidate = candidate
            .canonicalize()
            .map_err(|error| format!("cwd '{requested}' is not usable: {error}"))?;
        if !candidate.starts_with(&root) {
            return Err(format!(
                "cwd '{requested}' is outside the project root; external agents work inside the project only"
            ));
        }
        if !candidate.is_dir() {
            return Err(format!("cwd '{requested}' is not a directory"));
        }
        Ok(candidate)
    }

    pub(crate) async fn list_providers(
        &self,
        call: &ExternalAgentCall,
        providers: &[String],
    ) -> CallToolResult {
        if providers.is_empty() {
            return text_result("No external agent providers are enabled for this task.");
        }
        let mut out = String::from("Agent providers available to this task:\n");
        if providers
            .iter()
            .any(|provider| provider == claude::PROVIDER_ID)
        {
            let detection = claude::detect(call.login_path.as_deref()).await;
            let detail = if detection.executable.is_none() {
                "Install Claude Code and make sure `claude` is on PATH.".to_string()
            } else if let Some(problem) = detection.problem {
                problem
            } else {
                format!(
                    "{}; uses your Claude Code account. If sign-in is needed, run `claude auth login`. Supports model and effort overrides.",
                    detection.version.unwrap_or_default()
                )
            };
            let _ = writeln!(out, "- claude: {detail}");
        }
        if providers
            .iter()
            .any(|provider| provider == codex::PROVIDER_ID)
        {
            let detection = codex::detect(call.login_path.as_deref()).await;
            match (&detection.executable, &detection.problem) {
                (None, _) => {
                    let _ = writeln!(
                        out,
                        "- codex: not installed. Ask the user to install the Codex CLI and make sure `codex` is on PATH."
                    );
                }
                (Some(_), Some(problem)) => {
                    let _ = writeln!(out, "- codex: unusable. {problem}");
                }
                (Some(_), None) => {
                    let version = detection.version.as_deref().unwrap_or("unknown version");
                    let sign_in = match detection.signed_in {
                        Some(true) => "signed in".to_string(),
                        Some(false) => codex::sign_in_hint().to_string(),
                        None => "sign-in state unknown".to_string(),
                    };
                    let _ = writeln!(
                        out,
                        "- codex: {} {version}, {sign_in}. Runs `codex app-server` with the user's own Codex account and configuration; optional `model` and `effort` arguments override its defaults.",
                        codex::PROVIDER_NAME
                    );
                }
            }
        }
        let _ = writeln!(
            out,
            "\nStart one with {AGENT_START_TOOL}(provider, prompt). Write a self-contained briefing: the new agent has none of this conversation's context."
        );
        text_result(out)
    }

    pub(crate) async fn start(
        &self,
        call: ExternalAgentCall,
        params: AgentStartParams,
    ) -> CallToolResult {
        if let Err(error) = Self::require_provider(&params.provider) {
            return error_result(error);
        }
        let prompt = params.prompt.trim().to_string();
        if prompt.is_empty() {
            return error_result("prompt must not be empty");
        }
        let cwd = match self.resolve_cwd(&call, params.cwd.as_deref()) {
            Ok(cwd) => cwd,
            Err(error) => return error_result(error),
        };
        let agent = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.entry(call.session_id.clone()).or_default();
            if session.agents.len() >= MAX_AGENTS_PER_SESSION {
                return error_result(format!(
                    "This task already has {MAX_AGENTS_PER_SESSION} external agents. Reuse one with {AGENT_SEND_TOOL} or wait for one to finish."
                ));
            }
            let agent_id = format!(
                "{}-{}",
                params.provider.trim(),
                self.next_agent.fetch_add(1, Ordering::Relaxed)
            );
            let agent = Arc::new(ExternalAgent::new(
                params.provider.trim().to_string(),
                agent_id.clone(),
                call.session_id.clone(),
                first_line_label(&prompt),
                cwd,
                self.host.clone(),
                Arc::clone(&self.issued_permission_ids),
            ));
            session.agents.insert(agent_id, Arc::clone(&agent));
            agent
        };
        agent
            .run_turn(
                &call,
                TurnInput {
                    prompt,
                    background: params.background,
                    model: params.model,
                    effort: params.effort,
                },
            )
            .await
    }

    pub(crate) async fn send(
        &self,
        call: ExternalAgentCall,
        params: AgentSendParams,
    ) -> CallToolResult {
        if let Err(error) = Self::require_provider(&params.provider) {
            return error_result(error);
        }
        let prompt = params.prompt.trim().to_string();
        if prompt.is_empty() {
            return error_result("prompt must not be empty");
        }
        let Some(agent) = self.agent(&call.session_id, &params.agent_id).await else {
            return error_result(unknown_agent(&params.agent_id));
        };
        if agent.provider != params.provider.trim() {
            return error_result("This agent belongs to a different provider.");
        }
        agent
            .run_turn(
                &call,
                TurnInput {
                    prompt,
                    background: params.background,
                    model: params.model,
                    effort: params.effort,
                },
            )
            .await
    }

    pub(crate) async fn status(
        &self,
        call: &ExternalAgentCall,
        params: AgentRefParams,
    ) -> CallToolResult {
        if let Err(error) = Self::require_provider(&params.provider) {
            return error_result(error);
        }
        let Some(agent) = self.agent(&call.session_id, &params.agent_id).await else {
            return error_result(unknown_agent(&params.agent_id));
        };
        if agent.provider != params.provider.trim() {
            return error_result("This agent belongs to a different provider.");
        }
        let activity = agent.activity().await;
        let guidance = if activity.status == "running" {
            background_guidance()
        } else {
            completion_guidance(&activity)
        };
        let mut result = text_result(render_activity(&activity, &guidance));
        result.structured_content = Some(json!({ ACTIVITY_KEY: activity }));
        result
    }

    pub(crate) async fn cancel_tool(
        &self,
        call: &ExternalAgentCall,
        params: AgentRefParams,
    ) -> CallToolResult {
        if let Err(error) = Self::require_provider(&params.provider) {
            return error_result(error);
        }
        let Some(agent) = self.agent(&call.session_id, &params.agent_id).await else {
            return error_result(unknown_agent(&params.agent_id));
        };
        if agent.provider != params.provider.trim() {
            return error_result("This agent belongs to a different provider.");
        }
        match self.cancel(&call.session_id, &params.agent_id).await {
            Ok(activity) => {
                let mut result =
                    text_result(render_activity(&activity, &completion_guidance(&activity)));
                result.structured_content = Some(json!({ ACTIVITY_KEY: activity }));
                result
            }
            Err(error) => error_result(error),
        }
    }

    /// Stop the agent's current turn and its process. The agent stays
    /// resumable: the next send starts a fresh process on the same thread.
    pub(crate) async fn cancel(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<ExternalAgentActivity, String> {
        let Some(agent) = self.agent(session_id, agent_id).await else {
            return Err(unknown_agent(agent_id));
        };
        agent.stop_turn().await;
        Ok(agent.activity().await)
    }

    /// The agents of a task that are still working, for a caller that
    /// opens the task after the run that started them ended.
    pub(crate) async fn snapshot(&self, session_id: &str) -> Vec<AgentSubagent> {
        let agents = match self.sessions.lock().await.get(session_id) {
            Some(session) => session.agents.values().cloned().collect::<Vec<_>>(),
            None => return Vec::new(),
        };
        let mut rows = Vec::new();
        for agent in agents {
            if let Some(row) = agent.subagent_row().await {
                rows.push(row);
            }
        }
        rows.sort_by_key(|row| std::cmp::Reverse(row.elapsed_ms));
        rows
    }

    pub(crate) async fn shutdown_session(&self, session_id: &str) {
        let agents = self
            .sessions
            .lock()
            .await
            .remove(session_id)
            .map(|session| session.agents.into_values().collect::<Vec<_>>())
            .unwrap_or_default();
        for agent in agents {
            agent.shutdown().await;
        }
    }

    pub(crate) async fn shutdown_all(&self, graceful_timeout: Duration) {
        let agents = std::mem::take(&mut *self.sessions.lock().await)
            .into_values()
            .flat_map(|session| session.agents.into_values())
            .collect::<Vec<_>>();
        let shutdown = futures_util::future::join_all(agents.iter().map(|agent| agent.shutdown()));
        if tokio::time::timeout(graceful_timeout, shutdown)
            .await
            .is_err()
        {
            log::warn!(
                "External agents did not shut down in time; their process groups were killed"
            );
        }
    }
}

fn unknown_agent(agent_id: &str) -> String {
    format!(
        "No external agent '{}' in this task. Start one with {AGENT_START_TOOL}.",
        agent_id.trim()
    )
}

fn first_line_label(prompt: &str) -> String {
    let first_line = prompt.lines().next().unwrap_or(prompt).trim();
    bounded_timeline_text(first_line, MAX_PROMPT_LABEL_CHARS)
}

struct TurnInput {
    prompt: String,
    background: bool,
    model: Option<String>,
    effort: Option<String>,
}

/// The end of one turn, as the waiter reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl TurnOutcome {
    fn status(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

struct ActiveTurn {
    turn_id: Option<String>,
    row_id: String,
    /// The row belongs to no tool call: Maple started this turn itself to
    /// deliver the user's answers, so the row needs its own title.
    synthetic: bool,
    background: bool,
    done: Option<oneshot::Sender<TurnOutcome>>,
    /// Cancelled when the turn ends, so a request the agent left open
    /// (an approval, a question) stops waiting on the user.
    ended: CancellationToken,
}

/// Enough of a tool call to start a turn without one.
#[derive(Clone)]
struct StoredCall {
    working_dir: Option<PathBuf>,
    login_path: Option<String>,
    tool_context: AgentToolContextSnapshot,
}

struct AgentProcess {
    child: ArmedShellChild,
    client: Arc<AgentClient>,
    reader: tokio::task::JoinHandle<()>,
    events: tokio::task::JoinHandle<()>,
}

struct AgentState {
    process: Option<AgentProcess>,
    thread_id: Option<String>,
    turn: Option<ActiveTurn>,
    activity: ExternalAgentActivity,
    /// Streamed agent messages of the current turn, in order.
    messages: Vec<(String, String)>,
    /// Questions Codex asked without blocking that still wait on the user.
    open_questions: usize,
    /// What the last tool call gave, so an answer turn Maple starts on its
    /// own can spawn the process the same way.
    last_call: Option<StoredCall>,
    last_row_emit: Option<Instant>,
    /// The transcript row of the latest turn, for the notice that lands
    /// after the turn is gone.
    last_row_id: Option<String>,
}

struct ExternalAgent {
    provider: String,
    launch: Mutex<()>,
    agent_id: String,
    session_id: String,
    task: String,
    cwd: PathBuf,
    started: Instant,
    /// Ends the agent. Derived from the runtime lifetime so logout and
    /// Stop end it too.
    cancel: CancellationToken,
    host: ExternalAgentHost,
    issued_permission_ids: IssuedPermissionIds,
    state: Mutex<AgentState>,
}

impl ExternalAgent {
    fn provider_name(&self) -> &str {
        if self.provider == claude::PROVIDER_ID {
            claude::PROVIDER_NAME
        } else {
            codex::PROVIDER_NAME
        }
    }
    fn new(
        provider: String,
        agent_id: String,
        session_id: String,
        task: String,
        cwd: PathBuf,
        host: ExternalAgentHost,
        issued_permission_ids: IssuedPermissionIds,
    ) -> Self {
        let cancel = host.lifetime.child_token();
        Self {
            state: Mutex::new(AgentState {
                process: None,
                thread_id: None,
                turn: None,
                activity: ExternalAgentActivity {
                    provider: provider.clone(),
                    agent_id: agent_id.clone(),
                    status: "idle".to_string(),
                    ..Default::default()
                },
                messages: Vec::new(),
                open_questions: 0,
                last_call: None,
                last_row_emit: None,
                last_row_id: None,
            }),
            provider,
            launch: Mutex::new(()),
            agent_id,
            session_id,
            task,
            cwd,
            started: Instant::now(),
            cancel,
            host,
            issued_permission_ids,
        }
    }

    async fn activity(&self) -> ExternalAgentActivity {
        let mut state = self.state.lock().await;
        self.refresh_elapsed(&mut state);
        state.activity.clone()
    }

    fn refresh_elapsed(&self, state: &mut AgentState) {
        state.activity.elapsed_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    }

    async fn subagent_row(&self) -> Option<AgentSubagent> {
        let state = self.state.lock().await;
        let turn = state.turn.as_ref()?;
        Some(AgentSubagent {
            id: subagent_row_id(&self.agent_id),
            task: self.task.clone(),
            background: turn.background,
            elapsed_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            activity: latest_activity_label(&state.activity),
            external: Some(ExternalAgentRef {
                provider: self.provider.clone(),
                agent_id: self.agent_id.clone(),
            }),
        })
    }

    /// Run one turn: start the process if needed, send the prompt, and
    /// either wait for the end or hand the wait to a background task.
    async fn run_turn(
        self: &Arc<Self>,
        call: &ExternalAgentCall,
        input: TurnInput,
    ) -> CallToolResult {
        let launch = self.launch.lock().await;
        if self.cancel.is_cancelled() {
            return error_result("This external agent has been shut down.");
        }
        if self.state.lock().await.turn.is_some() {
            return error_result(format!(
                "Agent {} is still working on its previous turn. Wait for Maple's notice, or check with {AGENT_STATUS_TOOL}.",
                self.agent_id
            ));
        }
        let ready = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err("This external agent has been shut down.".to_string()),
            _ = call.cancel_token.cancelled() => Err("The external agent launch was cancelled.".to_string()),
            _ = call.tool_context.revoked.cancelled() => Err("The external agent context was revoked.".to_string()),
            result = tokio::time::timeout(Duration::from_secs(30), self.ensure_process(call, input.model.as_deref(), input.effort.as_deref())) =>
                result.unwrap_or_else(|_| Err("The external agent did not initialize in time.".to_string())),
        };
        if let Err(error) = ready {
            return error_result(error);
        }
        let (client, thread_id, done_rx) = {
            let mut state = self.state.lock().await;
            if state.turn.is_some() {
                return error_result(format!(
                    "Agent {} is still working on its previous turn. Wait for Maple's notice, or check with {AGENT_STATUS_TOOL}.",
                    self.agent_id
                ));
            }
            let Some(client) = state
                .process
                .as_ref()
                .map(|process| Arc::clone(&process.client))
            else {
                return error_result("The agent process is not running.");
            };
            let Some(thread_id) = state.thread_id.clone() else {
                return error_result("The agent has no thread.");
            };
            let (done_tx, done_rx) = oneshot::channel();
            let synthetic = call.row_id.is_none();
            let row_id = call.row_id.clone().unwrap_or_else(|| {
                format!(
                    "external-{}-turn-{}",
                    self.agent_id,
                    state.activity.turns + 1
                )
            });
            state.last_call = Some(StoredCall {
                working_dir: call.working_dir.clone(),
                login_path: call.login_path.clone(),
                tool_context: call.tool_context.clone(),
            });
            state.turn = Some(ActiveTurn {
                turn_id: None,
                row_id,
                synthetic,
                background: input.background,
                done: Some(done_tx),
                ended: CancellationToken::new(),
            });
            state.messages.clear();
            state.activity.begin_turn();
            (client, thread_id, done_rx)
        };
        log::info!("External agent {} turn starts", self.agent_id);
        self.emit_subagent_started(input.background);
        self.emit_row(true).await;

        let params = codex::turn_start_params(&codex::TurnRequest {
            thread_id: &thread_id,
            prompt: &input.prompt,
            cwd: &self.cwd,
            model: input.model.as_deref(),
            effort: input.effort.as_deref(),
        });
        if let Err(error) = client.request(RequestMethod::TurnStart, params).await {
            self.finish_turn(TurnOutcome::Failed, Some(error.clone()))
                .await;
            return error_result(error);
        }

        drop(launch);
        if input.background {
            let agent = Arc::clone(self);
            tokio::spawn(async move {
                let outcome = done_rx.await.unwrap_or(TurnOutcome::Cancelled);
                agent.report_background_turn_end(outcome).await;
            });
            let activity = self.activity().await;
            // A short turn can be over before this returns; say so rather
            // than promising a notice that already went out.
            let guidance = if activity.status == "running" {
                background_guidance()
            } else {
                completion_guidance(&activity)
            };
            let mut result = text_result(render_activity(&activity, &guidance));
            if activity.status != "running" {
                result.structured_content = Some(json!({ ACTIVITY_KEY: activity }));
            }
            return result;
        }

        enum Wait {
            RunCancelled,
            ContextRevoked,
            Done,
        }
        let revoked = call.tool_context.revoked.clone();
        let mut done_rx = done_rx;
        let wait = tokio::select! {
            biased;
            _ = call.cancel_token.cancelled() => Wait::RunCancelled,
            _ = revoked.cancelled() => Wait::ContextRevoked,
            _ = &mut done_rx => Wait::Done,
        };
        match wait {
            Wait::RunCancelled => {
                // The Maple turn ended; the agent keeps its thread but
                // stops working on this prompt, and its process goes so a
                // sandboxed command cannot outlive the stop.
                drop(done_rx);
                self.stop_turn().await;
            }
            Wait::ContextRevoked => self.shutdown().await,
            Wait::Done => {}
        }
        let activity = self.activity().await;
        let mut result = text_result(render_activity(&activity, &completion_guidance(&activity)));
        result.structured_content = Some(json!({ ACTIVITY_KEY: activity }));
        result
    }

    async fn ensure_process(
        self: &Arc<Self>,
        call: &ExternalAgentCall,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<(), String> {
        {
            let state = self.state.lock().await;
            if let Some(process) = state.process.as_ref()
                && !process.client.closed().is_cancelled()
            {
                return Ok(());
            }
        }
        let existing_thread = self.state.lock().await.thread_id.clone();
        let claude_thread = existing_thread
            .clone()
            .unwrap_or_else(claude::new_session_id);
        let (executable, args) = if self.provider == claude::PROVIDER_ID {
            let executable = claude::find_executable(call.login_path.as_deref())
                .ok_or("Install Claude Code and make sure `claude` is on PATH.")?;
            (
                executable,
                claude::command_args(&claude_thread, existing_thread.is_some(), model, effort),
            )
        } else {
            let executable = codex::find_executable(call.login_path.as_deref()).ok_or_else(|| {
                "Codex is not installed, or `codex` is not on PATH. Ask the user to install the Codex CLI.".to_string()
            })?;
            (
                executable,
                codex::app_server_args()
                    .iter()
                    .map(|arg| arg.to_string())
                    .collect(),
            )
        };
        let mut command = build_external_agent_command(
            &executable,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            &self.cwd,
            call.login_path.as_deref(),
            Some(&self.session_id),
            &call.tool_context,
        )?;
        if self.provider == claude::PROVIDER_ID {
            command.env_remove("CLAUDECODE");
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = {
            let _launch = call.tool_context.begin_process_launch(&call.cancel_token)?;
            spawn_contained(command)
                .map_err(|error| format!("Failed to start {}: {error}", self.provider_name()))?
        };
        let stdin = child
            .as_mut()
            .stdin()
            .take()
            .ok_or_else(|| "Failed to open the agent stdin".to_string())?;
        let stdout = child
            .as_mut()
            .stdout()
            .take()
            .ok_or_else(|| "Failed to open the agent stdout".to_string())?;
        let (client, receiver, reader) = if self.provider == claude::PROVIDER_ID {
            let (client, receiver, reader) = claude::Client::new(stdin, stdout, claude_thread);
            (Arc::new(AgentClient::Claude(client)), receiver, reader)
        } else {
            let (client, receiver, reader) = AppServerClient::new(stdin, stdout);
            (Arc::new(AgentClient::Codex(client)), receiver, reader)
        };
        client
            .request(RequestMethod::Initialize, codex::initialize_params())
            .await?;
        client.initialized().await?;
        let thread_id = {
            let existing = self.state.lock().await.thread_id.clone();
            let response = match &existing {
                Some(thread_id) => {
                    client
                        .request(
                            RequestMethod::ThreadResume,
                            codex::thread_resume_params(thread_id),
                        )
                        .await?
                }
                None => {
                    client
                        .request(
                            RequestMethod::ThreadStart,
                            codex::thread_start_params(&self.cwd, model),
                        )
                        .await?
                }
            };
            match existing {
                Some(thread_id) => thread_id,
                None => codex::thread_id_from_response(&response)
                    .ok_or_else(|| "The agent did not report a thread ID".to_string())?,
            }
        };
        let events = tokio::spawn(Arc::clone(self).consume_server_messages(receiver));
        let mut state = self.state.lock().await;
        if let Some(previous) = state.process.take() {
            previous.reader.abort();
            previous.events.abort();
        }
        state.thread_id = Some(thread_id.clone());
        state.activity.thread_id = Some(thread_id);
        state.process = Some(AgentProcess {
            child,
            client,
            reader,
            events,
        });
        Ok(())
    }

    async fn consume_server_messages(self: Arc<Self>, mut receiver: mpsc::Receiver<ServerMessage>) {
        while let Some(message) = receiver.recv().await {
            match message {
                ServerMessage::Notification { method, params } => {
                    let event = codex::parse_notification(&method, &params);
                    if self.provider == claude::PROVIDER_ID
                        && let CodexEvent::TurnCompleted { ref status, .. } = event
                    {
                        // A Claude process serves one turn. Reclaim it before
                        // reporting completion, including on protocol failure.
                        // On success stdin is closed: give session writes time
                        // to flush, then clean up any remaining descendants.
                        // Hold the lifecycle lock through cleanup so shutdown
                        // cannot return while this task still owns a child.
                        let mut state = self.state.lock().await;
                        if let Some(mut process) = state.process.take() {
                            if status == "completed" {
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(2),
                                    process.child.as_mut().wait(),
                                )
                                .await;
                            }
                            process.child.kill_and_wait().await;
                            process.reader.abort();
                            // Do not abort process.events: it is this task.
                        }
                        drop(state);
                        self.handle_event(event).await;
                        return;
                    }
                    self.handle_event(event).await;
                }
                ServerMessage::Request { id, method, params } => {
                    // An approval waits on the user. It must not stall the
                    // notifications behind it, so it runs on its own task.
                    let agent = Arc::clone(&self);
                    tokio::spawn(async move {
                        agent.handle_request(id, &method, params).await;
                    });
                }
            }
        }
        // The process ended. A turn it owed an answer to is over.
        let had_turn = self.state.lock().await.turn.is_some();
        if had_turn {
            self.finish_turn(
                TurnOutcome::Failed,
                Some("The Codex process exited before the turn finished.".to_string()),
            )
            .await;
        }
    }

    async fn handle_event(self: &Arc<Self>, event: CodexEvent) {
        match event {
            CodexEvent::ThreadStarted { thread_id } => {
                let mut state = self.state.lock().await;
                state.thread_id.get_or_insert(thread_id.clone());
                state.activity.thread_id.get_or_insert(thread_id);
            }
            CodexEvent::TurnStarted { turn_id } => {
                let mut state = self.state.lock().await;
                if let Some(turn) = state.turn.as_mut() {
                    turn.turn_id = turn_id;
                }
            }
            CodexEvent::TurnCompleted { status, error } => {
                let outcome = match status.as_str() {
                    "completed" => TurnOutcome::Completed,
                    "interrupted" | "cancelled" | "canceled" => TurnOutcome::Cancelled,
                    _ => TurnOutcome::Failed,
                };
                self.finish_turn(outcome, error).await;
            }
            CodexEvent::AgentMessageDelta { item_id, delta } => {
                {
                    let mut state = self.state.lock().await;
                    match state.messages.iter_mut().find(|(id, _)| *id == item_id) {
                        Some((_, text)) => text.push_str(&delta),
                        None => state.messages.push((item_id, delta)),
                    }
                    Self::refresh_text(&mut state);
                }
                self.emit_row(false).await;
            }
            CodexEvent::ItemStarted(item) => {
                let label = {
                    let mut state = self.state.lock().await;
                    match item {
                        CodexItem::CommandExecution { id, command, .. } => {
                            let label = format!("Running: {command}");
                            state.activity.record_command_started(id, command);
                            Some(label)
                        }
                        CodexItem::TodoList { items, .. } => {
                            state.activity.record_todos(items);
                            None
                        }
                        CodexItem::FileChange { changes, .. } => {
                            let label = changes
                                .first()
                                .map(|change| format!("Editing: {}", change.path));
                            state.activity.record_file_changes(changes);
                            label
                        }
                        _ => None,
                    }
                };
                if let Some(label) = label {
                    self.emit_subagent_activity(label);
                }
                self.emit_row(true).await;
            }
            CodexEvent::ItemCompleted(item) => {
                let label = {
                    let mut state = self.state.lock().await;
                    match item {
                        CodexItem::AgentMessage { id, text } => {
                            match state
                                .messages
                                .iter_mut()
                                .find(|(item_id, _)| *item_id == id)
                            {
                                Some((_, existing)) if !text.is_empty() => *existing = text,
                                Some(_) => {}
                                None => state.messages.push((id, text)),
                            }
                            Self::refresh_text(&mut state);
                            None
                        }
                        CodexItem::CommandExecution {
                            id,
                            command,
                            exit_code,
                            status,
                        } => {
                            state
                                .activity
                                .record_command_finished(&id, command, exit_code, status);
                            None
                        }
                        CodexItem::FileChange { changes, .. } => {
                            let label = changes
                                .first()
                                .map(|change| format!("Edited: {}", change.path));
                            state.activity.record_file_changes(changes);
                            label
                        }
                        CodexItem::TodoList { items, .. } => {
                            state.activity.record_todos(items);
                            None
                        }
                        CodexItem::AsyncQuestion { id, questions } => {
                            // Mark the wait now, before the turn can end and
                            // render its result; the answer task runs later.
                            state.open_questions += 1;
                            state.activity.pending_permission =
                                Some("answer a question".to_string());
                            let agent = Arc::clone(self);
                            tokio::spawn(agent.answer_async_question(id, questions));
                            None
                        }
                        CodexItem::Other { .. } => None,
                    }
                };
                if let Some(label) = label {
                    self.emit_subagent_activity(label);
                }
                self.emit_row(true).await;
            }
            CodexEvent::Other => {}
        }
    }

    fn refresh_text(state: &mut AgentState) {
        let joined = state
            .messages
            .iter()
            .map(|(_, text)| text.as_str())
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        state.activity.text = tail_bounded(&joined, MAX_RESULT_TEXT_CHARS);
    }

    /// A token that fires when the current turn ends; already fired when
    /// no turn is running.
    async fn turn_ended(&self) -> CancellationToken {
        let state = self.state.lock().await;
        match state.turn.as_ref() {
            Some(turn) => turn.ended.clone(),
            None => {
                let ended = CancellationToken::new();
                ended.cancel();
                ended
            }
        }
    }

    async fn handle_request(&self, id: Value, method: &str, params: Value) {
        log::info!("External agent {} asks: {method}", self.agent_id);
        let client = {
            let state = self.state.lock().await;
            match state.process.as_ref() {
                Some(process) => Arc::clone(&process.client),
                None => return,
            }
        };
        if self.provider == claude::PROVIDER_ID && method == "claude/tool/requestApproval" {
            let tool = params["tool"].as_str().unwrap_or("tool");
            let arguments = params["input"].as_object().cloned().unwrap_or_default();
            let request = AgentPermissionRequest {
                request_id: format!("{}-{}", self.agent_id, id.as_str().unwrap_or("request")),
                tool_name: "claude_tool".into(),
                arguments,
                prompt: Some(format!("Claude Code wants to use {tool}")),
            };
            let decision = self
                .request_permission(request, format!("use {tool}"))
                .await;
            let _ = client.respond(id, codex::approval_response(decision)).await;
            return;
        }
        let response = match codex::parse_server_request(method, &params) {
            CodexServerRequest::CommandApproval {
                item_id,
                command,
                cwd,
                reason,
            } => {
                let mut arguments = serde_json::Map::new();
                arguments.insert("command".to_string(), json!(command));
                if let Some(cwd) = cwd {
                    arguments.insert("cwd".to_string(), json!(cwd));
                }
                if let Some(reason) = &reason {
                    arguments.insert("reason".to_string(), json!(reason));
                }
                let request = AgentPermissionRequest {
                    request_id: format!("{}-{item_id}", self.agent_id),
                    tool_name: "codex_command".to_string(),
                    arguments,
                    prompt: Some(format!("Codex wants to run: {command}")),
                };
                let decision = self
                    .request_permission(request, format!("run `{command}`"))
                    .await;
                codex::approval_response(decision)
            }
            CodexServerRequest::FileChangeApproval { item_id, reason } => {
                let mut arguments = serde_json::Map::new();
                if let Some(reason) = &reason {
                    arguments.insert("reason".to_string(), json!(reason));
                }
                let request = AgentPermissionRequest {
                    request_id: format!("{}-{item_id}", self.agent_id),
                    tool_name: "codex_file_change".to_string(),
                    arguments,
                    prompt: Some("Codex wants to change files in the project".to_string()),
                };
                let decision = self
                    .request_permission(request, "change project files".to_string())
                    .await;
                codex::approval_response(decision)
            }
            CodexServerRequest::UserInput { questions } => {
                if questions.is_empty() {
                    codex::user_input_response("")
                } else {
                    // The service's own broker, not the process global: a
                    // rebuilt service must not strand this agent's question
                    // in a broker nobody answers.
                    let broker = self.host.service.questions.clone();
                    self.set_pending_permission(Some("answer a question".to_string()))
                        .await;
                    let ended = self.turn_ended().await;
                    let answer = tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => String::new(),
                        _ = ended.cancelled() => String::new(),
                        answer = broker.ask(&self.session_id, questions) => answer,
                    };
                    self.set_pending_permission(None).await;
                    codex::user_input_response(&answer)
                }
            }
            CodexServerRequest::Unknown { method } => {
                client
                    .respond_error(id, &format!("{method} is not supported by this client"))
                    .await;
                return;
            }
        };
        log::info!(
            "External agent {} answered {method}: {}",
            self.agent_id,
            response
                .get("decision")
                .and_then(Value::as_str)
                .unwrap_or("answers")
        );
        if let Err(error) = client.respond(id, response).await {
            log::warn!("Failed to answer an external agent request: {error}");
        }
    }

    /// Codex asked the user without blocking its turn. Show the question,
    /// and hand the answer back as the next input: steered into the turn
    /// if it still runs, otherwise as a turn of Maple's own.
    fn answer_async_question(
        self: Arc<Self>,
        item_id: String,
        questions: Vec<codex::AsyncQuestion>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        // Boxed: the answer may start a turn, whose event loop reaches this
        // function again. A concrete boxed type ends the recursive future.
        Box::pin(async move {
            log::info!(
                "External agent {} asks the user {} question(s) without blocking",
                self.agent_id,
                questions.len()
            );
            self.set_pending_permission(Some("answer a question".to_string()))
                .await;
            let broker = self.host.service.questions.clone();
            let answer = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => String::new(),
                answer = broker.ask(&self.session_id, codex::async_question_prompts(&questions)) => answer,
            };
            {
                let mut state = self.state.lock().await;
                state.open_questions = state.open_questions.saturating_sub(1);
            }
            self.set_pending_permission(None).await;
            let Some(prompt) = codex::async_answer_prompt(&questions, &answer) else {
                log::info!(
                    "External agent {} question {item_id} was dismissed",
                    self.agent_id
                );
                return;
            };
            let steer = {
                let state = self.state.lock().await;
                match (
                    state.process.as_ref(),
                    state.turn.as_ref(),
                    state.thread_id.as_ref(),
                ) {
                    (Some(process), Some(turn), Some(thread_id)) => {
                        turn.turn_id.as_ref().map(|turn_id| {
                            (
                                Arc::clone(&process.client),
                                thread_id.clone(),
                                turn_id.clone(),
                            )
                        })
                    }
                    _ => None,
                }
            };
            if let Some((client, thread_id, turn_id)) = steer {
                match client
                    .request(
                        RequestMethod::TurnSteer,
                        codex::turn_steer_params(&thread_id, &turn_id, &prompt),
                    )
                    .await
                {
                    Ok(_) => {
                        log::info!("External agent {} took the answers mid-turn", self.agent_id);
                        return;
                    }
                    Err(error) => {
                        log::debug!("Steering the answers failed, starting a turn: {error}")
                    }
                }
            }
            let Some(stored) = self.state.lock().await.last_call.clone() else {
                return;
            };
            let call = ExternalAgentCall {
                session_id: self.session_id.clone(),
                working_dir: stored.working_dir,
                row_id: None,
                login_path: stored.login_path,
                tool_context: stored.tool_context,
                cancel_token: CancellationToken::new(),
            };
            let result = self
                .run_turn(
                    &call,
                    TurnInput {
                        prompt,
                        background: true,
                        model: None,
                        effort: None,
                    },
                )
                .await;
            if result.is_error.unwrap_or(false) {
                log::warn!(
                    "External agent {} could not take the answers: {}",
                    self.agent_id,
                    result
                        .content
                        .iter()
                        .filter_map(|content| content.as_text().map(|text| text.text.clone()))
                        .collect::<String>()
                );
            }
        })
    }

    async fn set_pending_permission(&self, pending: Option<String>) {
        let label = {
            let mut state = self.state.lock().await;
            state.activity.pending_permission = pending;
            latest_activity_label(&state.activity)
        };
        // The row above the composer shows only its latest label; say what
        // the agent waits on, and clear it again once the user decided.
        self.emit_subagent_activity(label.unwrap_or_else(|| "Working".to_string()));
        self.emit_row(true).await;
    }

    /// Put one approval in front of the user through Maple's permission
    /// card and wait for the decision.
    async fn request_permission(
        &self,
        request: AgentPermissionRequest,
        summary: String,
    ) -> AgentPermissionDecision {
        if self.cancel.is_cancelled() || self.turn_ended().await.is_cancelled() {
            return AgentPermissionDecision::Cancel;
        }
        {
            let modes = self.host.permission_modes.lock().await;
            if modes
                .get(&self.session_id)
                .copied()
                .unwrap_or(GOOSE_PERMISSION_ROUTING_MODE)
                == GooseMode::Auto
            {
                return AgentPermissionDecision::AllowOnce;
            }
        }
        if self.cancel.is_cancelled() {
            return AgentPermissionDecision::Cancel;
        }
        let (tx, rx) = oneshot::channel();
        let responder = ExternalPermissionResponder::new(tx);
        let run_id = external_run_id(&self.agent_id);
        let request_id = request.request_id.clone();
        let registration = register_pending_permission(
            &self.host.service.pending_permissions,
            &self.issued_permission_ids,
            &self.session_id,
            &run_id,
            AgentPermissionRouting::Desktop,
            request.clone(),
            &self.cancel,
            PendingPermissionOrigin::ExternalAgent(responder),
        )
        .await;
        if registration != PendingPermissionRegistration::Registered {
            return AgentPermissionDecision::Cancel;
        }
        self.set_pending_permission(Some(summary)).await;
        let item = external_permission_item(&request, unix_ms());
        self.record_live_if_desktop_run(item.clone()).await;
        emit_agent_event(
            &self.host.service.host.events,
            AgentServiceEvent::Run {
                session_id: self.session_id.clone(),
                run_id,
                event: AgentRunEvent::PermissionRequested { request, item },
            },
        );
        let ended = self.turn_ended().await;
        let withdraw = async {
            let key = (self.session_id.clone(), request_id.clone());
            let removed = self
                .host
                .service
                .pending_permissions
                .lock()
                .await
                .remove(&key);
            if let Some(removed) = removed {
                publish_external_permission_decision(
                    &self.host.service,
                    &self.session_id,
                    &removed.request,
                    decision_row_status(AgentPermissionDecision::Cancel),
                )
                .await;
            }
            AgentPermissionDecision::Cancel
        };
        let decision = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => withdraw.await,
            _ = ended.cancelled() => withdraw.await,
            decision = rx => decision.unwrap_or(AgentPermissionDecision::Cancel),
        };
        self.set_pending_permission(None).await;
        decision
    }

    /// Ask the agent to stop its current turn. The thread stays open.
    async fn interrupt(&self) {
        let (client, thread_id, turn_id) = {
            let state = self.state.lock().await;
            let Some(process) = state.process.as_ref() else {
                return;
            };
            let Some(turn) = state.turn.as_ref() else {
                return;
            };
            (
                Arc::clone(&process.client),
                state.thread_id.clone(),
                turn.turn_id.clone(),
            )
        };
        let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) else {
            // The turn has not been identified yet; the agent will report
            // it and the caller's settle timeout ends the wait.
            return;
        };
        let request = client.request(
            RequestMethod::TurnInterrupt,
            codex::turn_interrupt_params(&thread_id, &turn_id),
        );
        if let Ok(Err(error)) = tokio::time::timeout(INTERRUPT_REQUEST_TIMEOUT, request).await {
            log::debug!("External agent interrupt was refused: {error}");
        }
    }

    /// Stop what the agent is doing now and reclaim its process. Codex
    /// does not always end a sandboxed command on `turn/interrupt`, so the
    /// process group goes too; the thread is on disk and the next send
    /// resumes it in a fresh process.
    async fn stop_turn(&self) {
        let had_turn = self.state.lock().await.turn.is_some();
        if !had_turn {
            return;
        }
        self.interrupt().await;
        let settled = tokio::time::timeout(INTERRUPT_SETTLE_TIMEOUT, async {
            loop {
                if self.state.lock().await.turn.is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .is_ok();
        let process = self.state.lock().await.process.take();
        if let Some(mut process) = process {
            process.child.kill_and_wait().await;
            process.reader.abort();
            process.events.abort();
        }
        if !settled {
            self.finish_turn(TurnOutcome::Cancelled, None).await;
        }
    }

    /// End the agent: interrupt, kill its process group, and close its turn.
    async fn shutdown(&self) {
        self.cancel.cancel();
        self.interrupt().await;
        let process = self.state.lock().await.process.take();
        if let Some(mut process) = process {
            process.child.kill_and_wait().await;
            process.reader.abort();
            process.events.abort();
        }
        let had_turn = self.state.lock().await.turn.is_some();
        if had_turn {
            self.finish_turn(TurnOutcome::Cancelled, None).await;
        }
    }

    async fn finish_turn(&self, outcome: TurnOutcome, error: Option<String>) {
        let done = {
            let mut state = self.state.lock().await;
            let Some(mut turn) = state.turn.take() else {
                return;
            };
            state.activity.status = outcome.status().to_string();
            if state.open_questions == 0 {
                state.activity.pending_permission = None;
            }
            if error.is_some() {
                state.activity.error = error;
            }
            self.refresh_elapsed(&mut state);
            let mut row = activity_row_item(&turn.row_id, &state.activity, unix_ms());
            if turn.synthetic {
                row.title = Some(SYNTHETIC_TURN_TITLE.to_string());
            }
            state.last_row_id = Some(turn.row_id.clone());
            state.last_row_emit = Some(Instant::now());
            turn.ended.cancel();
            log::info!(
                "External agent {} turn ended: {}",
                self.agent_id,
                outcome.status()
            );
            // The turn is gone from the state, so emit its last row here.
            self.record_live_if_desktop_run(row.clone()).await;
            emit_agent_event(
                &self.host.service.host.events,
                AgentServiceEvent::TimelineItem {
                    session_id: self.session_id.clone(),
                    run_id: None,
                    item: row,
                },
            );
            turn.done.take()
        };
        emit_agent_event(
            &self.host.service.host.events,
            AgentServiceEvent::Run {
                session_id: self.session_id.clone(),
                run_id: external_run_id(&self.agent_id),
                event: AgentRunEvent::SubagentFinished {
                    id: subagent_row_id(&self.agent_id),
                },
            },
        );
        if let Some(done) = done {
            let _ = done.send(outcome);
        }
    }

    /// A background turn ended: leave the result in the transcript and
    /// tell the model, into the turn that is running or the next one.
    async fn report_background_turn_end(&self, outcome: TurnOutcome) {
        if self.host.lifetime.is_cancelled() {
            return;
        }
        let activity = self.activity().await;
        let result_text = render_activity(&activity, &completion_guidance(&activity));
        let row_id = self.last_row_id().await;
        let for_model = background_result_message(
            &format!("external agent {} ({})", self.agent_id, self.provider),
            outcome.status(),
            &result_text,
            &format!(
                "Use {AGENT_STATUS_TOOL}(provider: \"{}\", agent_id: \"{}\") only if you need to inspect its current state again.",
                self.provider, self.agent_id
            ),
        );
        let delivered = self
            .host
            .runtime
            .send_background_completion(
                &self.session_id,
                for_model.clone(),
                self.host.lifetime.clone(),
            )
            .await
            .is_ok();
        // Serialize these durable notices with logout and task deletion too.
        let _runtime_guard = self.host.service.runtime_lifecycle.lock().await;
        let _session_guard = self.host.service.session_lifecycle.lock().await;
        if self.host.lifetime.is_cancelled() || self.host.runtime.verify_generation().await.is_err()
        {
            return;
        }
        if !delivered {
            // A rejected start can still be read on the next explicit user send.
            // The visible notice below must not claim that a run was scheduled.
            let _ = self
                .host
                .session_manager
                .add_message(&self.session_id, &for_model)
                .await;
        }
        let notice = Message::assistant()
            .with_system_notification_with_data(
                SystemNotificationType::InlineMessage,
                format!(
                    "External agent {} {}",
                    self.agent_id,
                    match outcome {
                        TurnOutcome::Completed => "finished",
                        TurnOutcome::Failed => "failed",
                        TurnOutcome::Cancelled => "was interrupted",
                    }
                ),
                json!({
                    ACTIVITY_KEY: activity,
                    NOTICE_ROW_KEY: row_id,
                    NOTICE_RESULT_KEY: result_text,
                }),
            )
            .with_visibility(true, false)
            .with_generated_id();
        // Two notices: one carries the activity back onto the tool row, the
        // other is a plain line the user cannot miss, like the one a
        // background subagent leaves.
        let visible = Message::assistant()
            .with_system_notification(
                SystemNotificationType::InlineMessage,
                format!(
                    "External agent {} ({}) {}. {}",
                    self.agent_id,
                    self.provider_name(),
                    match outcome {
                        TurnOutcome::Completed => "finished",
                        TurnOutcome::Failed => "failed",
                        TurnOutcome::Cancelled => "was interrupted",
                    },
                    if delivered {
                        "The result was delivered to the task."
                    } else {
                        "The task could not be resumed. Send a message to read its result."
                    }
                ),
            )
            .with_visibility(true, false)
            .with_generated_id();
        for message in [&notice, &visible] {
            if let Err(error) = self
                .host
                .session_manager
                .add_message(&self.session_id, message)
                .await
            {
                log::warn!("Failed to record the end of an external agent turn: {error}");
                continue;
            }
            for item in message_to_timeline_items(message, false) {
                self.record_live_if_desktop_run(item.clone()).await;
                emit_agent_event(
                    &self.host.service.host.events,
                    AgentServiceEvent::TimelineItem {
                        session_id: self.session_id.clone(),
                        run_id: None,
                        item,
                    },
                );
            }
        }
    }

    async fn last_row_id(&self) -> String {
        let state = self.state.lock().await;
        state
            .turn
            .as_ref()
            .map(|turn| turn.row_id.clone())
            .unwrap_or_else(|| self.row_id_hint(&state))
    }

    fn row_id_hint(&self, state: &AgentState) -> String {
        state
            .last_row_id
            .clone()
            .unwrap_or_else(|| format!("external-{}-turn-{}", self.agent_id, state.activity.turns))
    }

    fn emit_subagent_started(&self, background: bool) {
        emit_agent_event(
            &self.host.service.host.events,
            AgentServiceEvent::Run {
                session_id: self.session_id.clone(),
                run_id: external_run_id(&self.agent_id),
                event: AgentRunEvent::SubagentStarted {
                    id: subagent_row_id(&self.agent_id),
                    task: self.task.clone(),
                    background,
                    external: Some(ExternalAgentRef {
                        provider: self.provider.clone(),
                        agent_id: self.agent_id.clone(),
                    }),
                },
            },
        );
    }

    fn emit_subagent_activity(&self, tool: String) {
        emit_agent_event(
            &self.host.service.host.events,
            AgentServiceEvent::Run {
                session_id: self.session_id.clone(),
                run_id: external_run_id(&self.agent_id),
                event: AgentRunEvent::SubagentActivity {
                    id: subagent_row_id(&self.agent_id),
                    tool,
                },
            },
        );
    }

    /// Repaint the transcript row of the current turn with the activity so
    /// far. Streamed text is throttled; structural changes are not.
    async fn emit_row(&self, force: bool) {
        let item = {
            let mut state = self.state.lock().await;
            let Some(row_id) = state.turn.as_ref().map(|turn| turn.row_id.clone()) else {
                return;
            };
            let now = Instant::now();
            if !force
                && state
                    .last_row_emit
                    .is_some_and(|last| now.duration_since(last) < LIVE_ROW_INTERVAL)
            {
                return;
            }
            state.last_row_emit = Some(now);
            state.last_row_id = Some(row_id.clone());
            self.refresh_elapsed(&mut state);
            let mut item = activity_row_item(&row_id, &state.activity, unix_ms());
            if state.turn.as_ref().is_some_and(|turn| turn.synthetic) {
                item.title = Some(SYNTHETIC_TURN_TITLE.to_string());
            }
            item
        };
        self.record_live_if_desktop_run(item.clone()).await;
        emit_agent_event(
            &self.host.service.host.events,
            AgentServiceEvent::TimelineItem {
                session_id: self.session_id.clone(),
                run_id: None,
                item,
            },
        );
    }

    /// Keep the live overlay of a desktop run current, so a mid-run
    /// reopen shows the row. With no run there is no overlay to keep.
    async fn record_live_if_desktop_run(&self, item: AgentTimelineItem) {
        let desktop_run_active = {
            let runtime = self.host.service.inner.lock().await;
            runtime.as_ref().is_some_and(|current| {
                current.active_runs.values().any(|run| {
                    run.session_id == self.session_id
                        && run.permission_routing == AgentPermissionRouting::Desktop
                })
            })
        };
        if desktop_run_active {
            record_timeline_item(
                &self.host.service.live_timelines,
                &self.session_id,
                AgentPermissionRouting::Desktop,
                item,
            )
            .await;
        }
    }
}

fn latest_activity_label(activity: &ExternalAgentActivity) -> Option<String> {
    if let Some(pending) = &activity.pending_permission {
        return Some(format!("Waiting for you to {pending}"));
    }
    if let Some(command) = activity
        .commands
        .iter()
        .rev()
        .find(|command| command.status == "running")
    {
        return Some(format!("Running: {}", command.command));
    }
    activity
        .file_changes
        .last()
        .map(|change| format!("Edited: {}", change.path))
        .or_else(|| {
            activity
                .commands
                .last()
                .map(|command| format!("Ran: {}", command.command))
        })
}

/// Keep the end of `text`, which is where an agent's conclusion lives.
fn tail_bounded(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars;
    format!("…{}", text.chars().skip(skip + 1).collect::<String>())
}

/// The transcript row for a turn in progress, or its final state.
fn activity_row_item(
    row_id: &str,
    activity: &ExternalAgentActivity,
    created_ms: u128,
) -> AgentTimelineItem {
    let status = match activity.status.as_str() {
        "running" => "running",
        "failed" => "failed",
        "cancelled" => "cancelled",
        _ => "completed",
    };
    AgentTimelineItem {
        id: row_id.to_string(),
        item_type: "tool".to_string(),
        role: Some("assistant".to_string()),
        title: None,
        text: None,
        status: Some(status.to_string()),
        input: None,
        output: Some(json!({
            "text": render_activity(activity, ""),
            "structuredContent": { ACTIVITY_KEY: activity },
            "content": [],
        })),
        created_ms,
        merge: "replace".to_string(),
    }
}

/// Tell the desktop how an external agent's permission was decided. The
/// live overlay is updated when a run holds one; the row is emitted either
/// way, because a background agent's request has no run behind it and the
/// card would otherwise stay "pending" forever.
pub(super) async fn publish_external_permission_decision(
    service: &MapleAgentService,
    session_id: &str,
    request: &AgentPermissionRequest,
    status: &str,
) {
    let item = match update_live_permission_status(
        &service.live_timelines,
        session_id,
        AgentPermissionRouting::Desktop,
        &request.request_id,
        status,
    )
    .await
    {
        Some(item) => item,
        None => {
            let mut item = external_permission_item(request, unix_ms());
            item.status = Some(status.to_string());
            item
        }
    };
    emit_agent_event(
        &service.host.events,
        AgentServiceEvent::TimelineItem {
            session_id: session_id.to_string(),
            run_id: None,
            item,
        },
    );
}

/// The permission row for an external agent's approval request, shaped
/// like Goose's so the transcript treats them alike.
pub(super) fn external_permission_item(
    request: &AgentPermissionRequest,
    created_ms: u128,
) -> AgentTimelineItem {
    AgentTimelineItem {
        id: format!("permission-{}", request.request_id),
        item_type: "permission".to_string(),
        role: Some("system".to_string()),
        title: Some(match request.tool_name.as_str() {
            "codex_command" => "Codex: run command".to_string(),
            "codex_file_change" => "Codex: change files".to_string(),
            other => other.to_string(),
        }),
        text: request.prompt.clone(),
        status: Some("pending".to_string()),
        input: Some(Value::Object(request.arguments.clone())),
        output: None,
        created_ms,
        merge: "replace".to_string(),
    }
}

/// Project a persisted end-of-turn notice back onto the tool row it
/// belongs to, so a reopened task shows what the agent did.
pub(super) fn notice_timeline_item(
    notification: &SystemNotificationContent,
    created_ms: u128,
) -> Option<AgentTimelineItem> {
    let data = notification.data.as_ref()?;
    let activity: ExternalAgentActivity =
        serde_json::from_value(data.get(ACTIVITY_KEY)?.clone()).ok()?;
    let row_id = data.get(NOTICE_ROW_KEY)?.as_str()?;
    let result_text = data
        .get(NOTICE_RESULT_KEY)
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut item = activity_row_item(row_id, &activity, created_ms);
    item.output = Some(json!({
        "text": result_text,
        "structuredContent": { ACTIVITY_KEY: activity },
        "content": [],
    }));
    Some(item)
}

/// Answers one external-agent permission request. Shared between the
/// pending-permission table and the waiting agent; whoever resolves first
/// takes the sender.
#[derive(Clone)]
pub(super) struct ExternalPermissionResponder {
    sender: Arc<StdMutex<Option<oneshot::Sender<AgentPermissionDecision>>>>,
}

impl ExternalPermissionResponder {
    fn new(sender: oneshot::Sender<AgentPermissionDecision>) -> Self {
        Self {
            sender: Arc::new(StdMutex::new(Some(sender))),
        }
    }

    pub(super) fn resolve(&self, decision: AgentPermissionDecision) -> bool {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        sender.is_some_and(|sender| sender.send(decision).is_ok())
    }

    pub(super) fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.sender, &other.sender)
    }
}

impl std::fmt::Debug for ExternalPermissionResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExternalPermissionResponder")
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn activity_bounds_commands_files_and_todos() {
        let mut activity = ExternalAgentActivity::default();
        for index in 0..(MAX_ACTIVITY_COMMANDS + 5) {
            activity.record_command_started(format!("c{index}"), format!("cmd {index}"));
        }
        assert_eq!(activity.commands.len(), MAX_ACTIVITY_COMMANDS);
        assert_eq!(activity.commands[0].id, "c5");
        activity.record_command_finished("c24", "cmd 24".into(), Some(2), None);
        assert_eq!(activity.commands.last().unwrap().status, "failed");
        activity.record_command_finished("c23", "cmd 23".into(), Some(0), None);
        assert_eq!(
            activity.commands[MAX_ACTIVITY_COMMANDS - 2].status,
            "completed"
        );

        activity.record_file_changes(
            (0..(MAX_ACTIVITY_FILE_CHANGES + 3))
                .map(|index| codex::FileChangeEntry {
                    path: format!("f{index}"),
                    kind: "update".into(),
                })
                .collect(),
        );
        assert_eq!(activity.file_changes.len(), MAX_ACTIVITY_FILE_CHANGES);
        activity.record_file_changes(vec![codex::FileChangeEntry {
            path: "f42".into(),
            kind: "delete".into(),
        }]);
        assert_eq!(activity.file_changes.len(), MAX_ACTIVITY_FILE_CHANGES);
        assert_eq!(activity.file_changes.last().unwrap().kind, "delete");

        activity.record_todos(
            (0..(MAX_ACTIVITY_TODOS + 1))
                .map(|index| codex::TodoEntry {
                    text: format!("t{index}"),
                    completed: false,
                })
                .collect(),
        );
        assert_eq!(activity.todos.len(), MAX_ACTIVITY_TODOS);
    }

    #[test]
    fn rendered_result_is_paseo_shaped_and_bounded() {
        let activity = ExternalAgentActivity {
            provider: "codex".into(),
            agent_id: "codex-1".into(),
            thread_id: Some("thread".into()),
            status: "completed".into(),
            text: "x".repeat(MAX_RESULT_TEXT_CHARS + 10),
            file_changes: vec![ActivityFileChange {
                path: "a.rs".into(),
                kind: "update".into(),
            }],
            commands: vec![ActivityCommand {
                id: "c".into(),
                command: "cargo test".into(),
                exit_code: Some(1),
                status: "failed".into(),
            }],
            ..Default::default()
        };
        let text = render_activity(&activity, "Go on.");
        assert!(text.starts_with(
            "Status: completed\nProvider: codex\nAgent ID: codex-1\nThread ID: thread\n"
        ));
        assert!(text.contains("Files changed (1): a.rs"));
        assert!(text.contains("Commands run: 1 (1 failed)"));
        let response = text
            .split("<agent-response>\n")
            .nth(1)
            .unwrap()
            .split("\n</agent-response>")
            .next()
            .unwrap();
        // The bound keeps the text plus one ellipsis, like every other
        // bounded string in the transcript.
        assert_eq!(response.chars().count(), MAX_RESULT_TEXT_CHARS + 1);
        assert!(response.ends_with('…'));
        assert!(text.ends_with("Go on."));
    }

    #[test]
    fn tail_bound_keeps_the_end() {
        assert_eq!(tail_bounded("abcdef", 3), "…ef");
        assert_eq!(tail_bounded("abc", 3), "abc");
    }

    #[test]
    fn notice_projects_onto_the_tool_row() {
        let activity = ExternalAgentActivity {
            provider: "codex".into(),
            agent_id: "codex-1".into(),
            status: "completed".into(),
            ..Default::default()
        };
        let notification = SystemNotificationContent {
            notification_type: SystemNotificationType::InlineMessage,
            msg: "External agent codex-1 finished".into(),
            data: Some(json!({
                ACTIVITY_KEY: activity,
                NOTICE_ROW_KEY: "row-9",
                NOTICE_RESULT_KEY: "Status: completed",
            })),
        };
        let item = notice_timeline_item(&notification, 7).unwrap();
        assert_eq!(item.id, "row-9");
        assert_eq!(item.item_type, "tool");
        assert_eq!(item.status.as_deref(), Some("completed"));
        assert_eq!(item.output.as_ref().unwrap()["text"], "Status: completed");
        assert_eq!(
            item.output.as_ref().unwrap()["structuredContent"][ACTIVITY_KEY]["agentId"],
            "codex-1"
        );
        let plain = SystemNotificationContent {
            notification_type: SystemNotificationType::InlineMessage,
            msg: "hi".into(),
            data: None,
        };
        assert!(notice_timeline_item(&plain, 7).is_none());
    }

    #[test]
    fn permission_row_mirrors_goose_shape() {
        let request = AgentPermissionRequest {
            request_id: "codex-1-i1".into(),
            tool_name: "codex_command".into(),
            arguments: serde_json::Map::from_iter([("command".to_string(), json!("ls"))]),
            prompt: Some("Codex wants to run: ls".into()),
        };
        let item = external_permission_item(&request, 1);
        assert_eq!(item.id, "permission-codex-1-i1");
        assert_eq!(item.item_type, "permission");
        assert_eq!(item.status.as_deref(), Some("pending"));
        assert_eq!(item.title.as_deref(), Some("Codex: run command"));
    }

    #[test]
    fn responder_resolves_once() {
        let (tx, rx) = oneshot::channel();
        let responder = ExternalPermissionResponder::new(tx);
        assert!(responder.resolve(AgentPermissionDecision::AllowOnce));
        assert!(!responder.resolve(AgentPermissionDecision::DenyOnce));
        assert_eq!(
            rx.blocking_recv().unwrap(),
            AgentPermissionDecision::AllowOnce
        );
    }
}
