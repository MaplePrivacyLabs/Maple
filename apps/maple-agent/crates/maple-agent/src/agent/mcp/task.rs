//! A task's MCP servers, run as Pi runs them. The servers chosen for a task
//! connect in the background when it runs, and once one connects, its tools
//! join the task's Pi session as `mcp__<server>__<tool>`. A run waits a
//! little for servers still connecting, so its first prompt can use them;
//! one that connects later adds its tools from the next prompt. The servers
//! live with the task's loaded session, which stops them when it shuts
//! down, and their instructions reach the model in a prompt section.
//!
//! Which servers a task has is saved with the task, by name. A task runs the
//! servers' current settings, so an edited server is restarted at the task's
//! next run, and a chosen server that is no longer configured stays chosen
//! but cannot run.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation};
use pi_coding_agent::extensions::{
    BeforeAgentStart, BeforeAgentStartResult, Extension, ExtensionContext, SessionShutdown,
    ToolPrompt, extension,
};
use serde_json::Value;
use tokio::sync::Notify;

use super::super::config::{account_config_dir_path, load_agent_config_file};
use super::super::cua;
use super::super::external_agents;
use super::super::integrations::{cua_default, is_cua_identity};
use super::super::store::{TaskKind, TaskRow};
use super::super::timeline::bounded_timeline_text;
use super::super::{
    AgentIntegration, AgentMcpServer, AgentMcpTransport, AgentPathLayout, AgentRuntimeHandle,
    AgentSessionIntegrationKind, AgentSessionMcpServer, AgentSetSessionMcpServerRequest,
};
use super::connection::{Listing, McpServer, ToolsChanged};
use super::tool::{convert_result, description, parameters, tool_name};
use super::{RESERVED_KEYS, name_to_key, normalize_mcp_servers};

/// How long a run waits for its task's servers to connect before its first
/// prompt, as Pi's first prompt waits, and a server switched on for a task
/// is waited for.
pub(in crate::agent) const STARTUP_WAIT: Duration = Duration::from_secs(10);

/// The task setting that lists the task's servers by name.
const TASK_SERVERS: &str = "mcpServers";

/// The prompt section with the connected servers' instructions.
const INSTRUCTIONS_SECTION: &str = "mcp_servers";
/// The part of one server's instructions the prompt keeps.
const MAX_INSTRUCTIONS_CHARS: usize = 4_000;

/// The failed servers one notice names, and how much of each.
const MAX_REPORTED_FAILURES: usize = 3;
const MAX_FAILURE_CHARS: usize = 200;
const FAILURES_PREFIX: &str = "Some MCP servers could not connect:";

