//! The Agent Client Protocol server (`maple-agent acp`): an editor, or a
//! bridge such as Buzz, drives Maple's agent over stdio.
//!
//! Each ACP session is one Maple task the connection holds with a lease
//! (`AgentSurfaceLease`): its tools get the bridge's variables, its runs
//! answer the caller alone, and the desktop's own tools stay out of it.
//! `session/new` creates an ACP task, which no list shows until its first
//! prompt; `session/load` holds an existing task and replays its history.
//! Maple has no session modes: every tool call runs without asking.

mod config;
mod convert;
mod handler;
mod session;
mod transport;

pub use config::{AgentAcpConfig, load_acp_config};
use config::{AgentAcpStats, normalize_config};
use convert::{
    AcpProjection, COMPACTING_NOTICE, COMPACTION_COMPLETED_NOTICE, MAX_ACP_ERROR_CHARS,
    NOTHING_TO_COMPACT_NOTICE, acp_available_commands, acp_config_options,
    acp_session_config_options, acp_usage, bounded_chars, event_error_text, internal_acp_error,
    outbound_error, parse_slash_command, project_trust_elicitation_request,
    project_trust_permission_decision, project_trust_permission_options, prompt_images,
    prompt_result_from_terminal, prompt_text, text_chunk, timeline_update,
};
use handler::{AcpCallerSessionFields, MapleAcpHandler};
use session::{
    ALLOWED_BRIDGE_ENV, AcpConnectionContext, AcpProjectTrustResolution, AcpPromptState,
    AcpSession, AcpSessionOperation, UnpublishedAcpSession, bridge_tool_context_spec,
    canonical_session_id, canonical_session_id_text, close_registration_may_be_released,
    ensure_allowed_project_root, filter_bridge_environment, find_acp_session, has_buzz_credentials,
    prepare_session_mcp,
};
use transport::{
    AcpOutboundReservation, AcpOutboundSendError, AcpOutboundTracker, BoundedLineReader,
    MAX_ACP_FRAME_BYTES, NEXT_ACP_MESSAGE_ID, tracked_outgoing_lines,
};

use crate::agent::{
    AGENT_SURFACE_INACTIVE_ERROR, AgentCreateSessionRequest, AgentImageUpload, AgentRunEvent,
    AgentRunTerminal, AgentRuntimeHandle, AgentSendMessageRequest, AgentTaskState,
    AgentTimelineItem, CatalogEntry, NOTHING_TO_COMPACT_ERROR,
};
use agent_client_protocol::schema::v1::{
    CancelNotification, CloseSessionRequest, CloseSessionResponse, ConfigOptionUpdate,
    ContentBlock, DeleteSessionRequest, DeleteSessionResponse, ElicitationAction,
    ElicitationContentValue, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    RequestPermissionRequest, SessionId, SessionInfo, SessionInfoUpdate, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, TextContent, ToolCall,
    ToolCallContent, ToolCallStatus, ToolKind, UsageUpdate,
};
use agent_client_protocol::{Agent as AcpAgent, Client, ConnectionTo, Lines};
use futures_util::StreamExt as _;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, RwLock};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

const ACP_CONNECTION_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const ACP_SYNTHETIC_STOP_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const ACP_SESSION_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The runtime start an ACP connection shares between its sessions, so the
/// handshake can answer before the runtime boots: the first session request
/// starts it once, and later requests wait for the same start.
pub type SharedRuntimeStart = futures_util::future::Shared<
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>,
>;

/// Build the shared lazy start `serve_stdio` expects from any start future.
pub fn shared_runtime_start<F>(start: F) -> SharedRuntimeStart
where
    F: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    futures_util::FutureExt::shared(Box::pin(start))
}

/// Serve ACP on this process's stdin and stdout for one signed-in account.
///
/// `maple-agent acp` calls this with the agent runtime's start. The editor
/// that spawned the process owns the connection: when its stdin closes,
/// every open session is cleaned up and the call returns. The bridge
/// environment (Buzz credentials and `PATH`) is read from this process's
/// own environment.
pub async fn serve_stdio(
    agent: AgentRuntimeHandle,
    config: AgentAcpConfig,
    runtime_start: SharedRuntimeStart,
) -> Result<(), String> {
    let config = Arc::new(RwLock::new(normalize_config(config)?));
    let stats = Arc::new(AgentAcpStats::default());
    let context = AcpConnectionContext::new(agent, config, stats, runtime_start);
    let environment = ALLOWED_BRIDGE_ENV
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| ((*key).to_string(), value))
        })
        .collect::<HashMap<_, _>>();
    context.set_bridge_environment(environment).await;
    serve(context, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serve one connection on `input` and `output` until the peer closes it,
/// then clean up every session it opened.
async fn serve<R, W>(context: Arc<AcpConnectionContext>, input: R, output: W) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let peer_eof = CancellationToken::new();
    let read = BoundedLineReader::new(input, peer_eof.clone());
    let incoming =
        FramedRead::new(read, LinesCodec::new_with_max_length(MAX_ACP_FRAME_BYTES)).map(|result| {
            result.map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        });
    let outgoing = tracked_outgoing_lines(output, Arc::clone(&context.outbound));
    let serving = AcpAgent
        .builder()
        .name("maple-acp")
        .with_handler(MapleAcpHandler {
            context: Arc::clone(&context),
        })
        .connect_to(Lines::new(outgoing, incoming));
    let result = tokio::select! {
        result = serving => result.map_err(|error| bounded_chars(&error.to_string(), MAX_ACP_ERROR_CHARS)),
        _ = peer_eof.cancelled() => Ok(()),
    };
    context.cleanup().await;
    result
}

/// A prompt the connection admitted, in order with the client's other
/// messages, and runs in the background.
pub(super) struct PromptAdmission {
    pub(super) session_id: String,
    prompt: String,
    images: Vec<AgentImageUpload>,
    cancellation: CancellationToken,
    operation: Arc<AcpSessionOperation>,
}

/// What ended a turn's stream of events early.
enum StreamStop {
    /// The run's events overflowed the stream.
    Overflowed,
    /// One update was too large to send.
    UpdateTooLarge,
}

impl AcpConnectionContext {
    fn new(
        agent: AgentRuntimeHandle,
        config: Arc<RwLock<AgentAcpConfig>>,
        stats: Arc<AgentAcpStats>,
        runtime_start: SharedRuntimeStart,
    ) -> Arc<Self> {
        Arc::new(Self {
            agent,
            runtime_start,
            config,
            stats,
            bridge_environment: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            session_operations: Mutex::new(HashMap::new()),
            closing_sessions: Mutex::new(HashSet::new()),
            prompt_states: Mutex::new(HashMap::new()),
            background_tasks: Mutex::new(tokio::task::JoinSet::new()),
            finalization: Mutex::new(()),
            lifetime: CancellationToken::new(),
            closed: AtomicBool::new(false),
            has_credentials: AtomicBool::new(false),
            client_supports_form_elicitation: AtomicBool::new(false),
            outbound: AcpOutboundTracker::new(),
        })
    }

