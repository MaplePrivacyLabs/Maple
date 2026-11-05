//! One MCP server's connection, over rmcp: a stdio server Maple starts in its
//! own process group, or a streamable HTTP endpoint. As in Pi, the server
//! connects when first needed, a dropped connection is opened again on the
//! next call, and a stdio server is stopped by closing its input, then
//! SIGTERM, then SIGKILL to its whole process group.
//!
//! An HTTP server is opened on the stateless 2026-07-28 revision when it
//! offers it, and with the older `initialize` handshake when it does not. A
//! stdio server keeps the handshake: an older one may only answer the
//! revision's probe by timing out, which would cost every task ten seconds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use http::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotificationParam,
    ClientCapabilities, ClientConfig, ClientRequest, Implementation, JsonObject,
    ProgressNotificationParam, ProgressToken, ProtocolVersion, ServerNotification, ServerResult,
    SubscriptionFilter, Tool,
};
// Roots, which newer drafts of MCP drop; see `Client::list_roots`.
#[allow(deprecated)]
use rmcp::model::{ListRootsResult, Root};
use rmcp::service::{
    ClientInitializeError, ClientLifecycleMode, ClientServiceExt, NotificationContext, Peer,
    PeerRequestOptions, RequestContext, RunningService,
};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{ClientHandler, ErrorData, RoleClient, ServiceError};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use super::super::{AgentMcpKeyValue, AgentMcpServer, AgentMcpTransport};
use super::split_command;

/// How long a stdio server has to exit after its input closes, before SIGTERM.
const STDIN_CLOSE_GRACE: Duration = Duration::from_millis(500);
/// How long a stdio server has after SIGTERM, before SIGKILL.
const TERMINATE_GRACE: Duration = Duration::from_secs(2);
/// The end of a stdio server's error output kept to explain a failure.
const STDERR_TAIL_BYTES: usize = 2_000;
/// How long after a stateless server's tool-change stream ends before it is
/// opened again.
const TOOLS_WATCH_RETRY: Duration = Duration::from_secs(30);

/// Something to tell the task about a server: its tools changed, or it
/// connected again and may offer others.
pub(crate) type ToolsChanged = Arc<dyn Fn() + Send + Sync>;

/// The tool calls waiting for progress, by token.
type Progress = Arc<StdMutex<HashMap<ProgressToken, pi_agent_core::ToolUpdates>>>;

/// What a connected server offers.
#[derive(Clone, Debug, Default)]
pub(crate) struct Listing {
    pub(crate) tools: Vec<Tool>,
    /// How to use the server, from its first answer.
    pub(crate) instructions: Option<String>,
}

/// The client side of one connection: who Maple is, the task's folder as a
/// root for a server on this computer, and where progress and tool-list
/// changes go. A remote server is not told where the task lives.
#[allow(deprecated)]
struct Client {
    root: Option<Root>,
    progress: Progress,
    tools_changed: ToolsChanged,
}

impl ClientHandler for Client {
    fn get_info(&self) -> ClientConfig {
        let mut capabilities = ClientCapabilities::default();
        if self.root.is_some() {
            capabilities.roots = Some(Default::default());
        }
        // rmcp's default is the newest revision, which has no handshake, so
        // this is the version a stdio server's `initialize` carries.
        ClientConfig::new(
            capabilities,
            Implementation::new("maple", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
    }

    // Newer drafts of MCP drop roots; servers on today's protocol still read
    // them to learn the folder they may work in, as Pi offers it.
    #[allow(deprecated)]
    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Ok(ListRootsResult::new(self.root.iter().cloned().collect()))
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let updates = lock(&self.progress).get(&params.progress_token).cloned();
        if let Some(updates) = updates {
            let total = params
                .total
                .map(|total| format!("/{total}"))
                .unwrap_or_default();
            let text = params
                .message
                .unwrap_or_else(|| format!("Progress {}{total}", params.progress));
            updates.send(pi_agent_core::AgentToolResult::text(text));
        }
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        (self.tools_changed)();
    }
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Stateless when the HTTP server offers it, else the handshake.
pub(super) fn http_lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
    }
}