/// The idle tasks that keep their servers running, most recently run
/// first; the servers of tasks idle longer stop until they run again.
pub(in crate::agent) const MAX_IDLE_TASKS_WITH_SERVERS: usize = 4;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The names of the servers chosen for a task.
pub(in crate::agent) fn chosen_servers(row: &TaskRow) -> Vec<String> {
    row.settings
        .get(TASK_SERVERS)
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub(in crate::agent) fn set_chosen_servers(row: &mut TaskRow, names: Vec<String>) {
    if !row.settings.is_object() {
        row.settings = Value::Object(Default::default());
    }
    row.settings[TASK_SERVERS] = Value::from(names);
}

/// The servers a new task gets: those switched on for new tasks, or
/// exactly the ones `requested` names. Computer use's names are left to the
/// integration.
pub(in crate::agent) fn servers_for_new_task(
    saved: &[AgentMcpServer],
    requested: Option<&[String]>,
) -> Result<Vec<String>, String> {
    let Some(requested) = requested else {
        return Ok(saved
            .iter()
            .filter(|server| server.enabled && !is_cua_identity(&server.name))
            .map(|server| server.name.clone())
            .collect());
    };
    let mut names: Vec<String> = Vec::new();
    for name in requested {
        if is_cua_identity(name) {
            continue;
        }
        let key = name_to_key(name.trim());
        let server = saved
            .iter()
            .find(|server| name_to_key(&server.name) == key)
            .ok_or_else(|| {
                format!(
                    "MCP server '{}' is no longer configured. Reopen the MCP menu and try again.",
                    name.trim()
                )
            })?;
        if !names.iter().any(|chosen| name_to_key(chosen) == key) {
            names.push(server.name.clone());
        }
    }
    Ok(names)
}

/// The saved servers a task runs: those chosen for it that are still
/// configured.
pub(in crate::agent) fn task_servers(
    saved: &[AgentMcpServer],
    row: &TaskRow,
) -> Vec<AgentMcpServer> {
    let chosen: HashSet<String> = chosen_servers(row)
        .iter()
        .map(|name| name_to_key(name))
        .collect();
    saved
        .iter()
        .filter(|server| {
            chosen.contains(&name_to_key(&server.name)) && !is_cua_identity(&server.name)
        })
        .cloned()
        .collect()
}

/// The account's saved servers, read without the settings lock since
/// nothing is written.
pub(in crate::agent) fn read_saved_servers(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<Vec<AgentMcpServer>, String> {
    let config = account_config_dir_path(paths, user_id)
        .and_then(|dir| load_agent_config_file(&dir.join("config.json")))
        .map_err(|error| error.to_string())?;
    normalize_mcp_servers(config.mcp_servers)
}

fn transport_label(server: &AgentMcpServer) -> &'static str {
    match server.transport {
        AgentMcpTransport::Stdio { .. } => "stdio",
        AgentMcpTransport::StreamableHttp { .. } => "streamable_http",
    }
}

/// A task's rows in the MCP menu: every saved server, switched on when the
/// task has it, then the servers the task has that are no longer
/// configured, which can only be switched off.
fn session_rows(saved: &[AgentMcpServer], row: &TaskRow) -> Vec<AgentSessionMcpServer> {
    let chosen = chosen_servers(row);
    let chosen_keys: HashSet<String> = chosen.iter().map(|name| name_to_key(name)).collect();
    let saved_keys: HashSet<String> = saved
        .iter()
        .map(|server| name_to_key(&server.name))
        .collect();
    let configured = saved
        .iter()
        .filter(|server| !is_cua_identity(&server.name))
        .map(|server| AgentSessionMcpServer {
            name: server.name.clone(),
            kind: AgentSessionIntegrationKind::Mcp,
            display_name: server.name.clone(),
            description: server.description.clone(),
            transport: transport_label(server).to_string(),
            enabled: chosen_keys.contains(&name_to_key(&server.name)),
            available: true,
        });
    let missing = chosen
        .into_iter()
        .filter(|name| !saved_keys.contains(&name_to_key(name)) && !is_cua_identity(name))
        .map(|name| AgentSessionMcpServer {
            display_name: name.clone(),
            name,
            kind: AgentSessionIntegrationKind::Mcp,
            description: String::new(),
            transport: "unconfigured".to_string(),
            enabled: true,
            available: false,
        });
    configured.chain(missing).collect()
}

/// Where one server of a task is.
enum Status {
    Connecting,
    Connected,
    Failed(String),
}

struct Entry {
    config: AgentMcpServer,
    server: Arc<McpServer>,
    status: Status,
    /// A run waits for it to connect: not when it tries again after failing.
    awaited: bool,
    /// Its failure was reported, or is the same as the last one reported.
    reported: bool,
    last_error: Option<String>,
    instructions: Option<String>,
    /// The model's names for its registered tools, by the server's names.
    tools: HashMap<String, String>,
}

/// The MCP servers of one loaded task.
pub(crate) struct TaskMcp {
    cwd: PathBuf,
    search_path: Option<String>,
    /// The session's handle on this extension, once the session loads it.
    context: OnceLock<ExtensionContext>,
    /// By server key.
    servers: Mutex<HashMap<String, Entry>>,
    changed: Notify,
    last_used: Mutex<Option<Instant>>,
    weak: Weak<TaskMcp>,
}

impl TaskMcp {
    pub(crate) fn new(cwd: &Path, search_path: Option<String>) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            cwd: cwd.to_path_buf(),
            search_path,
            context: OnceLock::new(),
            servers: Mutex::new(HashMap::new()),
            changed: Notify::new(),
            last_used: Mutex::new(None),
            weak: weak.clone(),
        })
    }

    /// The extension the task's session loads: it registers the servers'
    /// tools, gives the prompt their instructions, and stops the servers
    /// when the session shuts down.
    pub(crate) fn extension(self: &Arc<Self>) -> Arc<dyn Extension> {
        let mcp = Arc::clone(self);
        extension("maple-mcp", move |api| {
            let _ = mcp.context.set(api.context());
            let prompt = Arc::clone(&mcp);
            api.on(move |event: BeforeAgentStart, _context| {
                let section = prompt.instructions_section();
                async move {
                    let mut options = event.options;
                    match section {
                        Some(section) => {
                            options
                                .sections
                                .insert(INSTRUCTIONS_SECTION.to_string(), section);
                        }
                        None => {
                            options.sections.shift_remove(INSTRUCTIONS_SECTION);
                        }
                    }
                    Ok(BeforeAgentStartResult {
                        options: Some(options),
                        ..BeforeAgentStartResult::default()
                    })
                }
            });
            let shutdown = Arc::clone(&mcp);
            api.on(move |_event: SessionShutdown, _context| {
                let shutdown = Arc::clone(&shutdown);
                async move {
                    shutdown.stop_all().await;
                    Ok(())
                }
            });
        })
    }

    fn servers(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        lock(&self.servers)
    }

    fn touch(&self) {
        *lock(&self.last_used) = Some(Instant::now());
    }

    /// When the task last ran or changed a server, while it has servers.
    pub(crate) fn last_used(&self) -> Option<Instant> {
        if self.servers().is_empty() {
            None
        } else {
            *lock(&self.last_used)
        }
    }

    /// A server for `config` that tells this task when its tools change.
    fn server(&self, key: &str, config: AgentMcpServer) -> Arc<McpServer> {
        let (mcp, key) = (self.weak.clone(), key.to_string());
        Arc::new_cyclic(|server: &Weak<McpServer>| {
            let server = server.clone();
            let tools_changed: ToolsChanged = Arc::new(move || {
                if let (Some(mcp), Some(server)) = (mcp.upgrade(), server.upgrade()) {
                    mcp.refresh(&key, server);
                }
            });
            McpServer::new(config, &self.cwd, self.search_path.clone(), tools_changed)
        })
    }

    fn entry(&self, key: &str, config: AgentMcpServer) -> Entry {
        let server = self.server(key, config.clone());
        self.connect(key, &server);
        Entry {
            config,
            server,
            status: Status::Connecting,
            awaited: true,
            reported: false,
            last_error: None,
            instructions: None,
            tools: HashMap::new(),
        }
    }

    /// Connect a server in the background.
    fn connect(&self, key: &str, server: &Arc<McpServer>) {
        let (mcp, key, server) = (self.weak.clone(), key.to_string(), Arc::clone(server));
        tokio::spawn(async move {
            let result = server.connect().await;
            if let Some(mcp) = mcp.upgrade() {
                mcp.connected(&key, &server, result);
            }
        });
    }

    fn connected(&self, key: &str, server: &Arc<McpServer>, result: Result<Listing, String>) {
        {
            let mut servers = self.servers();
            // Stopped or replaced while it connected.
            let Some(entry) = servers
                .get_mut(key)
                .filter(|entry| Arc::ptr_eq(&entry.server, server))
            else {
                return;
            };
            match result {
                Ok(listing) => {
                    entry.status = Status::Connected;
                    entry.last_error = None;
                    entry.reported = false;
                    entry.instructions = listing.instructions;
                    self.register(key, entry, listing.tools);
                }
                Err(error) => {
                    log::warn!(
                        "MCP server \"{}\" could not connect: {error}",
                        entry.config.name
                    );
                    entry.reported = entry.last_error.as_ref() == Some(&error);
                    entry.last_error = Some(error.clone());
                    entry.status = Status::Failed(error);
                }
            }
        }
        self.changed.notify_waiters();
    }

    /// Read a server's tools again: it said they changed, or it connected
    /// again.
    fn refresh(&self, key: &str, server: Arc<McpServer>) {
        let (mcp, key) = (self.weak.clone(), key.to_string());
        tokio::spawn(async move {
            let listing = match server.refresh().await {
                Ok(listing) => listing,
                Err(error) => {
                    log::debug!(
                        "MCP server \"{}\" changed its tools, but {error}",
                        server.name()
                    );
                    return;
                }
            };
            let Some(mcp) = mcp.upgrade() else {
                return;
            };
            let mut servers = mcp.servers();
            if let Some(entry) = servers.get_mut(&key).filter(|entry| {
                Arc::ptr_eq(&entry.server, &server) && matches!(entry.status, Status::Connected)
            }) {
                mcp.register(&key, entry, listing.tools);
            }
        });
    }

    /// Give the session a server's tools as they are now: new ones join,
    /// changed ones are replaced, and withdrawn ones leave. A tool keeps
    /// the name it was given; names taken by other tools, or shared by
    /// two of this server's tools once cleaned, end in a hash.
    fn register(&self, key: &str, entry: &mut Entry, tools: Vec<rmcp::model::Tool>) {
        let Some(context) = self.context.get() else {
            return;
        };
        let own: HashSet<String> = entry.tools.values().cloned().collect();
        let others: HashSet<String> = context
            .tool_names()
            .into_iter()
            .filter(|name| !own.contains(name))
            .collect();
        let mut plain: HashMap<String, usize> = HashMap::new();
        for tool in &tools {
            *plain
                .entry(tool_name(key, &tool.name, |_| false))
                .or_default() += 1;
        }
        let mut names: HashMap<String, String> = HashMap::new();
        let mut assigned: HashSet<String> = HashSet::new();
        for tool in &tools {
            let name = entry
                .tools
                .get(tool.name.as_ref())
                .filter(|name| !assigned.contains(*name))
                .cloned()
                .unwrap_or_else(|| {
                    tool_name(key, &tool.name, |candidate| {
                        others.contains(candidate)
                            || assigned.contains(candidate)
                            || plain.get(candidate).is_some_and(|count| *count > 1)
                    })
                });
            assigned.insert(name.clone());
            names.insert(tool.name.to_string(), name);
        }
        for (server_name, name) in &entry.tools {
            if !names.contains_key(server_name) {
                context.unregister_tool(name);
            }
        }
        for tool in &tools {
            let name = names[tool.name.as_ref()].clone();
            let declaration = pi_ai::Tool::new(
                name,
                description(&entry.config.name, tool),
                parameters(&tool.input_schema),
            );
            context.register_tool(
                Arc::new(McpTool {
                    declaration,
                    server: Arc::clone(&entry.server),
                    server_name: entry.config.name.clone(),
                    tool: tool.name.to_string(),
                }),
                ToolPrompt::default(),
                true,
            );
        }
        entry.tools = names;
    }

    fn unregister(&self, entry: &Entry) {
        if let Some(context) = self.context.get() {
            for name in entry.tools.values() {
                context.unregister_tool(name);
            }
        }
    }

    /// Run `servers` for the task: start those not running, restart those
    /// whose settings changed, and stop those no longer chosen. A server
    /// that failed tries again, and runs do not wait for it.
    pub(crate) fn sync(&self, wanted: Vec<AgentMcpServer>) {
        self.touch();
        let wanted: HashMap<String, AgentMcpServer> = wanted
            .into_iter()
            .map(|server| (name_to_key(&server.name), server))
            .collect();
        let mut stopped = Vec::new();
        {
            let mut servers = self.servers();
            let outdated: Vec<String> = servers
                .iter()
                .filter(|(key, entry)| wanted.get(*key) != Some(&entry.config))
                .map(|(key, _)| key.clone())
                .collect();
            for key in outdated {
                if let Some(entry) = servers.remove(&key) {
                    self.unregister(&entry);
                    stopped.push(entry.server);
                }
            }
            for (key, config) in wanted {
                match servers.get_mut(&key) {
                    Some(entry) => {
                        if matches!(entry.status, Status::Failed(_)) {
                            entry.status = Status::Connecting;
                            entry.awaited = false;
                            self.connect(&key, &entry.server);
                        }
                    }
                    None => {
                        let entry = self.entry(&key, config);
                        servers.insert(key, entry);
                    }
                }
            }
        }
        for server in stopped {
            tokio::spawn(async move { server.shutdown().await });
        }
        self.changed.notify_waiters();
    }

    /// Wait for the servers a run waits for, at most `limit`. Returns
    /// whether none is still connecting.
    pub(crate) async fn wait(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let waiting = self
                .servers()
                .values()
                .any(|entry| entry.awaited && matches!(entry.status, Status::Connecting));
            if !waiting {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return false;
            }
        }
    }

    /// The notice for a run: servers that failed and were not reported yet,
    /// with `also_failed` beside them, such as computer use, and, when the
    /// wait ran out, those still connecting.
    pub(crate) fn take_notice(&self, also_failed: Option<(String, String)>) -> Option<String> {
        let mut servers = self.servers();
        let mut failed: Vec<(String, String)> = also_failed.into_iter().collect();
        let mut connecting: Vec<String> = Vec::new();
        for entry in servers.values_mut() {
            match &entry.status {
                Status::Failed(error) if !entry.reported => {
                    entry.reported = true;
                    failed.push((entry.config.name.clone(), error.clone()));
                }
                Status::Connecting if entry.awaited => {
                    connecting.push(entry.config.name.clone());
                }
                _ => {}
            }
        }
        failed.sort();
        connecting.sort();
        let mut parts = Vec::new();
        if !failed.is_empty() {
            parts.push(failures_notice(&failed));
        }
        if !connecting.is_empty() {
            parts.push(format!(
                "Still connecting: {}. Their tools join the task once they connect.",
                connecting.join(", ")
            ));
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    /// Switch a server on for the task now, and wait a little for it to
    /// connect. Fails with why it could not connect, when it fails in time.
    pub(crate) async fn enable(&self, config: AgentMcpServer) -> Result<(), String> {
        self.touch();
        let key = name_to_key(&config.name);
        let (server, replaced) = {
            let mut servers = self.servers();
            match servers.get(&key) {
                Some(entry)
                    if entry.config == config && !matches!(entry.status, Status::Failed(_)) =>
                {
                    (Arc::clone(&entry.server), None)
                }
                _ => {
                    let replaced = servers.remove(&key).map(|entry| {
                        self.unregister(&entry);
                        entry.server
                    });
                    let entry = self.entry(&key, config);
                    let server = Arc::clone(&entry.server);
                    servers.insert(key.clone(), entry);
                    (server, replaced)
                }
            }
        };
        if let Some(replaced) = replaced {
            tokio::spawn(async move { replaced.shutdown().await });
        }
        let deadline = tokio::time::Instant::now() + STARTUP_WAIT;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut servers = self.servers();
                let Some(entry) = servers
                    .get_mut(&key)
                    .filter(|entry| Arc::ptr_eq(&entry.server, &server))
                else {
                    return Ok(());
                };
                match &entry.status {
                    Status::Connected => return Ok(()),
                    Status::Failed(error) => {
                        entry.reported = true;
                        return Err(error.clone());
                    }
                    Status::Connecting => {}
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Ok(());
            }
        }
    }

    /// Switch a server off for the task and stop it.
    pub(crate) async fn disable(&self, key: &str) {
        let entry = self.servers().remove(key);
        if let Some(entry) = entry {
            self.unregister(&entry);
            entry.server.shutdown().await;
        }
        self.changed.notify_waiters();
    }

    /// Stop every server; the next run starts them again.
    pub(crate) async fn stop_all(&self) {
        let servers = self.take_all();
        futures_util::future::join_all(servers.iter().map(|server| server.shutdown())).await;
    }

    /// Take every server from the task, with its tools, and return them
    /// to be shut down.
    pub(crate) fn take_all(&self) -> Vec<Arc<McpServer>> {
        let entries: Vec<Entry> = self.servers().drain().map(|(_, entry)| entry).collect();
        for entry in &entries {
            self.unregister(entry);
        }
        self.changed.notify_waiters();
        entries.into_iter().map(|entry| entry.server).collect()
    }

    /// The prompt section with the instructions of the servers whose tools
    /// the model has.
    fn instructions_section(&self) -> Option<String> {
        let servers = self.servers();
        let mut listed: Vec<(&String, &Entry)> = servers
            .iter()
            .filter(|(_, entry)| {
                matches!(entry.status, Status::Connected)
                    && !entry.tools.is_empty()
                    && entry.instructions.is_some()
            })
            .collect();
        if listed.is_empty() {
            return None;
        }
        listed.sort_by(|a, b| a.0.cmp(b.0));
        let mut section =
            "The MCP servers whose tools you have explained how to use them:".to_string();
        for (key, entry) in listed {
            let prefix = tool_name(key, "", |_| false);
            let instructions = bounded_timeline_text(
                entry.instructions.as_deref().unwrap_or_default(),
                MAX_INSTRUCTIONS_CHARS,
            );
            section.push_str(&format!(
                "\n\n## {} ({prefix}*)\n{instructions}",
                entry.config.name
            ));
        }
        Some(section)
    }
}