    async fn set_bridge_environment(&self, environment: HashMap<String, String>) {
        let _finalization = self.finalization.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let environment = filter_bridge_environment(environment);
        let has_credentials = has_buzz_credentials(&environment);
        *self.bridge_environment.lock().await = environment;
        if has_credentials && !self.has_credentials.swap(true, Ordering::SeqCst) {
            self.stats
                .credential_connections
                .fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn new_session(
        &self,
        cx: &ConnectionTo<Client>,
        request: NewSessionRequest,
        caller: AcpCallerSessionFields,
    ) -> Result<NewSessionResponse, agent_client_protocol::Error> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(agent_client_protocol::Error::internal_error()
                .data("The Maple ACP connection is closing"));
        }
        if !request.cwd.is_absolute() {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("ACP session cwd must be an absolute path"));
        }
        self.await_runtime_start().await?;
        let config = self.config.read().await.clone();
        let project_root = ensure_allowed_project_root(&request.cwd, &config.allowed_project_roots)
            .map_err(|error| agent_client_protocol::Error::invalid_params().data(error))?;
        let project_trust = self
            .agent
            .get_project_trust(project_root.to_string_lossy().into_owned())
            .await
            .map_err(internal_acp_error)?;

        let available_models = self.available_models().await?;
        let model = available_models
            .first()
            .cloned()
            .ok_or_else(|| internal_acp_error("Maple returned no models".to_string()))?;
        let bridge_environment = self.bridge_environment.lock().await.clone();
        let (environment, session_servers) =
            prepare_session_mcp(&bridge_environment, &request.mcp_servers)?;
        let tool_context = bridge_tool_context_spec(&environment).map_err(internal_acp_error)?;
        let created = self
            .agent
            .create_surface_session(
                AgentCreateSessionRequest {
                    project_root: Some(project_root.to_string_lossy().into_owned()),
                    title: caller.session_title,
                    model: Some(model.clone()),
                    context_limit: None,
                    mcp_server_names: None,
                    system_prompt: caller.system_prompt,
                },
                tool_context,
                session_servers,
            )
            .await
            .map_err(internal_acp_error)?;
        // Own the lease before anything can fail, so an early return
        // discards the new task instead of leaking it.
        let unpublished = UnpublishedAcpSession::new(created.lease);
        let session_id = canonical_session_id_text(&created.detail.session.id)?;
        let finalization = self.finalization.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            drop(finalization);
            drop(unpublished);
            return Err(agent_client_protocol::Error::internal_error()
                .data("The Maple ACP connection closed while configuring the session"));
        }
        let mut sessions = self.sessions.lock().await;
        let mut operations = self.session_operations.lock().await;
        if sessions.contains_key(&session_id) || operations.contains_key(&session_id) {
            drop(operations);
            drop(sessions);
            drop(finalization);
            drop(unpublished);
            return Err(agent_client_protocol::Error::internal_error()
                .data("The Maple ACP connection duplicated a new session"));
        }
        let lease = unpublished.publish();
        sessions.insert(
            session_id.clone(),
            AcpSession {
                lease: Some(lease),
                model: model.clone(),
                available_models: available_models.clone(),
                catalog: None,
                advertised_title: Some(created.detail.session.title.clone()),
                message_count: created.detail.session.message_count,
                created_here: true,
                prompted: false,
                project_root: project_root.clone(),
                project_trust_decision: project_trust.decision,
            },
        );
        operations.insert(session_id.clone(), AcpSessionOperation::new(&self.lifetime));
        drop(operations);
        drop(sessions);
        self.stats.active_sessions.fetch_add(1, Ordering::SeqCst);
        if has_buzz_credentials(&environment) && !self.has_credentials.swap(true, Ordering::SeqCst)
        {
            self.stats
                .credential_connections
                .fetch_add(1, Ordering::SeqCst);
        }
        drop(finalization);
        self.send_available_commands(cx, &session_id, &project_root)
            .await;
        Ok(NewSessionResponse::new(session_id)
            .config_options(acp_config_options(&model, &available_models)))
    }

    async fn retire_session(&self, session_id: &str) {
        let session = self.sessions.lock().await.remove(session_id);
        if let Some(operation) = self.session_operations.lock().await.remove(session_id) {
            operation.cancellation.cancel();
        }
        if let Some(mut session) = session {
            self.stats.active_sessions.fetch_sub(1, Ordering::SeqCst);
            if let Some(lease) = session.lease.take() {
                lease.release().await;
            }
        }
    }

    /// Block until the lazy runtime start finished. The handshake answers
    /// before the runtime boots; the first session request pays the boot.
    async fn await_runtime_start(&self) -> Result<(), agent_client_protocol::Error> {
        let start = self.runtime_start.clone();
        tokio::select! {
            biased;
            _ = self.lifetime.cancelled() => Err(
                agent_client_protocol::Error::internal_error()
                    .data("The Maple ACP connection closed while the runtime started"),
            ),
            result = start => result
                .map_err(|error| internal_acp_error(format!("Failed to start the Agent runtime: {error}"))),
        }
    }

    /// What the catalog says about the session's model, cached per session.
    /// A catalog that cannot be read gives an empty entry, and the next turn
    /// asks again.
    async fn session_catalog_entry(&self, session_id: &str, model: &str) -> CatalogEntry {
        if let Some(cached) = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .filter(|session| session.model == model)
            .and_then(|session| session.catalog.clone())
        {
            return cached;
        }
        let Some(entry) = self.agent.model_catalog_entry(model).await.ok().flatten() else {
            return CatalogEntry::default();
        };
        if let Some(session) = self.sessions.lock().await.get_mut(session_id)
            && session.model == model
        {
            session.catalog = Some(entry.clone());
        }
        entry
    }

    /// Soft-delete a task: archive it so it disappears from session lists
    /// but stays recoverable in Maple Desktop. An owned live session is
    /// retired first; a running turn refuses the delete.
    async fn delete_session(
        &self,
        request: DeleteSessionRequest,
    ) -> Result<DeleteSessionResponse, agent_client_protocol::Error> {
        let session_id = canonical_session_id(&request.session_id)?;
        if self.sessions.lock().await.contains_key(&session_id) {
            self.retire_session(&session_id).await;
        }
        self.agent
            .set_session_state(session_id.clone(), AgentTaskState::Archived)
            .await
            .map_err(|error| {
                agent_client_protocol::Error::invalid_request()
                    .data(bounded_chars(&error, MAX_ACP_ERROR_CHARS))
            })?;
        Ok(DeleteSessionResponse::new())
    }

    async fn available_models(&self) -> Result<Vec<String>, agent_client_protocol::Error> {
        tokio::select! {
            biased;
            _ = self.lifetime.cancelled() => Err(
                agent_client_protocol::Error::internal_error()
                    .data("The Maple ACP connection closed while loading models")
            ),
            result = self.agent.available_model_ids() => result.map_err(internal_acp_error),
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => Err(
                agent_client_protocol::Error::internal_error()
                    .data("Maple model discovery timed out")
            ),
        }
    }

    async fn remove_session_operation_if_same(
        &self,
        session_id: &str,
        operation: &Arc<AcpSessionOperation>,
    ) {
        let mut operations = self.session_operations.lock().await;
        if operations
            .get(session_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, operation))
        {
            operations.remove(session_id);
        }
    }

    async fn load_session(
        &self,
        cx: &ConnectionTo<Client>,
        request: LoadSessionRequest,
    ) -> Result<LoadSessionResponse, agent_client_protocol::Error> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(agent_client_protocol::Error::internal_error()
                .data("The Maple ACP connection is closing"));
        }
        if !request.cwd.is_absolute() {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("ACP session cwd must be an absolute path"));
        }
        self.await_runtime_start().await?;
        let config = self.config.read().await.clone();
        let project_root = ensure_allowed_project_root(&request.cwd, &config.allowed_project_roots)
            .map_err(|error| agent_client_protocol::Error::invalid_params().data(error))?;
        let project_root_text = project_root.to_string_lossy().into_owned();
        let project_trust = self
            .agent
            .get_project_trust(project_root_text.clone())
            .await
            .map_err(internal_acp_error)?;
        let session_id = canonical_session_id(&request.session_id)?;
        let persisted_sessions = self
            .agent
            .list_sessions(Some(project_root_text))
            .await
            .map_err(internal_acp_error)?;
        let persisted = find_acp_session(&persisted_sessions, &session_id)
            .map_err(|error| agent_client_protocol::Error::invalid_request().data(error))?;
        let available_models = self.available_models().await?;
        if let Some(model) = persisted.model.as_ref()
            && !available_models.iter().any(|available| available == model)
        {
            return Err(agent_client_protocol::Error::invalid_request().data(format!(
                "This Maple Agent task uses model '{model}', which is no longer available; the task remains available in Maple Desktop"
            )));
        }
        let bridge_environment = self.bridge_environment.lock().await.clone();
        let (environment, session_servers) =
            prepare_session_mcp(&bridge_environment, &request.mcp_servers)?;
        let tool_context = bridge_tool_context_spec(&environment).map_err(internal_acp_error)?;
        let protocol_session_id = SessionId::new(session_id.clone());
        let operation = AcpSessionOperation::new(&self.lifetime);
        // Held until the history is replayed, so a prompt's updates follow it.
        let operation_guard = Arc::clone(&operation.gate).lock_owned().await;
        {
            // Register the operation before the task is held. Close can now
            // mark it closing and wait, while disconnect linearizes through
            // the same finalization barrier used by session creation.
            let _finalization = self.finalization.lock().await;
            if self.closed.load(Ordering::SeqCst) {
                return Err(agent_client_protocol::Error::internal_error()
                    .data("The Maple ACP connection is closing"));
            }
            if self.closing_sessions.lock().await.contains(&session_id) {
                return Err(agent_client_protocol::Error::invalid_request()
                    .data("This ACP session is closing"));
            }
            if self.sessions.lock().await.contains_key(&session_id) {
                return Err(agent_client_protocol::Error::invalid_request()
                    .data("This ACP connection already owns the requested session"));
            }
            let mut operations = self.session_operations.lock().await;
            if operations.contains_key(&session_id) {
                return Err(agent_client_protocol::Error::invalid_request()
                    .data("This ACP connection is already loading the requested session"));
            }
            operations.insert(session_id.clone(), Arc::clone(&operation));
        }
        let attached = match self
            .agent
            .attach_surface_session(&session_id, tool_context, session_servers)
            .await
        {
            Ok(attached) => attached,
            Err(error) => {
                self.remove_session_operation_if_same(&session_id, &operation)
                    .await;
                return Err(internal_acp_error(error));
            }
        };
        let lease = attached.lease;
        let Some(model) = attached
            .detail
            .session
            .model
            .clone()
            .or_else(|| available_models.first().cloned())
        else {
            lease.release().await;
            self.remove_session_operation_if_same(&session_id, &operation)
                .await;
            return Err(internal_acp_error("Maple returned no models".to_string()));
        };
        let timeline = attached.detail.timeline;
        let message_count = attached.detail.session.message_count;
        let finalization = self.finalization.lock().await;
        if self.closed.load(Ordering::SeqCst)
            || self.closing_sessions.lock().await.contains(&session_id)
        {
            drop(finalization);
            lease.release().await;
            self.remove_session_operation_if_same(&session_id, &operation)
                .await;
            return Err(agent_client_protocol::Error::internal_error()
                .data("The Maple ACP session closed while it was loading"));
        }
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&session_id) {
            drop(sessions);
            drop(finalization);
            lease.release().await;
            self.remove_session_operation_if_same(&session_id, &operation)
                .await;
            return Err(agent_client_protocol::Error::invalid_request()
                .data("This ACP connection duplicated the requested session"));
        }
        sessions.insert(
            session_id.clone(),
            AcpSession {
                lease: Some(lease),
                model: model.clone(),
                available_models: available_models.clone(),
                catalog: None,
                advertised_title: Some(persisted.title.clone()),
                message_count,
                created_here: false,
                prompted: false,
                project_root: project_root.clone(),
                project_trust_decision: project_trust.decision,
            },
        );
        drop(sessions);
        self.stats.active_sessions.fetch_add(1, Ordering::SeqCst);
        drop(finalization);

        let mut projection = AcpProjection::default();
        for item in &timeline {
            if let Some(update) = timeline_update(item, &mut projection, true)
                && let Err(error) = self
                    .send_session_update(
                        cx,
                        SessionNotification::new(protocol_session_id.clone(), update),
                        &operation.cancellation,
                    )
                    .await
            {
                drop(operation_guard);
                self.retire_session(&session_id).await;
                return Err(outbound_error(error));
            }
        }
        drop(operation_guard);
        self.send_available_commands(cx, &session_id, &project_root)
            .await;
        Ok(
            LoadSessionResponse::new().config_options(acp_session_config_options(
                &model,
                &available_models,
                message_count,
            )),
        )
    }

    async fn list_sessions(
        &self,
        request: ListSessionsRequest,
    ) -> Result<ListSessionsResponse, agent_client_protocol::Error> {
        let config = self.config.read().await.clone();
        let project_root = match request.cwd.as_deref() {
            Some(cwd) => {
                if !cwd.is_absolute() {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("ACP session-list cwd must be an absolute path"));
                }
                Some(
                    ensure_allowed_project_root(cwd, &config.allowed_project_roots).map_err(
                        |error| agent_client_protocol::Error::invalid_params().data(error),
                    )?,
                )
            }
            None => None,
        };
        let sessions = self
            .agent
            .list_sessions(
                project_root
                    .as_ref()
                    .map(|root| root.to_string_lossy().into_owned()),
            )
            .await
            .map_err(internal_acp_error)?;
        let visible = sessions
            .into_iter()
            .filter(|session| {
                ensure_allowed_project_root(
                    Path::new(&session.project_root),
                    &config.allowed_project_roots,
                )
                .is_ok()
            })
            .collect::<Vec<_>>();
        let start = request
            .cursor
            .as_deref()
            .map(str::parse::<usize>)
            .transpose()
            .map_err(|_| {
                agent_client_protocol::Error::invalid_params()
                    .data("Invalid Maple ACP session-list cursor")
            })?
            .unwrap_or(0);
        if start > visible.len() {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("Maple ACP session-list cursor is out of range"));
        }
        let end = start.saturating_add(100).min(visible.len());
        let listed = visible[start..end]
            .iter()
            .map(|session| {
                let mut info =
                    SessionInfo::new(session.id.clone(), PathBuf::from(&session.project_root))
                        .title(session.title.clone());
                if let Some(updated_at) =
                    chrono::DateTime::from_timestamp_millis(session.updated_ms)
                {
                    info = info.updated_at(updated_at.to_rfc3339());
                }
                info
            })
            .collect();
        let mut response = ListSessionsResponse::new(listed);
        if end < visible.len() {
            response = response.next_cursor(end.to_string());
        }
        Ok(response)
    }

    async fn close_session(
        &self,
        request: CloseSessionRequest,
    ) -> Result<CloseSessionResponse, agent_client_protocol::Error> {
        let session_id = canonical_session_id(&request.session_id)?;
        let operation = self
            .session_operations
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                agent_client_protocol::Error::resource_not_found(Some(session_id.clone()))
                    .data("ACP session is not owned by this connection")
            })?;
        self.closing_sessions
            .lock()
            .await
            .insert(session_id.clone());
        // Stops the session's prompt, from its admission on, and its run.
        operation.cancellation.cancel();
        // One absolute deadline for every potentially blocking close phase.
        // Paseo awaits this response before terminating the ACP child, so a
        // fresh timeout per phase could still hang it for multiples of the
        // advertised close bound.
        let close_deadline = tokio::time::Instant::now() + ACP_SESSION_CLOSE_TIMEOUT;
        // A prompt holds this gate until its run ended. A broken provider
        // must not make close hang forever, though: after the bound, revoke
        // the lease at once and let its release finish in the background
        // while Paseo can terminate the child process.
        let operation_guard =
            tokio::time::timeout_at(close_deadline, Arc::clone(&operation.gate).lock_owned())
                .await
                .ok();
        let operation_drained = operation_guard.is_some();
        let Some(mut session) = self.sessions.lock().await.remove(&session_id) else {
            if close_registration_may_be_released(true, operation_drained, true) {
                self.remove_session_operation_if_same(&session_id, &operation)
                    .await;
                self.closing_sessions.lock().await.remove(&session_id);
            }
            return Ok(CloseSessionResponse::new());
        };
        self.stats.active_sessions.fetch_sub(1, Ordering::SeqCst);
        let discard = operation_drained && session.created_here && !session.prompted;
        let cleanup_completed = if let Some(lease) = session.lease.take() {
            if operation_drained {
                tokio::time::timeout_at(close_deadline, async move {
                    if discard {
                        lease.discard_created_if_untouched().await;
                    } else {
                        lease.release().await;
                    }
                })
                .await
                .is_ok()
            } else {
                lease.revoke();
                drop(lease);
                false
            }
        } else {
            operation_drained
        };
        if close_registration_may_be_released(true, operation_drained, cleanup_completed) {
            self.remove_session_operation_if_same(&session_id, &operation)
                .await;
            self.closing_sessions.lock().await.remove(&session_id);
        }
        Ok(CloseSessionResponse::new())
    }

    async fn set_config_option(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, agent_client_protocol::Error> {
        let session_id = canonical_session_id(&request.session_id)?;
        let operation = self
            .session_operations
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                agent_client_protocol::Error::resource_not_found(Some(session_id.clone()))
                    .data("ACP session is not owned by this connection")
            })?;
        let _operation_guard = Arc::clone(&operation.gate).lock_owned().await;
        if self.closed.load(Ordering::SeqCst)
            || self.closing_sessions.lock().await.contains(&session_id)
        {
            return Err(
                agent_client_protocol::Error::resource_not_found(Some(session_id.clone()))
                    .data("ACP session is closing"),
            );
        }
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(&session_id).ok_or_else(|| {
            agent_client_protocol::Error::resource_not_found(Some(session_id.clone()))
                .data("ACP session is not owned by this connection")
        })?;
        let selected_value = request.value.as_value_id().ok_or_else(|| {
            agent_client_protocol::Error::invalid_params()
                .data("Maple ACP configuration options require a select value")
        })?;
        match request.config_id.0.as_ref() {
            "model" => {
                let model = selected_value.0.as_ref();
                if !session
                    .available_models
                    .iter()
                    .any(|candidate| candidate == model)
                {
                    return Err(
                        agent_client_protocol::Error::invalid_params().data("Unknown Maple model")
                    );
                }
                if session.message_count > 0 && session.model != model {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("Maple tasks are model-locked after their first message"));
                }
                session.model = model.to_string();
                session.catalog = None;
            }
            _ => {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("Unknown Maple ACP configuration option"));
            }
        }
        Ok(SetSessionConfigOptionResponse::new(
            session.config_options(),
        ))
    }

    /// Maple advertises no session modes, so no mode id can be selected.
    /// The route stays so a client that still sends one gets a clear
    /// `invalid_params` answer instead of a method-not-found error.
    async fn set_mode(
        &self,
        request: SetSessionModeRequest,
    ) -> Result<SetSessionModeResponse, agent_client_protocol::Error> {
        let session_id = canonical_session_id(&request.session_id)?;
        let operation = self
            .session_operations
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                agent_client_protocol::Error::resource_not_found(Some(session_id.clone()))
                    .data("ACP session is not owned by this connection")
            })?;
        let _operation_guard = Arc::clone(&operation.gate).lock_owned().await;
        if self.closed.load(Ordering::SeqCst)
            || self.closing_sessions.lock().await.contains(&session_id)
            || !self.sessions.lock().await.contains_key(&session_id)
        {
            return Err(
                agent_client_protocol::Error::resource_not_found(Some(session_id))
                    .data("ACP session is not available on this connection"),
            );
        }
        Err(agent_client_protocol::Error::invalid_params().data(format!(
            "Maple ACP has no session modes ('{}' cannot be selected): every tool call runs without asking",
            request.mode_id.0
        )))
    }

    /// Admit a prompt: one at a time per session. Quick, so the dispatcher
    /// can admit it in order with the client's next messages.
    async fn begin_prompt(
        &self,
        request: &PromptRequest,
    ) -> Result<PromptAdmission, agent_client_protocol::Error> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(agent_client_protocol::Error::internal_error()
                .data("The Maple ACP connection is closing"));
        }
        let session_id = canonical_session_id(&request.session_id)?;
        let prompt = prompt_text(&request.prompt)?;
        let images = prompt_images(&request.prompt);
        let operation = self
            .session_operations
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                agent_client_protocol::Error::resource_not_found(Some(session_id.clone()))
                    .data("ACP session is not owned by this connection")
            })?;
        if self.closing_sessions.lock().await.contains(&session_id)
            || !self.sessions.lock().await.contains_key(&session_id)
        {
            return Err(
                agent_client_protocol::Error::resource_not_found(Some(session_id.clone()))
                    .data("ACP session is not available on this connection"),
            );
        }
        let mut states = self.prompt_states.lock().await;
        if states.contains_key(&session_id) {
            return Err(agent_client_protocol::Error::invalid_request()
                .data("This ACP session already has an active prompt"));
        }
        let cancellation = operation.cancellation.child_token();
        states.insert(
            session_id.clone(),
            AcpPromptState {
                cancellation: cancellation.clone(),
            },
        );
        Ok(PromptAdmission {
            session_id,
            prompt,
            images,
            cancellation,
            operation,
        })
    }

    async fn send_session_update(
        &self,
        cx: &ConnectionTo<Client>,
        notification: SessionNotification,
        cancellation: &CancellationToken,
    ) -> Result<(), AcpOutboundSendError> {
        let encoded_bytes = serde_json::to_vec(&notification)
            .map_err(|error| {
                AcpOutboundSendError::Transport(
                    agent_client_protocol::Error::internal_error()
                        .data(format!("Failed to encode Maple ACP update: {error}")),
                )
            })?
            .len();
        let reservation = self.outbound.reserve(encoded_bytes, cancellation).await?;
        self.outbound.enqueue(cx, notification, reservation)
    }

    /// Run the `/compact` built-in: summarize the history, tell the caller,
    /// and end the turn. No model turn runs.
    async fn run_compact_command(
        &self,
        cx: &ConnectionTo<Client>,
        protocol_session_id: &SessionId,
        session_id: &str,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        let notice = match self.agent.compact_session(session_id.to_string()).await {
            Ok(()) => COMPACTION_COMPLETED_NOTICE,
            Err(error) if error == NOTHING_TO_COMPACT_ERROR => NOTHING_TO_COMPACT_NOTICE,
            Err(error) => return Err(internal_acp_error(error)),
        };
        match self
            .send_final_agent_message(cx, protocol_session_id.clone(), notice, &self.lifetime)
            .await
        {
            // The compaction itself succeeded; a failed notice must not turn
            // the command into an error for the caller.
            Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {
                Ok(PromptResponse::new(StopReason::EndTurn))
            }
            Err(AcpOutboundSendError::Cancelled) => Ok(PromptResponse::new(StopReason::Cancelled)),
            Err(AcpOutboundSendError::Transport(error)) => Err(error),
        }
    }

    /// Tell the caller which slash commands this session offers: the
    /// agent-side built-ins plus the skills installed for its root.
    /// Best effort: a failed advertisement never fails the session.
    async fn send_available_commands(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: &str,
        root: &Path,
    ) {
        let skills = self.agent.slash_commands(Some(&root.to_string_lossy()));
        let notification = SessionNotification::new(
            SessionId::new(session_id.to_string()),
            acp_available_commands(&skills),
        );
        if let Err(error) = self
            .send_session_update(cx, notification, &self.lifetime)
            .await
        {
            log::warn!("Failed to send Maple ACP available commands: {error:?}");
        }
    }

    async fn send_final_agent_message(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: SessionId,
        message: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), AcpOutboundSendError> {
        let message_id = format!(
            "maple-acp-notice-{}",
            NEXT_ACP_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
        );
        self.send_session_update(
            cx,
            SessionNotification::new(
                session_id,
                SessionUpdate::AgentMessageChunk(text_chunk(message.to_string(), &message_id)),
            ),
            cancellation,
        )
        .await
    }

    async fn request_project_trust_with_elicitation(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: SessionId,
        project_root: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Option<bool>, AcpOutboundSendError> {
        let request = project_trust_elicitation_request(session_id, project_root);
        let encoded_bytes = serde_json::to_vec(&request)
            .map_err(|error| {
                AcpOutboundSendError::Transport(internal_acp_error(format!(
                    "Failed to encode Maple ACP project trust elicitation: {error}"
                )))
            })?
            .len();
        let reservation = self.outbound.reserve(encoded_bytes, cancellation).await?;
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let sent_request = cx.send_request(request);
        let mut response_future = Box::pin(sent_request.block_task());
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            response = &mut response_future => Some(response),
        };
        let Some(response) = response else {
            retain_cancelled_caller_request(response_future, reservation);
            return Ok(None);
        };
        drop(reservation);
        let response = response.map_err(AcpOutboundSendError::Transport)?;
        match response.action {
            ElicitationAction::Accept(action) => {
                let trusted = action
                    .content
                    .as_ref()
                    .and_then(|content| content.get("trustProject"))
                    .and_then(|value| match value {
                        ElicitationContentValue::Boolean(value) => Some(*value),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        AcpOutboundSendError::Transport(internal_acp_error(
                            "ACP client returned an invalid project trust elicitation response"
                                .to_string(),
                        ))
                    })?;
                Ok(Some(trusted))
            }
            ElicitationAction::Decline => Ok(Some(false)),
            _ => Ok(None),
        }
    }

    async fn request_project_trust_with_chooser(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: SessionId,
        project_root: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Option<bool>, AcpOutboundSendError> {
        let request_id = format!(
            "maple-project-trust-{}",
            NEXT_ACP_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
        );
        let tool_call = ToolCall::new(request_id, "Trust this project?")
            .kind(ToolKind::Other)
            .status(ToolCallStatus::Pending)
            .content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new(format!(
                    "Trusting '{}' allows Maple to use project-provided guidance, including agent skills. These instructions can influence how agents work and use tools, and Maple runs every tool call without asking.",
                    project_root.display()
                )),
            ))]);
        let request = RequestPermissionRequest::new(
            session_id,
            tool_call.into(),
            project_trust_permission_options(),
        );
        let encoded_bytes = serde_json::to_vec(&request)
            .map_err(|error| {
                AcpOutboundSendError::Transport(internal_acp_error(format!(
                    "Failed to encode Maple ACP project trust request: {error}"
                )))
            })?
            .len();
        let reservation = self.outbound.reserve(encoded_bytes, cancellation).await?;
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let sent_request = cx.send_request(request);
        let mut response_future = Box::pin(sent_request.block_task());
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            response = &mut response_future => Some(response),
        };
        let Some(response) = response else {
            retain_cancelled_caller_request(response_future, reservation);
            return Ok(None);
        };
        drop(reservation);
        let response = response.map_err(AcpOutboundSendError::Transport)?;
        project_trust_permission_decision(&response.outcome)
            .map_err(|error| AcpOutboundSendError::Transport(internal_acp_error(error)))
    }

    /// Ask the caller's user once whether to trust the session's project,
    /// when it has guidance a decision would change, and follow a decision
    /// made elsewhere since the session started.
    async fn resolve_project_trust_before_prompt(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<AcpProjectTrustResolution, agent_client_protocol::Error> {
        let (project_root, configured_decision, access) = {
            let sessions = self.sessions.lock().await;
            let session = sessions.get(session_id).ok_or_else(|| {
                agent_client_protocol::Error::resource_not_found(Some(session_id.to_string()))
                    .data("ACP session is no longer owned by this connection")
            })?;
            (
                session.project_root.clone(),
                session.project_trust_decision,
                session
                    .lease
                    .as_ref()
                    .expect("a runnable ACP session must own a lease")
                    .access(),
            )
        };
        let mut status = self
            .agent
            .get_project_trust(project_root.to_string_lossy().into_owned())
            .await
            .map_err(internal_acp_error)?;
        if status.decision.is_none() && !status.protected_features.is_empty() {
            let protocol_session_id = SessionId::new(session_id.to_string());
            let decision = if self.client_supports_form_elicitation.load(Ordering::SeqCst) {
                match self
                    .request_project_trust_with_elicitation(
                        cx,
                        protocol_session_id.clone(),
                        &project_root,
                        cancellation,
                    )
                    .await
                {
                    Ok(decision) => decision,
                    Err(error) if !cancellation.is_cancelled() => {
                        log::warn!(
                            "ACP client advertised form elicitation but project trust elicitation failed; using the permission chooser fallback: {error:?}"
                        );
                        self.request_project_trust_with_chooser(
                            cx,
                            protocol_session_id,
                            &project_root,
                            cancellation,
                        )
                        .await
                        .map_err(outbound_error)?
                    }
                    Err(error) => return Err(outbound_error(error)),
                }
            } else {
                self.request_project_trust_with_chooser(
                    cx,
                    protocol_session_id,
                    &project_root,
                    cancellation,
                )
                .await
                .map_err(outbound_error)?
            };
            let Some(trusted) = decision else {
                return Ok(AcpProjectTrustResolution::Cancelled);
            };
            status = self
                .agent
                .set_project_trust_for_surface(
                    &access,
                    project_root.to_string_lossy().into_owned(),
                    trusted,
                )
                .await
                .map_err(internal_acp_error)?;
        } else if status.decision != configured_decision {
            let trusted = status.decision.unwrap_or(false);
            status = self
                .agent
                .set_project_trust_for_surface(
                    &access,
                    project_root.to_string_lossy().into_owned(),
                    trusted,
                )
                .await
                .map_err(internal_acp_error)?;
        }
        if let Some(session) = self.sessions.lock().await.get_mut(session_id) {
            session.project_trust_decision = status.decision;
        }
        Ok(AcpProjectTrustResolution::Continue)
    }

    /// Run an admitted prompt: after the session's other operations, as one
    /// turn of its task, streaming the turn's updates to the caller.
    async fn prompt(
        self: &Arc<Self>,
        cx: &ConnectionTo<Client>,
        admission: PromptAdmission,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        let PromptAdmission {
            session_id,
            prompt,
            images,
            cancellation,
            operation,
        } = admission;
        let operation_guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            guard = Arc::clone(&operation.gate).lock_owned() => Some(guard),
        };
        let result = match operation_guard {
            Some(_) => {
                self.run_prompt(cx, &session_id, prompt, images, &cancellation)
                    .await
            }
            None => Ok(PromptResponse::new(StopReason::Cancelled)),
        };
        self.prompt_states.lock().await.remove(&session_id);
        drop(operation_guard);
        result
    }

    async fn run_prompt(
        self: &Arc<Self>,
        cx: &ConnectionTo<Client>,
        session_id: &str,
        prompt: String,
        images: Vec<AgentImageUpload>,
        cancellation: &CancellationToken,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        let protocol_session_id = SessionId::new(session_id.to_string());
        if self.closed.load(Ordering::SeqCst)
            || self.closing_sessions.lock().await.contains(session_id)
        {
            return Ok(PromptResponse::new(StopReason::Cancelled));
        }
        match self
            .resolve_project_trust_before_prompt(cx, session_id, cancellation)
            .await?
        {
            AcpProjectTrustResolution::Continue => {}
            AcpProjectTrustResolution::Cancelled => {
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
        }
        let (access, model, project_root) = {
            let sessions = self.sessions.lock().await;
            let session = sessions.get(session_id).ok_or_else(|| {
                agent_client_protocol::Error::resource_not_found(Some(session_id.to_string()))
                    .data("ACP session is no longer owned by this connection")
            })?;
            (
                session
                    .lease
                    .as_ref()
                    .expect("a runnable ACP session must own a lease")
                    .access(),
                session.model.clone(),
                session.project_root.clone(),
            )
        };
        // The catalog decides the turn's model, with or without images in
        // the prompt: a vision model sees images, embedded and from tools,
        // and the run compacts at the model's own window. A model the
        // catalog does not describe falls back to read_image, which still
        // sees the image, and to the default window, so no prompt is ever
        // rejected or silently dropped.
        let catalog = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            entry = self.session_catalog_entry(session_id, &model) => entry,
        };
        // Generated titles land between turns, after the previous turn's
        // events ended; catch up before the next one starts.
        if let Ok(Some(title)) = self.agent.session_display_title(session_id).await
            && let Some(update) = self.title_update(session_id, title).await
        {
            match self
                .send_session_update(
                    cx,
                    SessionNotification::new(protocol_session_id.clone(), update),
                    cancellation,
                )
                .await
            {
                Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {}
                Err(AcpOutboundSendError::Cancelled) => {
                    return Ok(PromptResponse::new(StopReason::Cancelled));
                }
                Err(AcpOutboundSendError::Transport(error)) => return Err(error),
            }
        }
        let mut prompt = prompt;
        if let Some((name, args)) = parse_slash_command(&prompt) {
            if name == "compact" {
                return self
                    .run_compact_command(cx, &protocol_session_id, session_id)
                    .await;
            }
            // A matching skill expands into its activation prompt; anything
            // else falls through to the model as plain text, like desktop.
            if let Ok(Some(expanded)) =
                self.agent
                    .expand_slash_command(Some(&project_root.to_string_lossy()), &name, &args)
            {
                prompt = expanded;
            }
        }
        let run = match self
            .agent
            .send_surface_message(
                &access,
                AgentSendMessageRequest {
                    session_id: session_id.to_string(),
                    text: prompt,
                    model: Some(model.clone()),
                    context_limit: catalog
                        .context_window
                        .and_then(|window| usize::try_from(window).ok()),
                    vision_capable: catalog.vision == Some(true),
                    steer: false,
                    queue_id: None,
                    attachments: images,
                },
                cancellation.clone(),
            )
            .await
        {
            Ok(run) => run,
            Err(_) if cancellation.is_cancelled() => {
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            Err(error) if error == AGENT_SURFACE_INACTIVE_ERROR => {
                self.retire_session(session_id).await;
                return Err(agent_client_protocol::Error::resource_not_found(Some(
                    session_id.to_string(),
                ))
                .data("The Maple Agent task was removed outside this ACP connection"));
            }
            Err(error) => return Err(internal_acp_error(error)),
        };
        let locked_config_options = {
            let mut sessions = self.sessions.lock().await;
            sessions.get_mut(session_id).and_then(|session| {
                session.prompted = true;
                let first_message = session.message_count == 0;
                session.message_count = session.message_count.saturating_add(1);
                first_message.then(|| session.config_options())
            })
        };
        self.stats.active_runs.fetch_add(1, Ordering::SeqCst);
        let mut terminal = run.terminal.clone();
        let usage = run.usage.clone();
        let result = self
            .stream_run(
                cx,
                &protocol_session_id,
                run,
                locked_config_options,
                cancellation,
            )
            .await;
        let (result, stopped_early) = match result {
            Ok((response, stopped)) => (Ok(response), stopped),
            Err(error) => (Err(error), true),
        };
        if stopped_early {
            // A turn that stopped early stops its run, and settles once the
            // run has, within a bound; the next prompt waits for the rest.
            cancellation.cancel();
            let _ = tokio::time::timeout(
                ACP_SYNTHETIC_STOP_DRAIN_TIMEOUT,
                wait_for_terminal(&mut terminal),
            )
            .await;
        }
        let turn_usage = usage.borrow().as_ref().copied().unwrap_or_default();
        // ACP defines PromptResponse.usage as usage for this prompt turn.
        // Paseo stores it as currentTurnUsage, so cumulative session totals
        // would be double-counted on every later turn.
        let result = result.map(|response| response.usage(acp_usage(turn_usage)));
        // ACP's native context indicator: one usage_update per turn with
        // the tokens now in the task's context and the model's window. It
        // needs both, so a window the catalog does not give sends none
        // rather than a made-up size.
        if let Some(size) = catalog.context_window {
            let used = self
                .agent
                .session_context_tokens(session_id)
                .await
                .ok()
                .flatten()
                .unwrap_or(turn_usage.total_tokens);
            let notification = SessionNotification::new(
                protocol_session_id.clone(),
                SessionUpdate::UsageUpdate(UsageUpdate::new(used, size)),
            );
            // Sent after a cancelled turn too: the context changed either way.
            if let Err(error) = self
                .send_session_update(cx, notification, &self.lifetime)
                .await
            {
                log::warn!("Failed to send the Maple ACP usage update: {error:?}");
            }
        }
        self.stats.active_runs.fetch_sub(1, Ordering::SeqCst);
        result
    }

    /// Stream a run's events to the caller until it ends. Returns the turn's
    /// response, and whether the stream stopped before the run did.
    async fn stream_run(
        &self,
        cx: &ConnectionTo<Client>,
        protocol_session_id: &SessionId,
        run: crate::agent::AgentRunHandle,
        locked_config_options: Option<Vec<agent_client_protocol::schema::v1::SessionConfigOption>>,
        cancellation: &CancellationToken,
    ) -> Result<(PromptResponse, bool), agent_client_protocol::Error> {
        let session_id = protocol_session_id.0.to_string();
        let mut events = run.events;
        let mut terminal = run.terminal;
        let overflowed = run.event_overflowed;
        if let Some(config_options) = locked_config_options {
            let update = SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(config_options));
            match self
                .send_session_update(
                    cx,
                    SessionNotification::new(protocol_session_id.clone(), update),
                    cancellation,
                )
                .await
            {
                Ok(()) => {}
                Err(AcpOutboundSendError::Cancelled) => {
                    return Ok((PromptResponse::new(StopReason::Cancelled), true));
                }
                Err(AcpOutboundSendError::UpdateTooLarge) => {
                    return Err(agent_client_protocol::Error::internal_error()
                        .data("Maple's locked model selector exceeded the ACP update limit"));
                }
                Err(AcpOutboundSendError::Transport(error)) => return Err(error),
            }
        }
        let mut projection = AcpProjection::default();
        // A failure is told once the run ends failed: Pi may still retry it.
        let mut failure: Option<AgentTimelineItem> = None;
        let stop = loop {
            if overflowed.load(Ordering::Acquire) {
                break StreamStop::Overflowed;
            }
            let event = events.recv().await;
            if overflowed.load(Ordering::Acquire) {
                break StreamStop::Overflowed;
            }
            let update = match event {
                Some(AgentRunEvent::TimelineItem(item)) if item.item_type == "error" => {
                    failure = Some(item);
                    None
                }
                Some(AgentRunEvent::TimelineItem(item)) => {
                    timeline_update(&item, &mut projection, false)
                }
                Some(AgentRunEvent::Error(item)) => {
                    failure = Some(item);
                    None
                }
                Some(AgentRunEvent::Finished(ended)) => {
                    return self
                        .finish_stream(cx, protocol_session_id, ended, failure, cancellation)
                        .await;
                }
                Some(AgentRunEvent::Compacting) => Some(SessionUpdate::AgentMessageChunk(
                    text_chunk(COMPACTING_NOTICE.to_string(), &notice_id()),
                )),
                Some(AgentRunEvent::Compacted) => Some(SessionUpdate::AgentMessageChunk(
                    text_chunk(COMPACTION_COMPLETED_NOTICE.to_string(), &notice_id()),
                )),
                // A generated title landing mid-run renames the task; keep
                // the caller's session list in step.
                Some(AgentRunEvent::SessionUpdated(summary)) => {
                    self.title_update(&session_id, summary.title).await
                }
                // The caller keeps its own transcript and cannot reload ours;
                // the compaction notices above say what changed. Queues and
                // external agents are the desktop's.
                Some(
                    AgentRunEvent::HistoryReplaced
                    | AgentRunEvent::Started
                    | AgentRunEvent::SetupWarning(_)
                    | AgentRunEvent::SubagentStarted { .. }
                    | AgentRunEvent::SubagentActivity { .. }
                    | AgentRunEvent::SubagentFinished { .. }
                    | AgentRunEvent::QueueChanged(_)
                    | AgentRunEvent::QueuePromoted { .. },
                ) => None,
                None => {
                    wait_for_terminal(&mut terminal).await;
                    let ended = *terminal.borrow();
                    return match ended {
                        Some(ended) => {
                            self.finish_stream(
                                cx,
                                protocol_session_id,
                                ended,
                                failure,
                                cancellation,
                            )
                            .await
                        }
                        None => Err(agent_client_protocol::Error::internal_error()
                            .data("Maple Agent run ended without a terminal result")),
                    };
                }
            };
            let Some(update) = update else {
                continue;
            };
            match self
                .send_session_update(
                    cx,
                    SessionNotification::new(protocol_session_id.clone(), update),
                    cancellation,
                )
                .await
            {
                Ok(()) => {}
                Err(AcpOutboundSendError::UpdateTooLarge) => break StreamStop::UpdateTooLarge,
                Err(AcpOutboundSendError::Cancelled) => {
                    return Ok((PromptResponse::new(StopReason::Cancelled), true));
                }
                Err(AcpOutboundSendError::Transport(error)) => return Err(error),
            }
        };
        let message = match stop {
            StreamStop::Overflowed => {
                "Maple stopped this turn because its bounded ACP event stream overflowed."
            }
            StreamStop::UpdateTooLarge => {
                "Maple stopped this turn because one ACP update exceeded the 4 MiB transport limit."
            }
        };
        cancellation.cancel();
        match self
            .send_final_agent_message(cx, protocol_session_id.clone(), message, &self.lifetime)
            .await
        {
            Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {
                Ok((PromptResponse::new(StopReason::EndTurn), true))
            }
            Err(AcpOutboundSendError::Cancelled) => {
                Ok((PromptResponse::new(StopReason::Cancelled), true))
            }
            Err(AcpOutboundSendError::Transport(error)) => Err(error),
        }
    }

    /// The end of a turn's stream: a failed run tells its failure first.
    async fn finish_stream(
        &self,
        cx: &ConnectionTo<Client>,
        protocol_session_id: &SessionId,
        ended: AgentRunTerminal,
        failure: Option<AgentTimelineItem>,
        cancellation: &CancellationToken,
    ) -> Result<(PromptResponse, bool), agent_client_protocol::Error> {
        if ended == AgentRunTerminal::Failed
            && let Some(item) = failure
            && let Some(message) = event_error_text(&item)
        {
            let update = SessionUpdate::AgentMessageChunk(text_chunk(message, &item.id));
            match self
                .send_session_update(
                    cx,
                    SessionNotification::new(protocol_session_id.clone(), update),
                    cancellation,
                )
                .await
            {
                Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {}
                Err(AcpOutboundSendError::Cancelled) => {
                    return Ok((PromptResponse::new(StopReason::Cancelled), false));
                }
                Err(AcpOutboundSendError::Transport(error)) => return Err(error),
            }
        }
        prompt_result_from_terminal(ended).map(|response| (response, false))
    }

    /// The update that renames the session for the caller, when `title` is
    /// not the one it was last told.
    async fn title_update(&self, session_id: &str, title: String) -> Option<SessionUpdate> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(session_id)?;
        if session.advertised_title.as_deref() == Some(title.as_str()) {
            return None;
        }
        session.advertised_title = Some(title.clone());
        Some(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title(title),
        ))
    }

    async fn cancel(
        &self,
        notification: CancelNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id = canonical_session_id(&notification.session_id)?;
        self.cancel_session(&session_id).await;
        Ok(())
    }

    /// Stop the session's prompt, from its admission on, and its run.
    async fn cancel_session(&self, session_id: &str) {
        if let Some(state) = self.prompt_states.lock().await.get(session_id) {
            state.cancellation.cancel();
        }
    }

    async fn cleanup(&self) {
        let deadline = tokio::time::Instant::now() + ACP_CONNECTION_CLEANUP_TIMEOUT;
        {
            // Linearize closure with the last new-session commit. A task that
            // reaches finalization after this point observes closed and
            // discards its new task instead of committing it.
            let _finalization = self.finalization.lock().await;
            self.closed.store(true, Ordering::SeqCst);
            self.lifetime.cancel();
            self.bridge_environment.lock().await.clear();
            if self.has_credentials.swap(false, Ordering::SeqCst) {
                self.stats
                    .credential_connections
                    .fetch_sub(1, Ordering::SeqCst);
            }
        }
        // Every prompt and run stops with the connection's lifetime.
        for state in self.prompt_states.lock().await.values() {
            state.cancellation.cancel();
        }
        // Revoke every lease before awaiting anything else: no task can
        // launch another credential-bearing tool once this returns.
        let session_ids = {
            let sessions = self.sessions.lock().await;
            for session in sessions.values() {
                if let Some(lease) = session.lease.as_ref() {
                    lease.revoke();
                }
            }
            sessions.keys().cloned().collect::<Vec<_>>()
        };
        let mut retired_sessions = Vec::with_capacity(session_ids.len());
        for session_id in session_ids {
            let operation = self
                .session_operations
                .lock()
                .await
                .get(&session_id)
                .cloned();
            // A prompt marks the session as prompted while holding this gate.
            // Waiting here closes the admission gap before deciding whether a
            // newly created empty task may be discarded. At the cleanup
            // deadline the task is kept rather than risk deleting work whose
            // admission is still settling.
            let operation_drained = match operation {
                Some(operation) => {
                    tokio::time::timeout_at(deadline, Arc::clone(&operation.gate).lock_owned())
                        .await
                        .is_ok()
                }
                None => true,
            };
            let session = self.sessions.lock().await.remove(&session_id);
            if let Some(session) = session {
                let discard = operation_drained && session.created_here && !session.prompted;
                retired_sessions.push((session, discard));
                self.stats.active_sessions.fetch_sub(1, Ordering::SeqCst);
            }
        }
        // Take ownership of the current task set before awaiting it. A
        // prompt that is itself in this set may still publish to it; leaving
        // an empty shared set lets that proceed without a self-deadlock.
        let mut tasks = {
            let mut shared = self.background_tasks.lock().await;
            std::mem::take(&mut *shared)
        };
        for (mut session, discard) in retired_sessions {
            tasks.spawn(async move {
                if let Some(lease) = session.lease.take() {
                    if discard {
                        lease.discard_created_if_untouched().await;
                    } else {
                        lease.release().await;
                    }
                }
            });
        }
        'drain: loop {
            match tokio::time::timeout_at(deadline, tasks.join_next()).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    let mut shared = self.background_tasks.lock().await;
                    if shared.is_empty() {
                        break 'drain;
                    }
                    tasks = std::mem::take(&mut *shared);
                }
                Err(_) => {
                    // Session creation and the prompt path both cross
                    // persistent state before they return. Aborting them here
                    // could orphan that state; detaching keeps their closed
                    // checks and cleanup while keeping shutdown bounded.
                    tasks.detach_all();
                    self.background_tasks.lock().await.detach_all();
                    break 'drain;
                }
            }
        }
        self.session_operations.lock().await.clear();
        self.closing_sessions.lock().await.clear();
    }
}

fn notice_id() -> String {
    format!(
        "maple-acp-notice-{}",
        NEXT_ACP_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn retain_cancelled_caller_request<F, T>(response: F, reservation: AcpOutboundReservation)
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(async move {
        let _reservation = reservation;
        let _ = response.await;
    });
}

/// Wait until a run's terminal is known, or its sender is gone.
async fn wait_for_terminal(terminal: &mut tokio::sync::watch::Receiver<Option<AgentRunTerminal>>) {
    loop {
        if terminal.borrow().is_some() {
            return;
        }
        if terminal.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