/// Why a server could not open: when neither way worked, the handshake's
/// reason, since the probe failing is what an older server does anyway.
fn open_failed(error: ClientInitializeError) -> String {
    match error {
        ClientInitializeError::LegacyFallbackFailed { fallback, .. } => fallback.to_string(),
        error => error.to_string(),
    }
}

/// Whether a server sends tool-list changes only on a stream the client
/// opens: on the stateless revision there is no session for them to arrive on.
fn needs_tools_watch(peer: &Peer<RoleClient>) -> bool {
    peer.peer_info().is_some_and(|info| {
        !info.protocol_version.has_initialize()
            && info
                .capabilities
                .tools
                .as_ref()
                .is_some_and(|tools| tools.list_changed == Some(true))
    })
}

/// Tell the task of a stateless server's tool-list changes for as long as the
/// connection lasts. A stream that ends is opened again, and may have missed
/// a change. A server that never opens one has nothing to watch.
async fn watch_tools(peer: Peer<RoleClient>, tools_changed: ToolsChanged) {
    let mut filter = SubscriptionFilter::new();
    filter.tools_list_changed = Some(true);
    let mut watched = false;
    while !peer.is_transport_closed() {
        match peer.listen(filter.clone()).await {
            Ok(mut subscription) => {
                if watched {
                    tools_changed();
                }
                watched = true;
                while let Ok(Some(notification)) = subscription.next().await {
                    if matches!(
                        notification,
                        ServerNotification::ToolListChangedNotification(_)
                    ) {
                        tools_changed();
                    }
                }
            }
            Err(_) if !watched => {
                log::debug!("MCP server tool-change stream did not open");
                return;
            }
            Err(_) => log::debug!("MCP server tool-change stream did not reopen"),
        }
        tokio::time::sleep(TOOLS_WATCH_RETRY).await;
    }
}

/// A stdio server's process and the end of its error output.
struct StdioProcess {
    child: tokio::process::Child,
    stderr_tail: Arc<StdMutex<Vec<u8>>>,
    stderr_reader: Option<tokio::task::JoinHandle<()>>,
}

impl StdioProcess {
    /// Stop a server that failed to connect, and say why with the end of
    /// what it wrote to its error output.
    async fn failed(mut self, error: String) -> String {
        let reader = self.stderr_reader.take();
        let tail = Arc::clone(&self.stderr_tail);
        self.stop().await;
        // What it wrote before it exited may still be on its way.
        if let Some(reader) = reader {
            let _ = tokio::time::timeout(STDIN_CLOSE_GRACE, reader).await;
        }
        let tail = String::from_utf8_lossy(&lock(&tail)).trim().to_string();
        if tail.is_empty() {
            error
        } else {
            format!("{error}\n{tail}")
        }
    }

    /// Stop the server as the MCP specification asks: its input is already
    /// closed; then SIGTERM, then SIGKILL, to everything it started.
    async fn stop(mut self) {
        if tokio::time::timeout(STDIN_CLOSE_GRACE, self.child.wait())
            .await
            .is_ok()
        {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self
            .child
            .id()
            .and_then(|pid| libc::pid_t::try_from(pid).ok())
        {
            // SAFETY: kill only sends a signal; a negative pid names the process group.
            unsafe { libc::kill(-pid, libc::SIGTERM) };
            if tokio::time::timeout(TERMINATE_GRACE, self.child.wait())
                .await
                .is_ok()
            {
                return;
            }
        }
        if let Some(pid) = self.child.id() {
            pi_coding_agent::tools::kill_process_tree(pid);
        }
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(TERMINATE_GRACE, self.child.wait()).await;
    }
}

impl Drop for StdioProcess {
    /// A server dropped without [`StdioProcess::stop`], as when a connection
    /// being opened is abandoned, takes everything it started with it. One
    /// that exited is left alone: its id may name another process by now.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None))
            && let Some(pid) = self.child.id()
        {
            pi_coding_agent::tools::kill_process_tree(pid);
        }
    }
}