/// The notice for servers that failed to connect, as Maple worded it with
/// Goose: the first few, each shortened.
fn failures_notice(failed: &[(String, String)]) -> String {
    let mut details: Vec<String> = failed
        .iter()
        .take(MAX_REPORTED_FAILURES)
        .map(|(name, error)| {
            format!(
                "{}: {}",
                bounded_timeline_text(name, super::MAX_MCP_SERVER_NAME_CHARS),
                bounded_timeline_text(error, MAX_FAILURE_CHARS)
            )
        })
        .collect();
    let remaining = failed.len().saturating_sub(details.len());
    if remaining > 0 {
        details.push(format!("and {remaining} more"));
    }
    bounded_timeline_text(
        &format!("{FAILURES_PREFIX} {}", details.join("; ")),
        super::super::timeline::MAX_AGENT_ERROR_CHARS,
    )
}

/// One of a server's tools, as the model calls it.
struct McpTool {
    declaration: pi_ai::Tool,
    server: Arc<McpServer>,
    server_name: String,
    tool: String,
}

#[async_trait]
impl AgentTool for McpTool {
    fn declaration(&self) -> &pi_ai::Tool {
        &self.declaration
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let arguments = match invocation.args {
            Value::Object(arguments) => arguments,
            _ => Default::default(),
        };
        let result = self
            .server
            .call_tool(&self.tool, arguments, invocation.cancel, invocation.updates)
            .await?;
        Ok(convert_result(&self.server_name, &self.tool, result))
    }
}