/// An open connection and what it found.
struct Connection {
    service: RunningService<RoleClient, Client>,
    progress: Progress,
    listing: Listing,
    stdio: Option<StdioProcess>,
    tools_watch: Option<tokio::task::JoinHandle<()>>,
    oauth_generation: Option<String>,
}

/// One MCP server of a task.
pub(crate) struct McpServer {
    config: AgentMcpServer,
    cwd: PathBuf,
    /// The PATH stdio servers start with: the login shell's, when known.
    search_path: Option<String>,
    tools_changed: ToolsChanged,
    connection: tokio::sync::Mutex<Option<Connection>>,
    shutdown: CancellationToken,
    oauth: Option<super::oauth::OAuthAccountContext>,
}

impl McpServer {
    pub(crate) fn new(
        config: AgentMcpServer,
        cwd: &Path,
        search_path: Option<String>,
        tools_changed: ToolsChanged,
    ) -> Self {
        Self {
            config,
            cwd: cwd.to_path_buf(),
            search_path,
            tools_changed,
            connection: tokio::sync::Mutex::new(None),
            shutdown: CancellationToken::new(),
            oauth: None,
        }
    }

    pub(crate) fn with_oauth_context(
        mut self,
        oauth: Option<super::oauth::OAuthAccountContext>,
    ) -> Self {
        self.oauth = oauth;
        self
    }

    pub(crate) fn name(&self) -> &str {
        &self.config.name
    }

    /// The per-request timeout, which progress resets.
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.timeout_seconds.max(1))
    }

    /// What the server offers, connecting first when it is not connected.
    /// The error says why it could not connect.
    pub(crate) async fn connect(&self) -> Result<Listing, String> {
        let mut connection = self.connection.lock().await;
        self.open(&mut connection).await?;
        Ok(connection
            .as_ref()
            .map(|open| open.listing.clone())
            .unwrap_or_default())
    }

    /// Read the tool list again, after the server said it changed.
    pub(crate) async fn refresh(&self) -> Result<Listing, String> {
        let mut connection = self.connection.lock().await;
        let Some(open) = connection.as_mut() else {
            return Err("it is not connected".to_string());
        };
        let tools = tokio::time::timeout(self.timeout(), open.service.peer().list_all_tools())
            .await
            .map_err(|_| "listing its tools took too long".to_string())?
            .map_err(|error| self.without_secrets(format!("listing its tools failed: {error}")))?;
        open.listing.tools = tools;
        Ok(open.listing.clone())
    }

    /// Connect when there is no open connection, or the last one dropped. A
    /// connection opened again tells the task, which reads its tools anew.
    async fn open(&self, connection: &mut Option<Connection>) -> Result<(), String> {
        if self.shutdown.is_cancelled() {
            return Err("it was stopped".to_string());
        }
        let generation = match (&self.oauth, &self.config.transport) {
            (Some(context), AgentMcpTransport::StreamableHttp { url, .. }) => {
                context.active_generation(&super::name_to_key(&self.config.name), url)?
            }
            _ => None,
        };
        if connection
            .as_ref()
            .is_some_and(|open| !open.service.is_closed() && open.oauth_generation == generation)
        {
            return Ok(());
        }
        let dropped = connection.take();
        let reopened = dropped.is_some();
        if let Some(dropped) = dropped {
            close(dropped).await;
        }
        let opened = tokio::select! {
            opened = self.open_connection() => opened,
            _ = self.shutdown.cancelled() => Err("it was stopped".to_string()),
        };
        *connection = Some(opened.map_err(|error| self.without_secrets(error))?);
        if reopened {
            (self.tools_changed)();
        }
        Ok(())
    }

    #[allow(deprecated)]
    fn client(&self, progress: &Progress) -> Client {
        let name = self
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let local = matches!(self.config.transport, AgentMcpTransport::Stdio { .. });
        Client {
            root: local.then(|| Root::new(url_of_path(&self.cwd)).with_name(name)),
            progress: Arc::clone(progress),
            tools_changed: Arc::clone(&self.tools_changed),
        }
    }

    async fn open_connection(&self) -> Result<Connection, String> {
        let progress = Progress::default();
        match &self.config.transport {
            AgentMcpTransport::Stdio {
                command,
                environment,
            } => {
                let mut stdio = self.spawn(command, environment)?;
                let stdout = stdio.child.stdout.take();
                let stdin = stdio.child.stdin.take();
                let (Some(stdout), Some(stdin)) = (stdout, stdin) else {
                    stdio.stop().await;
                    return Err("its input and output could not be opened".into());
                };
                let transport = AsyncRwTransport::new_client(stdout, stdin);
                match self
                    .handshake(transport, ClientLifecycleMode::Initialize, &progress)
                    .await
                {
                    Ok((service, listing)) => Ok(Connection {
                        service,
                        progress,
                        listing,
                        stdio: Some(stdio),
                        tools_watch: None,
                        oauth_generation: None,
                    }),
                    Err(error) => Err(stdio.failed(error).await),
                }
            }
            AgentMcpTransport::StreamableHttp {
                url,
                environment,
                headers,
            } => {
                if let Some(context) = &self.oauth
                    && let Some(client) = super::oauth::runtime_client(
                        context,
                        &super::name_to_key(&self.config.name),
                        url,
                    )
                    .await?
                {
                    if !environment.is_empty() || !headers.is_empty() {
                        return Err(
                            "OAuth MCP endpoints cannot also carry custom credentials".into()
                        );
                    }
                    let oauth_generation = client.generation();
                    let transport = StreamableHttpClientTransport::with_client(
                        client,
                        StreamableHttpClientTransportConfig::with_uri(url.as_str()),
                    );
                    let (service, listing) = self
                        .handshake(transport, http_lifecycle(), &progress)
                        .await?;
                    let tools_watch = self.tools_watch(&service);
                    return Ok(Connection {
                        service,
                        progress,
                        listing,
                        stdio: None,
                        tools_watch,
                        oauth_generation,
                    });
                }
                let mut custom = HashMap::new();
                for header in headers {
                    let key = HeaderName::try_from(header.key.as_str())
                        .map_err(|error| format!("header {}: {error}", header.key))?;
                    let value = HeaderValue::try_from(substitute(&header.value, environment))
                        .map_err(|error| format!("header {}: {error}", header.key))?;
                    custom.insert(key, value);
                }
                // Redirects are not followed: they would take the headers,
                // credentials among them, somewhere else.
                let client = reqwest::Client::builder()
                    .user_agent(concat!("maple/", env!("CARGO_PKG_VERSION")))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|error| error.to_string())?;
                let transport = StreamableHttpClientTransport::with_client(
                    client,
                    StreamableHttpClientTransportConfig::with_uri(substitute(url, environment))
                        .custom_headers(custom),
                );
                let (service, listing) = self
                    .handshake(transport, http_lifecycle(), &progress)
                    .await?;
                let tools_watch = self.tools_watch(&service);
                Ok(Connection {
                    service,
                    progress,
                    listing,
                    stdio: None,
                    tools_watch,
                    oauth_generation: None,
                })
            }
        }
    }

    fn tools_watch(
        &self,
        service: &RunningService<RoleClient, Client>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let peer = service.peer();
        needs_tools_watch(peer)
            .then(|| tokio::spawn(watch_tools(peer.clone(), Arc::clone(&self.tools_changed))))
    }

    /// Open the connection, then list the tools, each within the timeout.
    async fn handshake<T, E, A>(
        &self,
        transport: T,
        lifecycle: ClientLifecycleMode,
        progress: &Progress,
    ) -> Result<(RunningService<RoleClient, Client>, Listing), String>
    where
        T: rmcp::transport::IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let timeout = self.timeout();
        let mut service = tokio::time::timeout(
            timeout,
            self.client(progress)
                .serve_with_lifecycle(transport, lifecycle),
        )
        .await
        .map_err(|_| "it did not answer in time".to_string())?
        .map_err(open_failed)?;
        let info = service.peer().peer_info();
        let instructions = info
            .as_ref()
            .and_then(|info| info.instructions.clone())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let has_tools = info
            .as_ref()
            .is_some_and(|info| info.capabilities.tools.is_some());
        // Servers without tools, with only prompts or resources, do not answer tools/list.
        let tools = if has_tools {
            match tokio::time::timeout(timeout, service.peer().list_all_tools()).await {
                Ok(Ok(tools)) => tools,
                Ok(Err(error)) => {
                    let _ = service.close_with_timeout(STDIN_CLOSE_GRACE).await;
                    return Err(format!("listing its tools failed: {error}"));
                }
                Err(_) => {
                    let _ = service.close_with_timeout(STDIN_CLOSE_GRACE).await;
                    return Err("listing its tools took too long".to_string());
                }
            }
        } else {
            Vec::new()
        };
        Ok((
            service,
            Listing {
                tools,
                instructions,
            },
        ))
    }

    /// Start a stdio server in the task's folder, in its own process group,
    /// with the login shell's PATH and the server's environment.
    fn spawn(
        &self,
        command: &str,
        environment: &[AgentMcpKeyValue],
    ) -> Result<StdioProcess, String> {
        let words: Vec<String> = split_command(command)?
            .iter()
            .map(|word| expand_home(word))
            .collect();
        let Some((program, args)) = words.split_first() else {
            return Err("its command is empty".to_string());
        };
        let search_path = self.search_path.as_deref();
        // A bare name is looked up on the login shell's PATH, which on
        // Windows also finds npm's `.cmd` shims.
        let program = if Path::new(program).components().count() == 1 {
            super::super::integrations::find_executable(program, search_path)
                .unwrap_or_else(|| PathBuf::from(program))
        } else {
            PathBuf::from(program)
        };
        let mut process = tokio::process::Command::new(&program);
        process
            .args(args)
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(path) = search_path {
            process.env("PATH", path);
        }
        for variable in environment {
            process.env(&variable.key, &variable.value);
        }
        #[cfg(unix)]
        process.process_group(0);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            process.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = process
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", program.display()))?;
        let stderr_tail = Arc::new(StdMutex::new(Vec::new()));
        let stderr_reader = child.stderr.take().map(|mut stderr| {
            let tail = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut chunk = [0u8; 4096];
                while let Ok(read) = stderr.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    let mut tail = lock(&tail);
                    tail.extend_from_slice(&chunk[..read]);
                    let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                    tail.drain(..excess);
                }
            })
        });
        Ok(StdioProcess {
            child,
            stderr_tail,
            stderr_reader,
        })
    }

    /// Call a tool. A dropped connection opens again first; a call is never
    /// sent twice, since the server may already have run it. Stop sends the
    /// server a cancellation. Errors are written for the model.
    pub(crate) async fn call_tool(
        &self,
        tool: &str,
        arguments: JsonObject,
        cancel: CancellationToken,
        updates: pi_agent_core::ToolUpdates,
    ) -> Result<CallToolResult, String> {
        let (peer, progress) = {
            let mut connection = self.connection.lock().await;
            self.open(&mut connection).await.map_err(|error| {
                format!("MCP server \"{}\" is not connected: {error}", self.name())
            })?;
            let open = connection
                .as_ref()
                .ok_or_else(|| format!("MCP server \"{}\" is not connected", self.name()))?;
            (open.service.peer().clone(), Arc::clone(&open.progress))
        };
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            CallToolRequestParams::new(tool.to_string()).with_arguments(arguments),
        ));
        let options = PeerRequestOptions::with_timeout(self.timeout()).reset_timeout_on_progress();
        let handle = peer
            .send_request_with_option(request, options)
            .await
            .map_err(|error| self.call_failed(tool, error))?;
        let (id, token) = (handle.id.clone(), handle.progress_token.clone());
        lock(&progress).insert(token.clone(), updates);
        let response = tokio::select! {
            response = handle.await_response() => Some(response),
            _ = cancel.cancelled() => None,
        };
        lock(&progress).remove(&token);
        match response {
            Some(Ok(ServerResult::CallToolResult(result))) => Ok(result),
            Some(Ok(_)) => Err(format!(
                "MCP server \"{}\" answered {tool} with something other than a result",
                self.name()
            )),
            Some(Err(error)) => Err(self.call_failed(tool, error)),
            None => {
                let _ = peer
                    .notify_cancelled(CancelledNotificationParam::new(
                        Some(id),
                        Some("The user stopped the run".into()),
                    ))
                    .await;
                Err(format!("{tool} was cancelled"))
            }
        }
    }

    /// A failed call's message.
    fn call_failed(&self, tool: &str, error: ServiceError) -> String {
        let message = match &error {
            ServiceError::McpError(error) => error.message.to_string(),
            ServiceError::Timeout { timeout } => {
                format!("it did not answer within {} seconds", timeout.as_secs())
            }
            other => other.to_string(),
        };
        self.without_secrets(format!(
            "MCP server \"{}\" failed to run {tool}: {message}",
            self.name()
        ))
    }

    /// An error's text with an HTTP server's endpoint and saved values taken
    /// out. HTTP errors quote the URL, and these reach the log and the task.
    fn without_secrets(&self, mut text: String) -> String {
        let AgentMcpTransport::StreamableHttp {
            url,
            environment,
            headers,
        } = &self.config.transport
        else {
            return text;
        };
        let shown = url.split(['?', '#']).next().unwrap_or_default();
        let endpoint = substitute(url, environment);
        if let Ok(parsed) = reqwest::Url::parse(&endpoint) {
            text = text.replace(parsed.as_str(), shown);
        }
        text = text.replace(&endpoint, shown);
        for value in environment.iter().chain(headers).map(|entry| &entry.value) {
            let value = substitute(value, environment);
            if !value.is_empty() {
                text = text.replace(&value, "[hidden]");
            }
        }
        text
    }

    /// Close the connection and stop a stdio server. The server cannot
    /// connect again afterwards.
    pub(crate) async fn shutdown(&self) {
        self.shutdown.cancel();
        if let Some(open) = self.connection.lock().await.take() {
            close(open).await;
        }
    }
}