impl AgentRuntimeHandle {
    /// A task's rows in the MCP menu.
    pub async fn list_session_mcp_servers(
        &self,
        session_id: String,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        self.verify_generation().await?;
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err("Agent task ID cannot be empty".to_string());
        }
        let row = self
            .store()?
            .get(session_id)?
            .ok_or_else(|| format!("Failed to find Agent task {session_id}"))?;
        self.menu_rows(&row).await
    }

    /// Switch an MCP server on or off for a task. A task whose session is
    /// loaded connects the server now, and keeps it off when it fails to
    /// connect; one that is not loaded connects it when it next runs.
    pub async fn set_session_mcp_server_enabled(
        &self,
        request: AgentSetSessionMcpServerRequest,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let session_id = request.session_id.trim().to_string();
        if session_id.is_empty() {
            return Err("Agent task ID cannot be empty".to_string());
        }
        let name = request.name.trim();
        if request.kind == AgentSessionIntegrationKind::ExternalAgent {
            let (row, cards) = self
                .set_task_external_agent(&session_id, name, request.enabled)
                .await?;
            let saved = self.list_mcp_servers().await?;
            return Ok(self.menu_rows_with(&saved, &cards, &row));
        }
        if is_cua_identity(name) {
            let row = self.set_task_cua(&session_id, request.enabled).await?;
            return self.menu_rows(&row).await;
        }
        let key = name_to_key(name);
        if key.is_empty() || RESERVED_KEYS.contains(&key.as_str()) {
            return Err("That MCP server cannot be changed".to_string());
        }
        let runtime = self.runtime().await?;
        if runtime.runs.is_running(&session_id) {
            return Err("Stop the running agent before changing MCP servers".to_string());
        }
        let store = self.store()?;
        if store.get(&session_id)?.is_none() {
            return Err(format!("Failed to find Agent task {session_id}"));
        }
        let saved = self.list_mcp_servers().await?;
        let loaded = runtime.loaded_mcp(&session_id).await;
        let row = if request.enabled {
            let server = saved
                .iter()
                .find(|server| name_to_key(&server.name) == key)
                .cloned()
                .ok_or_else(|| {
                    format!("MCP server '{name}' is no longer configured and cannot be enabled")
                })?;
            if let Some(mcp) = &loaded
                && let Err(error) = mcp.enable(server.clone()).await
            {
                mcp.disable(&key).await;
                return Err(format!(
                    "Failed to connect MCP server '{}': {error}",
                    server.name
                ));
            }
            store.update(&session_id, |row| {
                let mut names = chosen_servers(row);
                names.retain(|chosen| name_to_key(chosen) != key);
                names.push(server.name.clone());
                set_chosen_servers(row, names);
            })?
        } else {
            let row = store.update(&session_id, |row| {
                let mut names = chosen_servers(row);
                names.retain(|chosen| name_to_key(chosen) != key);
                set_chosen_servers(row, names);
            })?;
            if let Some(mcp) = &loaded {
                mcp.disable(&key).await;
            }
            row
        };
        let row = row.ok_or_else(|| format!("Failed to find Agent task {session_id}"))?;
        let cards = match row.kind {
            TaskKind::Desktop => self.external_agent_cards().await?,
            TaskKind::Acp => Vec::new(),
        };
        Ok(self.menu_rows_with(&saved, &cards, &row))
    }

    /// A task's whole MCP menu, read now.
    async fn menu_rows(&self, row: &TaskRow) -> Result<Vec<AgentSessionMcpServer>, String> {
        let saved = self.list_mcp_servers().await?;
        let cards = match row.kind {
            TaskKind::Desktop => self.external_agent_cards().await?,
            TaskKind::Acp => Vec::new(),
        };
        Ok(self.menu_rows_with(&saved, &cards, row))
    }

    /// A task's whole MCP menu: its servers, then the external agents
    /// switched on in Settings, then built-in CUA.
    fn menu_rows_with(
        &self,
        saved: &[AgentMcpServer],
        agent_cards: &[AgentIntegration],
        row: &TaskRow,
    ) -> Vec<AgentSessionMcpServer> {
        let mut rows = session_rows(saved, row);
        rows.extend(external_agents::session_rows(agent_cards, row));
        if row.kind == TaskKind::Desktop {
            let device = cua_default(self.paths(), &self.user_id);
            let ready = (device.is_some() || cua::task_choice(row).is_some()) && cua::ready();
            rows.extend(cua::session_row(row, device, ready));
        }
        rows
    }
}

#[cfg(test)]
mod tests;