async fn close(mut open: Connection) {
    if let Some(watch) = open.tools_watch.take() {
        watch.abort();
    }
    let _ = open.service.close_with_timeout(STDIN_CLOSE_GRACE).await;
    if let Some(stdio) = open.stdio.take() {
        stdio.stop().await;
    }
}

/// `file://` URL of a folder, for the server's root.
fn url_of_path(path: &Path) -> String {
    match reqwest::Url::from_directory_path(path) {
        Ok(url) => url.to_string(),
        Err(()) => format!("file://{}", path.display()),
    }
}

/// A word starting with `~/` (or `~` alone) from the home folder, as Pi
/// reads a server's command and arguments.
fn expand_home(word: &str) -> String {
    let rest = match word.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => rest,
        _ => return word.to_string(),
    };
    match super::super::config::home_dir() {
        Some(home) => format!("{}{rest}", home.display()),
        None => word.to_string(),
    }
}

/// `${NAME}` and `$NAME` in an HTTP server's endpoint or header value, from
/// the server's environment entries, which exist for this. A name without
/// an entry is left as written.
pub(super) fn substitute(value: &str, environment: &[AgentMcpKeyValue]) -> String {
    let lookup = |name: &str| {
        environment
            .iter()
            .find(|entry| entry.key == name)
            .map(|entry| entry.value.as_str())
    };
    let is_name_start = |c: char| c.is_ascii_alphabetic() || c == '_';
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(braced) = after.strip_prefix('{')
            && let Some(close) = braced.find('}')
        {
            let name = braced[..close].trim();
            if name.starts_with(is_name_start)
                && name.chars().all(is_name_char)
                && let Some(found) = lookup(name)
            {
                out.push_str(found);
                rest = &braced[close + 1..];
                continue;
            }
        } else if after.starts_with(is_name_start) {
            let end = after
                .find(|c: char| !is_name_char(c))
                .unwrap_or(after.len());
            if let Some(found) = lookup(&after[..end]) {
                out.push_str(found);
                rest = &after[end..];
                continue;
            }
        }
        out.push('$');
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests;
