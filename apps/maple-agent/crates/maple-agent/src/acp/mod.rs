use crate::agent::{
    AGENT_TOOL_CONTEXT_INACTIVE_ERROR, AgentCreateSessionRequest, AgentHostEventPolicy,
    AgentPermissionDecision, AgentPermissionRequest, AgentRunEvent, AgentRunPermissionResponder,
    AgentRunTerminal, AgentRuntimeHandle, AgentSendMessageRequest, AgentTaskState,
    AgentTimelineItem,
};
mod config;
mod convert;
mod handler;
mod session;
mod transport;

pub use config::{AgentAcpConfig, AgentAcpPermissionMode, load_acp_config};
use config::{AgentAcpStats, normalize_config};
use convert::{
    AcpToolProjection, COMPACTION_COMPLETED_NOTICE, MAX_ACP_ERROR_CHARS, acp_available_commands,
    acp_config_options, acp_permission_decision, acp_permission_options, acp_permission_tool_call,
    acp_session_config_options, acp_session_modes, acp_usage, bounded_chars, event_error_text,
    internal_acp_error, outbound_error, parse_slash_command, project_trust_elicitation_request,
    project_trust_permission_decision, project_trust_permission_options, prompt_images,
    prompt_result_from_terminal, prompt_text, subagent_tool_update, timeline_update,
};
use handler::{AcpCallerSessionFields, MapleAcpHandler};
use session::{
    ALLOWED_BRIDGE_ENV, AcpConnectionContext, AcpPermissionResolution, AcpProjectTrustResolution,
    AcpPromptState, AcpSession, AcpSessionMode, AcpSessionOperation, SharedRuntimeStart,
    UnpublishedAcpSession, bridge_tool_context_spec, canonical_session_id,
    canonical_session_id_text, close_registration_may_be_released, ensure_acp_session_is_loadable,
    ensure_allowed_project_root, filter_bridge_environment, has_buzz_credentials,
    is_acp_loadable_session_mode, prepare_session_mcp,
};
use transport::{
    AcpOutboundReservation, AcpOutboundSendError, AcpOutboundTracker, BoundedLineReader,
    MAX_ACP_FRAME_BYTES, NEXT_ACP_MESSAGE_ID, tracked_outgoing_lines,
};

use agent_client_protocol::schema::v1::{
    CancelNotification, CloseSessionRequest, CloseSessionResponse, ConfigOptionUpdate,
    ContentBlock, ContentChunk, DeleteSessionRequest, DeleteSessionResponse, ElicitationAction,
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

/// Serve ACP on this process's stdin and stdout for one signed-in account.
///
/// `maple-gpui acp` calls this after it started the agent runtime. The
/// editor that spawned the process owns the connection: when its stdin
/// closes, every open session is cleaned up and the call returns. The
/// bridge environment (Buzz credentials and `PATH`) is read from this
/// process's own environment.
/// Build the shared lazy start `serve_stdio` expects from any start future.
pub fn shared_runtime_start<F>(start: F) -> SharedRuntimeStart
where
    F: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    futures_util::FutureExt::shared(Box::pin(start))
}

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

    let peer_eof = CancellationToken::new();
    let read = BoundedLineReader::new(tokio::io::stdin(), peer_eof.clone());
    let incoming =
        FramedRead::new(read, LinesCodec::new_with_max_length(MAX_ACP_FRAME_BYTES)).map(|result| {
            result.map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        });
    let outgoing = tracked_outgoing_lines(tokio::io::stdout(), Arc::clone(&context.outbound));
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
        let command_root = project_root.clone();
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
        let (environment, transient_mcp_servers) =
            prepare_session_mcp(&bridge_environment, &request.mcp_servers)?;
        let tool_context = bridge_tool_context_spec(&environment).map_err(internal_acp_error)?;
        let mode = AcpSessionMode::default();
        let created = self
            .agent
            .create_session_with_surface_context(
                Some(AgentCreateSessionRequest {
                    project_root: Some(project_root.to_string_lossy().into_owned()),
                    title: Some(
                        caller
                            .session_title
                            .unwrap_or_else(|| "Maple ACP".to_string()),
                    ),
                    model: Some(model.clone()),
                    context_limit: None,
                    mode: Some(mode.maple_mode().to_string()),
                    mcp_server_names: None,
                    system_prompt: caller.system_prompt,
                }),
                Some(tool_context),
                transient_mcp_servers,
                self.lifetime.child_token(),
                // The ACP caller is the only interactive surface for this task.
                // Persisted history remains loadable in Maple Desktop, but live
                // permission cards must never create a second approval broker.
                AgentHostEventPolicy::Suppress,
            )
            .await
            .map_err(internal_acp_error)?;
        // Own the lease before anything can fail, so an early return
        // releases the tool context instead of leaking it.
        let lease = created
            .tool_context_lease
            .expect("an explicit Agent tool context must return a lease");
        let unpublished = UnpublishedAcpSession::new(lease);
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
                context_limit: None,
                advertised_title: Some(created.detail.session.title.clone()),
                message_count: created.detail.session.message_count,
                created_here: true,
                prompted: false,
                project_root: project_root.clone(),
                project_trust_decision: project_trust.decision,
                mode,
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
        self.send_available_commands(cx, &session_id, &command_root)
            .await;
        Ok(NewSessionResponse::new(session_id)
            .modes(acp_session_modes(AcpSessionMode::default()))
            .config_options(acp_config_options(
                &model,
                &available_models,
                AcpSessionMode::default(),
            )))
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
        let result = tokio::select! {
            biased;
            _ = self.lifetime.cancelled() => Err(
                agent_client_protocol::Error::internal_error()
                    .data("The Maple ACP connection closed while the runtime started"),
            ),
            result = start => result
                .map_err(|error| internal_acp_error(format!("Failed to start the Agent runtime: {error}"))),
        };
        result
    }

    /// Resolve the session model's context window, cached per session. A
    /// `usage_update` needs both `used` and `size`, so an unknown window
    /// means no update is sent rather than a fabricated size.
    async fn session_context_limit(&self, session_id: &str, model: &str) -> Option<u64> {
        if let Some(cached) = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|session| session.context_limit)
        {
            return Some(cached);
        }
        let limit = self
            .agent
            .context_limit_for_model(model)
            .await
            .ok()
            .flatten()
            .and_then(|tokens| u64::try_from(tokens).ok());
        if let Some(limit) = limit
            && let Some(session) = self.sessions.lock().await.get_mut(session_id)
        {
            session.context_limit = Some(limit);
        }
        limit
    }

    /// Save a caller-selected mode to the task row so a later connection
    /// loads it. Runtime policy follows on the next prompt regardless,
    /// because every prompt carries the live mode.
    async fn persist_session_mode(
        &self,
        session_id: &str,
        mode: AcpSessionMode,
    ) -> Result<(), agent_client_protocol::Error> {
        self.agent
            .persist_session_permission_mode(session_id, mode.maple_mode())
            .await
            .map_err(internal_acp_error)
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
            .list_sessions(Some(project_root_text.clone()))
            .await
            .map_err(internal_acp_error)?;
        let persisted = ensure_acp_session_is_loadable(&persisted_sessions, &session_id)
            .map_err(|error| agent_client_protocol::Error::invalid_request().data(error))?;
        let persisted_model = persisted.model.clone();
        let session_mode = match persisted.mode.as_str() {
            "auto" => AcpSessionMode::ApproveAll,
            _ => AcpSessionMode::Interactive,
        };
        let available_models = self.available_models().await?;
        if let Some(model) = persisted_model.as_ref()
            && !available_models.iter().any(|available| available == model)
        {
            return Err(agent_client_protocol::Error::invalid_request().data(format!(
                    "This Maple Agent task uses model '{model}', which is no longer available; the task remains available in Maple Desktop"
                )));
        }
        let bridge_environment = self.bridge_environment.lock().await.clone();
        let (environment, transient_mcp_servers) =
            prepare_session_mcp(&bridge_environment, &request.mcp_servers)?;
        let tool_context = bridge_tool_context_spec(&environment).map_err(internal_acp_error)?;
        let protocol_session_id = SessionId::new(session_id.clone());
        let operation = AcpSessionOperation::new(&self.lifetime);
        let operation_guard = Arc::clone(&operation.gate).lock_owned().await;
        {
            // Register the operation before the fallible core attach. Close can
            // now mark it closing and wait, while disconnect linearizes through
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
            .attach_session_with_surface_context(
                session_id.clone(),
                project_root_text,
                tool_context,
                transient_mcp_servers,
                operation.cancellation.child_token(),
            )
            .await
        {
            Ok(attached) => attached,
            Err(error) => {
                self.remove_session_operation_if_same(&session_id, &operation)
                    .await;
                return Err(internal_acp_error(error));
            }
        };
        let lease = attached
            .tool_context_lease
            .expect("an attached ACP task must return a tool-context lease");
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
        let command_root = project_root.clone();
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
                context_limit: None,
                advertised_title: Some(persisted.title.clone()),
                message_count,
                created_here: false,
                prompted: false,
                project_root,
                project_trust_decision: project_trust.decision,
                mode: session_mode,
            },
        );
        drop(sessions);
        self.stats.active_sessions.fetch_add(1, Ordering::SeqCst);
        drop(finalization);

        let mut projection = AcpToolProjection::default();
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
                self.retire_session(&session_id).await;
                return Err(outbound_error(error));
            }
        }
        drop(operation_guard);
        self.send_available_commands(cx, &session_id, &command_root)
            .await;
        Ok(LoadSessionResponse::new()
            .modes(acp_session_modes(session_mode))
            .config_options(acp_session_config_options(
                &model,
                &available_models,
                message_count,
                session_mode,
            )))
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
                is_acp_loadable_session_mode(&session.mode)
                    && ensure_allowed_project_root(
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
        operation.cancellation.cancel();
        // Use one absolute deadline for every potentially blocking close
        // phase. Paseo awaits this response before terminating the ACP child,
        // so a fresh timeout per phase could still hang it for multiples of
        // the advertised close bound.
        let close_deadline = tokio::time::Instant::now() + ACP_SESSION_CLOSE_TIMEOUT;
        let cancel_result =
            tokio::time::timeout_at(close_deadline, self.cancel_session(&session_id)).await;
        let cancellation_completed = matches!(&cancel_result, Ok(Ok(())));
        // Starting and running prompts normally retain this guard through their
        // terminal barrier. A broken provider must not make ACP close hang
        // forever, though: after the bound, revoke the lease synchronously and
        // let its Drop retry exact-match cleanup while Paseo can terminate the
        // child process.
        let operation_guard =
            tokio::time::timeout_at(close_deadline, Arc::clone(&operation.gate).lock_owned())
                .await
                .ok();
        let operation_drained = operation_guard.is_some();
        let Some(mut session) = self.sessions.lock().await.remove(&session_id) else {
            if close_registration_may_be_released(cancellation_completed, operation_drained, true) {
                self.remove_session_operation_if_same(&session_id, &operation)
                    .await;
                self.closing_sessions.lock().await.remove(&session_id);
            }
            if let Ok(cancel_result) = cancel_result {
                cancel_result?;
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
        if close_registration_may_be_released(
            cancellation_completed,
            operation_drained,
            cleanup_completed,
        ) {
            self.remove_session_operation_if_same(&session_id, &operation)
                .await;
            self.closing_sessions.lock().await.remove(&session_id);
        }
        if let Ok(cancel_result) = cancel_result {
            cancel_result?;
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
        let (response, pending_mode) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.get_mut(&session_id).ok_or_else(|| {
                agent_client_protocol::Error::resource_not_found(Some(session_id.clone()))
                    .data("ACP session is not owned by this connection")
            })?;
            let mut pending_mode: Option<AcpSessionMode> = None;
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
                        return Err(agent_client_protocol::Error::invalid_params()
                            .data("Unknown Maple model"));
                    }
                    if session.message_count > 0 && session.model != model {
                        return Err(agent_client_protocol::Error::invalid_params()
                            .data("Maple tasks are model-locked after their first message"));
                    }
                    session.model = model.to_string();
                    session.context_limit = None;
                }
                "mode" => {
                    let mode =
                        AcpSessionMode::parse(selected_value.0.as_ref()).ok_or_else(|| {
                            agent_client_protocol::Error::invalid_params()
                                .data("Unknown Maple ACP permission mode")
                        })?;
                    session.mode = mode;
                    pending_mode = Some(mode);
                }
                _ => {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("Unknown Maple ACP configuration option"));
                }
            }
            let response = SetSessionConfigOptionResponse::new(session.config_options());
            (response, pending_mode)
        };
        // Persist outside the sessions lock; the write only decides what a
        // later connection loads, while every prompt carries the live mode.
        if let Some(mode) = pending_mode {
            self.persist_session_mode(&session_id, mode).await?;
        }
        Ok(response)
    }

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
        let mode = AcpSessionMode::parse(request.mode_id.0.as_ref()).ok_or_else(|| {
            agent_client_protocol::Error::invalid_params().data("Unknown Maple ACP permission mode")
        })?;
        self.persist_session_mode(&session_id, mode).await?;
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.mode = mode;
        }
        Ok(SetSessionModeResponse::new())
    }

    async fn begin_prompt(
        &self,
        request: &PromptRequest,
    ) -> Result<
        (
            String,
            Vec<crate::agent::AgentImageUpload>,
            bool,
            String,
            CancellationToken,
            tokio::sync::OwnedMutexGuard<()>,
        ),
        agent_client_protocol::Error,
    > {
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
        let operation_guard = Arc::clone(&operation.gate).lock_owned().await;
        if self.closed.load(Ordering::SeqCst)
            || self.closing_sessions.lock().await.contains(&session_id)
            || !self.sessions.lock().await.contains_key(&session_id)
        {
            return Err(
                agent_client_protocol::Error::resource_not_found(Some(session_id.clone()))
                    .data("ACP session is not available on this connection"),
            );
        }
        // The catalog decides how prompt images travel for this turn:
        // embedded for vision models, through the read_image helper for
        // everyone else. Unknown models fail closed to the helper, which
        // still sees the image, so no prompt is ever rejected or silently
        // dropped. Text-only prompts skip the catalog round-trip; they pass
        // the same value desktop does for a turn without attachments.
        let vision_capable = if images.is_empty() {
            false
        } else {
            let model = self
                .sessions
                .lock()
                .await
                .get(&session_id)
                .map(|session| session.model.clone());
            match model {
                Some(model) => matches!(
                    self.agent
                        .model_supports_vision(&model)
                        .await
                        .map_err(internal_acp_error)?,
                    Some(true)
                ),
                None => false,
            }
        };
        let mut states = self.prompt_states.lock().await;
        if states.contains_key(&session_id) {
            return Err(agent_client_protocol::Error::invalid_request()
                .data("This ACP session already has an active prompt"));
        }
        let cancellation = operation.cancellation.child_token();
        states.insert(
            session_id.clone(),
            AcpPromptState::Starting {
                cancellation: cancellation.clone(),
            },
        );
        Ok((
            prompt,
            images,
            vision_capable,
            session_id,
            cancellation,
            operation_guard,
        ))
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
    /// and end the turn. No model turn runs, so only the prompt state that
    /// `begin_prompt` registered has to be settled.
    async fn run_compact_command(
        &self,
        cx: &ConnectionTo<Client>,
        protocol_session_id: &SessionId,
        session_id: &str,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        self.prompt_states.lock().await.remove(session_id);
        let compacted = self.agent.compact_session(session_id.to_string()).await;
        match compacted {
            Ok(()) => {
                match self
                    .send_final_agent_message(
                        cx,
                        protocol_session_id.clone(),
                        COMPACTION_COMPLETED_NOTICE,
                        &self.lifetime,
                    )
                    .await
                {
                    // The compaction itself succeeded; a failed notice must
                    // not turn the command into an error for the caller.
                    Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {}
                    Err(AcpOutboundSendError::Cancelled) => {
                        return Ok(PromptResponse::new(StopReason::Cancelled));
                    }
                    Err(AcpOutboundSendError::Transport(error)) => return Err(error),
                }
                Ok(PromptResponse::new(StopReason::EndTurn))
            }
            Err(error) => Err(internal_acp_error(error)),
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
                SessionUpdate::AgentMessageChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(message.to_string())))
                        .message_id(message_id.as_str()),
                ),
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
            retain_cancelled_permission_request(response_future, reservation);
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
            ElicitationAction::Cancel => Ok(None),
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
                    "Trusting '{}' allows Maple to use project-provided guidance, including agent skills. These instructions can influence how agents work and use tools. Normal tool permissions still apply.",
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
            retain_cancelled_permission_request(response_future, reservation);
            return Ok(None);
        };
        drop(reservation);
        let response = response.map_err(AcpOutboundSendError::Transport)?;
        project_trust_permission_decision(&response.outcome)
            .map_err(|error| AcpOutboundSendError::Transport(internal_acp_error(error)))
    }

    async fn resolve_project_trust_before_prompt(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<AcpProjectTrustResolution, agent_client_protocol::Error> {
        let (project_root, configured_decision, tool_context_access) = {
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
                .set_project_trust_for_surface_session(
                    project_root.to_string_lossy().into_owned(),
                    trusted,
                    tool_context_access.clone(),
                )
                .await
                .map_err(internal_acp_error)?;
        } else if status.decision != configured_decision {
            let trusted = status.decision.unwrap_or(false);
            status = self
                .agent
                .set_project_trust_for_surface_session(
                    project_root.to_string_lossy().into_owned(),
                    trusted,
                    tool_context_access,
                )
                .await
                .map_err(internal_acp_error)?;
        }
        if let Some(session) = self.sessions.lock().await.get_mut(session_id) {
            session.project_trust_decision = status.decision;
        }
        Ok(AcpProjectTrustResolution::Continue)
    }

    async fn request_permission_from_caller(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: SessionId,
        request: AgentPermissionRequest,
        item: &AgentTimelineItem,
        responder: &AgentRunPermissionResponder,
        cancellation: &CancellationToken,
    ) -> Result<AcpPermissionResolution, AcpOutboundSendError> {
        let tool_call = acp_permission_tool_call(&request, item);
        let permission_request =
            RequestPermissionRequest::new(session_id, tool_call.into(), acp_permission_options());
        let encoded_bytes = serde_json::to_vec(&permission_request)
            .map_err(|error| {
                AcpOutboundSendError::Transport(internal_acp_error(format!(
                    "Failed to encode Maple ACP permission request: {error}"
                )))
            })?
            .len();
        // Permission requests use the same global event/byte budget as streamed
        // notifications. Hold the reservation until the caller responds so a
        // slow client cannot accumulate unbounded JSON-RPC request frames.
        let reservation = self.outbound.reserve(encoded_bytes, cancellation).await?;
        if cancellation.is_cancelled() {
            cancel_maple_permission(responder, &request.request_id).await;
            return Ok(AcpPermissionResolution::Cancelled);
        }
        let sent_request = cx.send_request(permission_request);
        let mut response_future = Box::pin(sent_request.block_task());
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            response = &mut response_future => Some(response),
        };
        let Some(response) = response else {
            // ACP v1 has no stable request-cancellation primitive. Stop Maple
            // immediately, but keep consuming the already-sent JSON-RPC request
            // and retain its outbound credits until the client replies or the
            // connection closes. Otherwise a cancel-and-never-reply client can
            // accumulate unbounded SDK correlation entries outside our limit.
            cancel_maple_permission(responder, &request.request_id).await;
            retain_cancelled_permission_request(response_future, reservation);
            return Ok(AcpPermissionResolution::Cancelled);
        };
        drop(reservation);

        let (decision, resolution) = match response {
            Ok(response) => match acp_permission_decision(&response.outcome) {
                Ok(AgentPermissionDecision::Cancel) => (
                    AgentPermissionDecision::Cancel,
                    AcpPermissionResolution::Cancelled,
                ),
                Ok(decision) => (decision, AcpPermissionResolution::Continue),
                Err(error) => {
                    cancel_maple_permission(responder, &request.request_id).await;
                    return Err(AcpOutboundSendError::Transport(internal_acp_error(error)));
                }
            },
            Err(error) => {
                cancel_maple_permission(responder, &request.request_id).await;
                return Err(AcpOutboundSendError::Transport(error));
            }
        };

        if let Err(error) = responder.respond(request.request_id, decision).await {
            if cancellation.is_cancelled() {
                return Ok(AcpPermissionResolution::Cancelled);
            }
            return Err(AcpOutboundSendError::Transport(internal_acp_error(
                format!("Failed to resolve Maple permission request: {error}"),
            )));
        }
        Ok(resolution)
    }

    #[allow(clippy::too_many_arguments)]
    async fn prompt(
        self: &Arc<Self>,
        cx: &ConnectionTo<Client>,
        session_id: String,
        prompt: String,
        images: Vec<crate::agent::AgentImageUpload>,
        vision_capable: bool,
        prompt_lifetime: CancellationToken,
        operation_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        let mut operation_guard = Some(operation_guard);
        let protocol_session_id = SessionId::new(session_id.clone());
        match self
            .resolve_project_trust_before_prompt(cx, &session_id, &prompt_lifetime)
            .await
        {
            Ok(AcpProjectTrustResolution::Continue) => {}
            Ok(AcpProjectTrustResolution::Cancelled) => {
                self.prompt_states.lock().await.remove(&session_id);
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            Err(error) => {
                self.prompt_states.lock().await.remove(&session_id);
                return Err(error);
            }
        }
        let (tool_context_access, model, session_mode, project_root) = {
            let sessions = self.sessions.lock().await;
            match sessions.get(&session_id) {
                Some(session) => (
                    session
                        .lease
                        .as_ref()
                        .expect("a runnable ACP session must own a lease")
                        .access(),
                    session.model.clone(),
                    session.mode,
                    session.project_root.clone(),
                ),
                None => {
                    drop(sessions);
                    self.prompt_states.lock().await.remove(&session_id);
                    return Err(agent_client_protocol::Error::resource_not_found(Some(
                        session_id.clone(),
                    ))
                    .data("ACP session is no longer owned by this connection"));
                }
            }
        };
        // Semantic titles land between turns, after the previous run's
        // event loop exited; catch up before the next one starts.
        if let Ok(Some(title)) = self.agent.session_display_title(&session_id).await {
            let changed = {
                let mut sessions = self.sessions.lock().await;
                match sessions.get_mut(&session_id) {
                    Some(session)
                        if session.advertised_title.as_deref() != Some(title.as_str()) =>
                    {
                        session.advertised_title = Some(title.clone());
                        Some(title)
                    }
                    _ => None,
                }
            };
            if let Some(title) = changed {
                let notification = SessionNotification::new(
                    protocol_session_id.clone(),
                    SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title)),
                );
                match self
                    .send_session_update(cx, notification, &prompt_lifetime)
                    .await
                {
                    Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {}
                    Err(AcpOutboundSendError::Cancelled) => {
                        self.prompt_states.lock().await.remove(&session_id);
                        return Ok(PromptResponse::new(StopReason::Cancelled));
                    }
                    Err(AcpOutboundSendError::Transport(error)) => {
                        self.prompt_states.lock().await.remove(&session_id);
                        return Err(error);
                    }
                }
            }
        }
        let mut prompt = prompt;
        if let Some((name, args)) = parse_slash_command(&prompt) {
            if name == "compact" {
                return self
                    .run_compact_command(cx, &protocol_session_id, &session_id)
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
            .send_message_with_tool_context(
                AgentSendMessageRequest {
                    session_id: session_id.clone(),
                    text: prompt,
                    model: Some(model.clone()),
                    context_limit: None,
                    mode: Some(session_mode.maple_mode().to_string()),
                    vision_capable,
                    steer: false,
                    queue_id: None,
                    attachments: images,
                },
                tool_context_access,
                prompt_lifetime.clone(),
                AgentHostEventPolicy::Suppress,
            )
            .await
        {
            Ok(run) => run,
            Err(_) if prompt_lifetime.is_cancelled() => {
                self.prompt_states.lock().await.remove(&session_id);
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            Err(error) if error == AGENT_TOOL_CONTEXT_INACTIVE_ERROR => {
                self.prompt_states.lock().await.remove(&session_id);
                self.retire_session(&session_id).await;
                return Err(agent_client_protocol::Error::resource_not_found(Some(
                    session_id.clone(),
                ))
                .data("The Maple Agent task was removed outside this ACP connection"));
            }
            Err(error) => {
                self.prompt_states.lock().await.remove(&session_id);
                return Err(internal_acp_error(error));
            }
        };
        let locked_config_options = {
            let mut sessions = self.sessions.lock().await;
            sessions.get_mut(&session_id).and_then(|session| {
                session.prompted = true;
                let first_message = session.message_count == 0;
                session.message_count = session.message_count.saturating_add(1);
                first_message.then(|| session.config_options())
            })
        };
        let mut events = run.events;
        let mut terminal = run.terminal;
        let usage = run.usage;
        let event_overflowed = run.event_overflowed;
        let Some(run_cancellation) = run.cancellation else {
            prompt_lifetime.cancel();
            self.prompt_states.lock().await.remove(&session_id);
            return Err(agent_client_protocol::Error::internal_error()
                .data("Maple did not create an ACP cancellation capability for this run"));
        };
        let Some(permission_responder) = run.permission_responder else {
            prompt_lifetime.cancel();
            let _ = run_cancellation.cancel().await;
            self.prompt_states.lock().await.remove(&session_id);
            return Err(agent_client_protocol::Error::internal_error()
                .data("Maple did not create an ACP permission responder for this run"));
        };
        let prompt_registered = {
            let mut states = self.prompt_states.lock().await;
            match states.get_mut(&session_id) {
                Some(state @ AcpPromptState::Starting { .. }) => {
                    *state = AcpPromptState::Running {
                        cancellation: prompt_lifetime.clone(),
                        run_cancellation: Box::new(run_cancellation.clone()),
                    };
                    true
                }
                _ => false,
            }
        };
        if !prompt_registered {
            let _ = run_cancellation.cancel().await;
            return Err(agent_client_protocol::Error::internal_error()
                .data("The Maple ACP connection closed while starting the prompt"));
        }
        self.stats.active_runs.fetch_add(1, Ordering::SeqCst);
        if prompt_lifetime.is_cancelled() {
            // A cancellation failure does not make the active Maple run
            // disappear. Keep listening so its lifecycle remains tracked.
            let _ = run_cancellation.cancel().await;
        }

        if let Some(config_options) = locked_config_options {
            match self
                .send_session_update(
                    cx,
                    SessionNotification::new(
                        protocol_session_id.clone(),
                        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(config_options)),
                    ),
                    &prompt_lifetime,
                )
                .await
            {
                Ok(()) => {}
                Err(AcpOutboundSendError::Cancelled) => {
                    let _ = run_cancellation.cancel().await;
                    if matches!(
                        self.prompt_states.lock().await.remove(&session_id),
                        Some(AcpPromptState::Running { .. })
                    ) {
                        self.stats.active_runs.fetch_sub(1, Ordering::SeqCst);
                    }
                    return Ok(PromptResponse::new(StopReason::Cancelled));
                }
                Err(AcpOutboundSendError::UpdateTooLarge) => {
                    let _ = run_cancellation.cancel().await;
                    if matches!(
                        self.prompt_states.lock().await.remove(&session_id),
                        Some(AcpPromptState::Running { .. })
                    ) {
                        self.stats.active_runs.fetch_sub(1, Ordering::SeqCst);
                    }
                    return Err(agent_client_protocol::Error::internal_error()
                        .data("Maple's locked model selector exceeded the ACP update limit"));
                }
                Err(AcpOutboundSendError::Transport(error)) => {
                    let _ = run_cancellation.cancel().await;
                    if matches!(
                        self.prompt_states.lock().await.remove(&session_id),
                        Some(AcpPromptState::Running { .. })
                    ) {
                        self.stats.active_runs.fetch_sub(1, Ordering::SeqCst);
                    }
                    return Err(error);
                }
            }
        }

        let mut cancel_after_result = false;
        let mut tool_projection = AcpToolProjection::default();
        let result = loop {
            if event_overflowed.load(Ordering::Acquire) {
                cancel_after_result = true;
                let _ = run_cancellation.cancel().await;
                match self
                    .send_final_agent_message(
                        cx,
                        protocol_session_id.clone(),
                        "Maple stopped this turn because its bounded ACP event stream overflowed.",
                        &self.lifetime,
                    )
                    .await
                {
                    Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {
                        break Ok(PromptResponse::new(StopReason::EndTurn));
                    }
                    Err(AcpOutboundSendError::Cancelled) => {
                        break Ok(PromptResponse::new(StopReason::Cancelled));
                    }
                    Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                }
            }
            let event = events.recv().await;
            if event_overflowed.load(Ordering::Acquire) {
                cancel_after_result = true;
                let _ = run_cancellation.cancel().await;
                match self
                    .send_final_agent_message(
                        cx,
                        protocol_session_id.clone(),
                        "Maple stopped this turn because its bounded ACP event stream overflowed.",
                        &self.lifetime,
                    )
                    .await
                {
                    Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {
                        break Ok(PromptResponse::new(StopReason::EndTurn));
                    }
                    Err(AcpOutboundSendError::Cancelled) => {
                        break Ok(PromptResponse::new(StopReason::Cancelled));
                    }
                    Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                }
            }
            match event {
                Some(AgentRunEvent::TimelineItem(item)) => {
                    if let Some(update) = timeline_update(&item, &mut tool_projection, false) {
                        match self
                            .send_session_update(
                                cx,
                                SessionNotification::new(protocol_session_id.clone(), update),
                                &prompt_lifetime,
                            )
                            .await
                        {
                            Ok(()) => {}
                            Err(AcpOutboundSendError::UpdateTooLarge) => {
                                cancel_after_result = true;
                                let _ = run_cancellation.cancel().await;
                                match self.send_final_agent_message(
                                    cx,
                                    protocol_session_id.clone(),
                                    "Maple stopped this turn because one ACP update exceeded the 4 MiB transport limit.",
                                    &self.lifetime,
                                )
                                .await
                                {
                                    Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {
                                        break Ok(PromptResponse::new(StopReason::EndTurn));
                                    }
                                    Err(AcpOutboundSendError::Cancelled) => {
                                        break Ok(PromptResponse::new(StopReason::Cancelled));
                                    }
                                    Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                                }
                            }
                            Err(AcpOutboundSendError::Cancelled) => {
                                cancel_after_result = true;
                                break Ok(PromptResponse::new(StopReason::Cancelled));
                            }
                            Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                        }
                    }
                }
                Some(AgentRunEvent::PermissionRequested {
                    request: permission,
                    item,
                }) => {
                    match self
                        .request_permission_from_caller(
                            cx,
                            protocol_session_id.clone(),
                            permission,
                            &item,
                            &permission_responder,
                            &prompt_lifetime,
                        )
                        .await
                    {
                        Ok(AcpPermissionResolution::Continue) => {}
                        Ok(AcpPermissionResolution::Cancelled) => {
                            cancel_after_result = true;
                            break Ok(PromptResponse::new(StopReason::Cancelled));
                        }
                        Err(AcpOutboundSendError::UpdateTooLarge) => {
                            cancel_after_result = true;
                            let _ = run_cancellation.cancel().await;
                            match self
                                .send_final_agent_message(
                                    cx,
                                    protocol_session_id.clone(),
                                    "Maple stopped this turn because one ACP permission request exceeded the 4 MiB transport limit.",
                                    &self.lifetime,
                                )
                                .await
                            {
                                Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {
                                    break Ok(PromptResponse::new(StopReason::EndTurn));
                                }
                                Err(AcpOutboundSendError::Cancelled) => {
                                    break Ok(PromptResponse::new(StopReason::Cancelled));
                                }
                                Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                            }
                        }
                        Err(AcpOutboundSendError::Cancelled) => {
                            cancel_after_result = true;
                            break Ok(PromptResponse::new(StopReason::Cancelled));
                        }
                        Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                    }
                }
                Some(AgentRunEvent::Error(item)) => {
                    if let Some(message) = event_error_text(&item) {
                        match self
                            .send_session_update(
                                cx,
                                SessionNotification::new(
                                    protocol_session_id.clone(),
                                    SessionUpdate::AgentMessageChunk(
                                        ContentChunk::new(ContentBlock::Text(TextContent::new(
                                            message,
                                        )))
                                        .message_id(item.id.as_str()),
                                    ),
                                ),
                                &prompt_lifetime,
                            )
                            .await
                        {
                            Ok(()) => {}
                            Err(AcpOutboundSendError::UpdateTooLarge) => {
                                cancel_after_result = true;
                                let _ = run_cancellation.cancel().await;
                                match self.send_final_agent_message(
                                    cx,
                                    protocol_session_id.clone(),
                                    "Maple stopped this turn because one ACP update exceeded the 4 MiB transport limit.",
                                    &self.lifetime,
                                )
                                .await
                                {
                                    Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {
                                        break Ok(PromptResponse::new(StopReason::EndTurn));
                                    }
                                    Err(AcpOutboundSendError::Cancelled) => {
                                        break Ok(PromptResponse::new(StopReason::Cancelled));
                                    }
                                    Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                                }
                            }
                            Err(AcpOutboundSendError::Cancelled) => {
                                cancel_after_result = true;
                                break Ok(PromptResponse::new(StopReason::Cancelled));
                            }
                            Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                        }
                    }
                }
                Some(AgentRunEvent::Finished(terminal)) => {
                    break prompt_result_from_terminal(terminal);
                }
                Some(AgentRunEvent::HistoryReplaced) => {
                    // The client keeps its own transcript and cannot reload
                    // ours, so only say that the compaction finished.
                    match self
                        .send_final_agent_message(
                            cx,
                            protocol_session_id.clone(),
                            COMPACTION_COMPLETED_NOTICE,
                            &prompt_lifetime,
                        )
                        .await
                    {
                        Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {}
                        Err(AcpOutboundSendError::Cancelled) => {
                            cancel_after_result = true;
                            break Ok(PromptResponse::new(StopReason::Cancelled));
                        }
                        Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                    }
                }
                // A subagent has no place of its own in ACP, so its work
                // rides the `delegate` tool call that owns it.
                Some(
                    AgentRunEvent::SubagentStarted { id, task, .. }
                    | AgentRunEvent::SubagentActivity { id, tool: task },
                ) => {
                    match self
                        .send_session_update(
                            cx,
                            SessionNotification::new(
                                protocol_session_id.clone(),
                                subagent_tool_update(&id, task),
                            ),
                            &prompt_lifetime,
                        )
                        .await
                    {
                        Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {}
                        Err(AcpOutboundSendError::Cancelled) => {
                            cancel_after_result = true;
                            break Ok(PromptResponse::new(StopReason::Cancelled));
                        }
                        Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                    }
                }
                // The end of a subagent is the end of its tool call, which
                // carries the result. Replacing that content with a notice
                // would drop what the caller came for.
                // A semantic title landing mid-run renames the task; keep
                // the caller's session list in sync.
                Some(AgentRunEvent::SessionUpdated(summary)) => {
                    let changed = {
                        let mut sessions = self.sessions.lock().await;
                        match sessions.get_mut(&session_id) {
                            Some(session) => {
                                if session.advertised_title.as_deref()
                                    == Some(summary.title.as_str())
                                {
                                    None
                                } else {
                                    session.advertised_title = Some(summary.title.clone());
                                    Some(summary.title.clone())
                                }
                            }
                            None => None,
                        }
                    };
                    if let Some(title) = changed {
                        let notification = SessionNotification::new(
                            protocol_session_id.clone(),
                            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title)),
                        );
                        match self
                            .send_session_update(cx, notification, &prompt_lifetime)
                            .await
                        {
                            Ok(()) | Err(AcpOutboundSendError::UpdateTooLarge) => {}
                            Err(AcpOutboundSendError::Cancelled) => {
                                cancel_after_result = true;
                                break Ok(PromptResponse::new(StopReason::Cancelled));
                            }
                            Err(AcpOutboundSendError::Transport(error)) => break Err(error),
                        }
                    }
                }
                Some(
                    AgentRunEvent::Started
                    | AgentRunEvent::SetupWarning(_)
                    | AgentRunEvent::SubagentFinished { .. }
                    | AgentRunEvent::QueueChanged(_)
                    | AgentRunEvent::QueuePromoted { .. },
                ) => {}
                None => {
                    let current_terminal = *terminal.borrow();
                    let fallback = match current_terminal {
                        Some(terminal) => Some(terminal),
                        None => match terminal.changed().await {
                            Ok(()) => *terminal.borrow_and_update(),
                            Err(_) => *terminal.borrow(),
                        },
                    };
                    if let Some(terminal) = fallback {
                        break prompt_result_from_terminal(terminal);
                    }
                    break Err(agent_client_protocol::Error::internal_error()
                        .data("Maple Agent run ended without a terminal result"));
                }
            }
        };
        let turn_usage = usage.borrow().as_ref().copied().unwrap_or_default();
        // ACP defines PromptResponse.usage as usage for this prompt turn. Paseo
        // stores it as currentTurnUsage, so cumulative session totals would be
        // double-counted on every later turn.
        let result = result.map(|response| response.usage(acp_usage(turn_usage)));
        // ACP's native context indicator: one usage_update per completed
        // turn carrying the tokens now in context and the model's window.
        if let Some(size) = self.session_context_limit(&session_id, &model).await {
            let notification = SessionNotification::new(
                protocol_session_id.clone(),
                SessionUpdate::UsageUpdate(UsageUpdate::new(turn_usage.total_tokens, size)),
            );
            if let Err(error) = self
                .send_session_update(cx, notification, &prompt_lifetime)
                .await
            {
                log::warn!("Failed to send the Maple ACP usage update: {error:?}");
            }
        }
        let mut deferred_prompt_cleanup = false;
        if cancel_after_result {
            // Synthetic stream stops settle only after the underlying run has
            // drained, or retain a same-session fence while it finishes in the
            // background. A completed run makes this cancellation a no-op.
            let _ = run_cancellation.cancel().await;
            if tokio::time::timeout(
                ACP_SYNTHETIC_STOP_DRAIN_TIMEOUT,
                wait_for_retained_terminal(&mut terminal),
            )
            .await
            .is_err()
            {
                // Do not let Buzz start a replacement turn against the same
                // Goose session while cancellation is still draining. The ACP
                // response remains bounded, while this retained state and task
                // own the terminal barrier asynchronously.
                deferred_prompt_cleanup = true;
                let context = Arc::clone(self);
                let draining_session_id = session_id.clone();
                let draining_operation_guard = operation_guard
                    .take()
                    .expect("a deferred ACP prompt must retain its session operation fence");
                let mut tasks = self.background_tasks.lock().await;
                tasks.spawn(async move {
                    let _operation_guard = draining_operation_guard;
                    wait_for_retained_terminal(&mut terminal).await;
                    if matches!(
                        context
                            .prompt_states
                            .lock()
                            .await
                            .remove(&draining_session_id),
                        Some(AcpPromptState::Running { .. })
                    ) {
                        context.stats.active_runs.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        } else if result.is_err() {
            let _ = run_cancellation.cancel().await;
        }
        if !deferred_prompt_cleanup
            && matches!(
                self.prompt_states.lock().await.remove(&session_id),
                Some(AcpPromptState::Running { .. })
            )
        {
            self.stats.active_runs.fetch_sub(1, Ordering::SeqCst);
        }
        drop(operation_guard);
        result
    }

    async fn cancel(
        &self,
        notification: CancelNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id = canonical_session_id(&notification.session_id)?;
        self.cancel_session(&session_id).await
    }

    async fn cancel_session(&self, session_id: &str) -> Result<(), agent_client_protocol::Error> {
        let (cancellation, run_cancellation) = {
            let states = self.prompt_states.lock().await;
            match states.get(session_id) {
                Some(AcpPromptState::Starting { cancellation }) => {
                    (Some(cancellation.clone()), None)
                }
                Some(AcpPromptState::Running {
                    cancellation,
                    run_cancellation,
                }) => (Some(cancellation.clone()), Some(run_cancellation.clone())),
                None => (None, None),
            }
        };
        if let Some(cancellation) = cancellation {
            // This token reaches core setup before a run ID exists and fences
            // the worker start once core setup completes.
            cancellation.cancel();
        }
        if let Some(run_cancellation) = run_cancellation {
            run_cancellation
                .cancel()
                .await
                .map_err(internal_acp_error)?;
        }
        Ok(())
    }

    async fn cleanup(&self) {
        let deadline = tokio::time::Instant::now() + ACP_CONNECTION_CLEANUP_TIMEOUT;
        {
            // Linearize closure with the last new-session credential commit.
            // A task that reaches finalization after this point observes closed
            // and rolls its newly persisted session back instead of committing.
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
        let prompt_states = std::mem::take(&mut *self.prompt_states.lock().await);
        let mut running_cancellations = Vec::new();
        for state in prompt_states.into_values() {
            match state {
                AcpPromptState::Starting { cancellation } => cancellation.cancel(),
                AcpPromptState::Running {
                    cancellation,
                    run_cancellation,
                } => {
                    cancellation.cancel();
                    running_cancellations.push(run_cancellation);
                    self.stats.active_runs.fetch_sub(1, Ordering::SeqCst);
                }
            }
        }
        // Revoke every capability synchronously before awaiting registry cleanup.
        // No queued or detached task can launch another credential-bearing tool
        // after this barrier returns.
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
            // deadline we preserve the durable row rather than risk deleting
            // work whose admission is still settling.
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
                retired_sessions.push((session_id, session, discard));
                self.stats.active_sessions.fetch_sub(1, Ordering::SeqCst);
            }
        }
        // Take ownership of the current task set before awaiting it. A prompt
        // that is itself in this set may need to publish a retained drain task;
        // leaving an empty shared set lets that path proceed without a mutex
        // self-deadlock. Newly published tasks are collected on the next pass.
        let mut tasks = {
            let mut shared = self.background_tasks.lock().await;
            std::mem::take(&mut *shared)
        };
        for run_cancellation in running_cancellations {
            tasks.spawn(async move {
                let _ = run_cancellation.cancel().await;
            });
        }
        for (_session_id, mut session, discard) in retired_sessions {
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
                    // Session creation and the pre-run prompt path both cross
                    // persistent core state before returning an ID. Aborting
                    // them here could orphan that state. Detaching preserves
                    // their existing closed checks and rollback/cancel paths
                    // while keeping connection shutdown bounded.
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

async fn cancel_maple_permission(responder: &AgentRunPermissionResponder, request_id: &str) {
    if let Err(error) = responder
        .respond(request_id.to_string(), AgentPermissionDecision::Cancel)
        .await
    {
        log::debug!(
            "Maple ACP permission request {request_id} was already resolved while failing closed: {error}"
        );
    }
}

fn retain_cancelled_permission_request<F, T>(response: F, reservation: AcpOutboundReservation)
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(async move {
        let _reservation = reservation;
        let _ = response.await;
    });
}

/// Agent-text notice sent when Goose replaced the history after compaction.
async fn wait_for_retained_terminal(
    terminal: &mut tokio::sync::watch::Receiver<Option<AgentRunTerminal>>,
) {
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
mod tests {
    use super::config::{MAX_ACP_CONNECTIONS, config_path, load_config, save_config};
    use super::convert::{acp_tool_update, client_supports_form_elicitation, timeline_tool_text};
    use super::session::completed_runtime_start;
    use super::transport::is_session_update_line;
    use super::*;
    use crate::agent::{AgentRunUsage, AgentSessionSummary};
    use agent_client_protocol::schema::v1::{
        ClientCapabilities, ElicitationCapabilities, ElicitationFormCapabilities, McpServerStdio,
        SelectedPermissionOutcome,
    };
    use agent_client_protocol::schema::v1::{
        InitializeRequest, McpServer, RequestPermissionOutcome,
    };
    use serde_json::json;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use crate::agent::test_support::started_agent_runtime;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    /// One ACP client speaking the same framed-line transport `serve_stdio`
    /// runs, one layer below the process boundary: identical
    /// `BoundedLineReader`/`LinesCodec`/`tracked_outgoing_lines` wiring on
    /// in-memory duplex pipes instead of stdin/stdout.
    struct StdioAcpClient {
        write: tokio::io::DuplexStream,
        read: BufReader<tokio::io::DuplexStream>,
        next_id: u64,
    }

    impl StdioAcpClient {
        async fn request(&mut self, method: &str, params: serde_json::Value) -> u64 {
            let id = self.next_id;
            self.next_id += 1;
            let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            self.write
                .write_all(line.to_string().as_bytes())
                .await
                .unwrap();
            self.write.write_all(b"\n").await.unwrap();
            self.write.flush().await.unwrap();
            id
        }

        /// Send a notification frame: same transport, no id, no response.
        async fn notification(&mut self, method: &str, params: serde_json::Value) {
            let line = json!({"jsonrpc": "2.0", "method": method, "params": params});
            self.write
                .write_all(line.to_string().as_bytes())
                .await
                .unwrap();
            self.write.write_all(b"\n").await.unwrap();
            self.write.flush().await.unwrap();
        }

        /// The response for `id`, skipping interleaved notifications.
        async fn response(&mut self, id: u64) -> serde_json::Value {
            loop {
                let mut line = String::new();
                let read =
                    tokio::time::timeout(Duration::from_secs(60), self.read.read_line(&mut line))
                        .await
                        .expect("a response should arrive within 60s")
                        .expect("reading the agent's stdout side should not fail");
                assert!(
                    read > 0,
                    "the agent closed the connection before answering id {id}"
                );
                let message: serde_json::Value =
                    serde_json::from_str(line.trim()).expect("every frame should be valid JSON");
                if message.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                    assert!(
                        message.get("error").is_none() || message.get("result").is_none(),
                        "a response carries either a result or an error: {message}"
                    );
                    return message;
                }
            }
        }

        async fn shutdown(mut self) {
            self.write.shutdown().await.unwrap();
        }
    }

    async fn spawn_acp_stdio_serve(
        handle: crate::agent::AgentRuntimeHandle,
    ) -> (
        StdioAcpClient,
        tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    ) {
        let config = Arc::new(RwLock::new(
            normalize_config(AgentAcpConfig::default()).expect("the default ACP config is valid"),
        ));
        let stats = Arc::new(AgentAcpStats::default());
        let context = AcpConnectionContext::new(handle, config, stats, completed_runtime_start());
        let (client_to_agent, agent_read) = tokio::io::duplex(16 * 1024);
        let (agent_to_client, client_read) = tokio::io::duplex(16 * 1024);
        let read = BoundedLineReader::new(agent_read, CancellationToken::new());
        let incoming = FramedRead::new(read, LinesCodec::new_with_max_length(MAX_ACP_FRAME_BYTES))
            .map(|result| {
                result.map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            });
        let outgoing = tracked_outgoing_lines(agent_to_client, Arc::clone(&context.outbound));
        let serving = tokio::spawn(async move {
            AcpAgent
                .builder()
                .name("maple-acp")
                .with_handler(MapleAcpHandler { context })
                .connect_to(Lines::new(outgoing, incoming))
                .await
        });
        (
            StdioAcpClient {
                write: client_to_agent,
                read: BufReader::new(client_read),
                next_id: 0,
            },
            serving,
        )
    }

    async fn finish_acp_stdio_serve(
        serving: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    ) {
        let served = tokio::time::timeout(Duration::from_secs(30), serving)
            .await
            .expect("the serve task should end after client EOF")
            .expect("the serve task should not panic");
        served.expect("serving should complete cleanly after client EOF");
    }

    /// The stdio contract an editor depends on before it can do anything:
    /// `initialize` answers, `session/new` returns the mode list AND the model
    /// list, and the task it persists is loadable by a later connection.
    /// Regression shape: any mismatch between the mode ACP maps onto and the
    /// runtime's external-surface gates turns `session/new` (or `session/load`)
    /// into an error and the editor loses the model list entirely.
    #[tokio::test]
    async fn stdio_surface_returns_modes_models_and_loads_persisted_sessions() {
        let agent = started_agent_runtime("acp-stdio").await;
        let project_root = agent.project_root.to_string_lossy().into_owned();

        let (mut client, serving) = spawn_acp_stdio_serve(agent.handle.clone()).await;
        let id = client
            .request(
                "initialize",
                json!({"protocolVersion": 2, "clientCapabilities": {}}),
            )
            .await;
        let initialize = client.response(id).await;
        assert!(
            initialize.get("error").is_none(),
            "initialize failed: {}",
            initialize["error"]
        );
        assert_eq!(initialize["result"]["agentInfo"]["name"], "maple");

        let id = client
            .request(
                "session/new",
                json!({"cwd": project_root, "mcpServers": [], "additionalDirectories": ["/tmp"]}),
            )
            .await;
        let new_session = client.response(id).await;
        assert!(
            new_session.get("error").is_none(),
            "session/new failed: {}",
            new_session["error"]
        );
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("session/new must return a session id")
            .to_string();
        assert!(!session_id.is_empty());
        assert_eq!(
            new_session["result"]["modes"]["currentModeId"], "interactive",
            "a fresh session must start interactive: {new_session}"
        );
        let mode_ids: Vec<&str> = new_session["result"]["modes"]["availableModes"]
            .as_array()
            .expect("modes must be a list")
            .iter()
            .map(|mode| mode["id"].as_str().expect("mode ids are strings"))
            .collect();
        assert_eq!(mode_ids, vec!["interactive", "approve_all"]);

        let model_option = new_session["result"]["configOptions"]
            .as_array()
            .expect("configOptions must be a list")
            .iter()
            .find(|option| option["id"] == "model")
            .expect("the model selector must be offered")
            .clone();
        let current = model_option["currentValue"]
            .as_str()
            .expect("a current model must be set")
            .to_string();
        assert!(!current.is_empty());
        let offered: Vec<&str> = model_option["options"]
            .as_array()
            .expect("the model selector must list options")
            .iter()
            .map(|option| option["value"].as_str().expect("model ids are strings"))
            .collect();
        assert!(
            offered.contains(&current.as_str()),
            "the current model must come from the offered list: {current:?} in {offered:?}"
        );

        let id = client
            .request(
                "session/set_mode",
                json!({"sessionId": session_id, "modeId": "approve_all"}),
            )
            .await;
        let switched = client.response(id).await;
        assert!(
            switched.get("error").is_none(),
            "approve_all is a mode: {switched}"
        );
        let id = client
            .request(
                "session/set_mode",
                json!({"sessionId": session_id, "modeId": "bogus"}),
            )
            .await;
        let rejected = client.response(id).await;
        assert!(
            rejected.get("error").is_some(),
            "unknown modes must be rejected: {rejected}"
        );

        // Client EOF must end the serve task and release the session lease.
        client.shutdown().await;
        finish_acp_stdio_serve(serving).await;

        // A persisted task must be loadable by a later connection. The ACP
        // task above stays hidden from listing until its first prompt, so load
        // a desktop task saved under the same caller-mediated mode: this only
        // succeeds while the mode ACP persists is one the load path accepts.
        let created = agent
            .handle
            .create_session(Some(crate::agent::AgentCreateSessionRequest {
                project_root: Some(project_root.clone()),
                title: Some("acp load target".to_string()),
                model: None,
                context_limit: None,
                mode: Some("smart_approve".to_string()),
                mcp_server_names: None,
                system_prompt: None,
            }))
            .await
            .expect("a desktop task should be creatable");
        let created_id = created.session.id.clone();
        let (mut client, serving) = spawn_acp_stdio_serve(agent.handle.clone()).await;
        let id = client
            .request(
                "initialize",
                json!({"protocolVersion": 2, "clientCapabilities": {}}),
            )
            .await;
        client.response(id).await;
        let id = client
            .request(
                "session/load",
                json!({"sessionId": created_id, "cwd": project_root, "mcpServers": []}),
            )
            .await;
        let loaded = client.response(id).await;
        assert!(
            loaded.get("error").is_none(),
            "session/load failed: {}",
            loaded["error"]
        );
        assert_eq!(
            loaded["result"]["modes"]["currentModeId"], "interactive",
            "a persisted caller-mediated task loads as interactive: {loaded}"
        );
        client.shutdown().await;
        finish_acp_stdio_serve(serving).await;
    }

    /// The stdio contract for image prompts: an editor may attach an image
    /// no matter what the catalog says about the session model. Vision models
    /// get the image embedded, everyone else gets a read_image reference, and
    /// an unreachable catalog (this runtime's model catalog answers over an
    /// unreachable endpoint) fails closed to the helper instead of rejecting
    /// the prompt. Regression shape: image prompts were either rejected
    /// outright or had their attachments silently dropped by the launch path.
    #[tokio::test]
    async fn stdio_prompt_images_travel_through_the_read_image_helper() {
        let agent = started_agent_runtime("acp-image-prompt").await;
        let project_root = agent.project_root.to_string_lossy().into_owned();

        let (mut client, serving) = spawn_acp_stdio_serve(agent.handle.clone()).await;
        let id = client
            .request(
                "initialize",
                json!({"protocolVersion": 2, "clientCapabilities": {}}),
            )
            .await;
        client.response(id).await;
        let id = client
            .request(
                "session/new",
                json!({"cwd": project_root, "mcpServers": []}),
            )
            .await;
        let new_session = client.response(id).await;
        assert!(
            new_session.get("error").is_none(),
            "session/new failed: {}",
            new_session["error"]
        );
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("session/new must return a session id")
            .to_string();

        let id = client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [
                        {"type": "text", "text": "What do you see?"},
                        {"type": "image", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAELFTkSuQmCC", "mimeType": "image/png"},
                    ],
                }),
            )
            .await;
        // The prompt is accepted and its user message persisted before the
        // model turn, so wait for the persisted attachment instead of the
        // reply: this runtime's model endpoint is unreachable and the turn
        // is settled below by cancelling it.
        let detail = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Ok(detail) = agent.handle.load_session(session_id.clone()).await
                    && detail.timeline.iter().any(|item| {
                        item.input
                            .as_ref()
                            .and_then(|input| input.get("imageAttachments"))
                            .is_some()
                    })
                {
                    return detail;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the persisted user message must reference its image attachment");
        let item = detail
            .timeline
            .iter()
            .find(|item| {
                item.input
                    .as_ref()
                    .and_then(|input| input.get("imageAttachments"))
                    .is_some()
            })
            .expect("the persisted user message must reference its image attachment");
        assert_eq!(item.text.as_deref(), Some("What do you see?"));
        let attachment = item
            .input
            .as_ref()
            .and_then(|input| input.get("imageAttachments"))
            .and_then(|attachments| attachments.as_array())
            .and_then(|attachments| attachments.first())
            .expect("one image attachment must be recorded");
        assert_eq!(attachment["name"], "acp-image-1.png");
        let source = attachment["source"]
            .as_str()
            .expect("attachment sources are strings");
        assert!(
            source.starts_with("maple-attachment://"),
            "a model without catalog vision support must be routed through read_image: {source}"
        );

        client
            .notification("session/cancel", json!({"sessionId": session_id}))
            .await;
        let prompt = client.response(id).await;
        assert!(
            prompt.get("error").is_none(),
            "an image prompt must be accepted, catalog gaps included: {}",
            prompt["error"]
        );
        assert_eq!(prompt["result"]["stopReason"], "cancelled");

        client.shutdown().await;
        finish_acp_stdio_serve(serving).await;
    }

    #[test]
    fn caller_session_fields_come_from_the_raw_session_new_params() {
        let fields = AcpCallerSessionFields::from_params(&json!({
            "cwd": "/tmp/project",
            "mcpServers": [],
            "systemPrompt": "  You are Buzz's Maple persona.  ",
            "_meta": { "sessionTitle": "general" },
        }));
        assert_eq!(
            fields,
            AcpCallerSessionFields {
                system_prompt: Some("You are Buzz's Maple persona.".to_string()),
                session_title: Some("general".to_string()),
            }
        );
        let empty = AcpCallerSessionFields::from_params(&json!({
            "cwd": "/tmp/project",
            "systemPrompt": "   ",
        }));
        assert_eq!(empty, AcpCallerSessionFields::default());
    }

    #[test]
    fn default_config_is_caller_mediated() {
        let config = AgentAcpConfig::default();
        assert_eq!(config.permission_mode, AgentAcpPermissionMode::ReadOnly);
        assert_eq!(config.max_connections, 8);
    }

    #[test]
    fn explicit_connection_limits_remain_configurable_below_the_default() {
        let one = normalize_config(AgentAcpConfig {
            max_connections: 1,
            ..AgentAcpConfig::default()
        })
        .unwrap();
        assert_eq!(one.max_connections, 1);

        let capped = normalize_config(AgentAcpConfig {
            max_connections: usize::MAX,
            ..AgentAcpConfig::default()
        })
        .unwrap();
        assert_eq!(capped.max_connections, MAX_ACP_CONNECTIONS);
    }

    #[test]
    fn session_ids_are_canonicalized_and_empty_ids_are_rejected() {
        assert_eq!(
            canonical_session_id(&SessionId::new("  task-123  ")).unwrap(),
            "task-123"
        );
        assert!(canonical_session_id(&SessionId::new(" \n\t ")).is_err());
    }

    fn session_summary_with_mode(id: &str, mode: &str) -> AgentSessionSummary {
        AgentSessionSummary {
            id: id.to_string(),
            title: id.to_string(),
            project_root: "/tmp/project".to_string(),
            created_ms: 1,
            updated_ms: 1,
            message_count: 0,
            model: Some("model".to_string()),
            mode: mode.to_string(),
            web_enabled: false,
            state: AgentTaskState::Active,
            acp: false,
        }
    }

    #[test]
    fn session_list_loadability_accepts_read_only_and_approve_all_tasks() {
        let sessions = [
            session_summary_with_mode("read-only", "smart_approve"),
            session_summary_with_mode("approve-all", "auto"),
            session_summary_with_mode("approval", "approve"),
            session_summary_with_mode("chat", "chat"),
            session_summary_with_mode("unknown", "future_mode"),
        ];

        let visible = sessions
            .iter()
            .filter(|session| is_acp_loadable_session_mode(&session.mode))
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(visible, vec!["read-only", "approve-all", "approval"]);
    }

    #[test]
    fn session_load_preflight_rejects_unknown_and_missing_tasks() {
        let sessions = [
            session_summary_with_mode("read-only", "smart_approve"),
            session_summary_with_mode("approve-all", "auto"),
        ];

        assert_eq!(
            ensure_acp_session_is_loadable(&sessions, "read-only")
                .unwrap()
                .id,
            "read-only"
        );
        assert_eq!(
            ensure_acp_session_is_loadable(&sessions, "approve-all")
                .unwrap()
                .id,
            "approve-all"
        );
        assert!(
            ensure_acp_session_is_loadable(&sessions, "missing")
                .unwrap_err()
                .contains("does not exist")
        );
    }

    #[tokio::test]
    async fn session_operation_cancellation_keeps_close_behind_the_active_fence() {
        let connection_lifetime = CancellationToken::new();
        let operation = AcpSessionOperation::new(&connection_lifetime);
        let active = Arc::clone(&operation.gate).lock_owned().await;
        let waiting_gate = Arc::clone(&operation.gate);
        let waiting = tokio::spawn(async move { waiting_gate.lock_owned().await });

        operation.cancellation.cancel();
        assert!(operation.cancellation.is_cancelled());
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        drop(active);
        let closing = tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        drop(closing);
    }

    #[test]
    fn timed_out_close_keeps_its_resurrection_fence() {
        assert!(close_registration_may_be_released(true, true, true));
        assert!(!close_registration_may_be_released(false, true, true));
        assert!(!close_registration_may_be_released(true, false, true));
        assert!(!close_registration_may_be_released(true, true, false));
    }

    #[test]
    fn completed_replayed_tool_prefers_result_over_request_summary() {
        let item = AgentTimelineItem {
            id: "tool-1".to_string(),
            item_type: "tool".to_string(),
            role: Some("assistant".to_string()),
            title: Some("Terminal".to_string()),
            text: Some("listing project root".to_string()),
            status: Some("completed".to_string()),
            input: Some(serde_json::json!({ "command": "pwd" })),
            output: Some(serde_json::json!({ "text": "/tmp/project" })),
            created_ms: 1,
            merge: "replace".to_string(),
        };
        let mut projection = AcpToolProjection::default();
        let encoded = serde_json::to_value(acp_tool_update(&item, &mut projection)).unwrap();

        assert_eq!(timeline_tool_text(&item).as_deref(), Some("/tmp/project"));
        assert_eq!(encoded["content"][0]["content"]["text"], "/tmp/project");
        assert_eq!(encoded["rawInput"]["command"], "pwd");
    }

    /// ACP has no subagent of its own, so live subagent work rides the
    /// `delegate` tool call, the way Goose's own ACP server reports it.
    #[test]
    fn subagent_progress_updates_the_delegate_tool_call() {
        let encoded = serde_json::to_value(subagent_tool_update(
            "delegate-1",
            "Terminal: cargo test".to_string(),
        ))
        .unwrap();

        assert_eq!(encoded["sessionUpdate"], "tool_call_update");
        assert_eq!(encoded["toolCallId"], "delegate-1");
        assert_eq!(encoded["status"], "in_progress");
        assert_eq!(
            encoded["content"][0]["content"]["text"],
            "Terminal: cargo test"
        );
    }

    #[test]
    fn slash_commands_parse_like_the_desktop_composer() {
        assert_eq!(
            parse_slash_command("/compact"),
            Some(("compact".to_string(), "".to_string()))
        );
        assert_eq!(
            parse_slash_command("  /skill-name some args  "),
            Some(("skill-name".to_string(), "some args".to_string()))
        );
        assert_eq!(parse_slash_command("plain text"), None);
        assert_eq!(parse_slash_command("/"), None);
        assert_eq!(parse_slash_command("/a/b nested"), None);
        assert_eq!(parse_slash_command("not/leading"), None);
    }

    #[test]
    fn available_commands_lead_with_the_built_ins_plus_skills() {
        let encoded =
            serde_json::to_value(acp_available_commands(&[crate::agent::AgentSlashCommand {
                name: "buzz-worklog".to_string(),
                description: "Publish a coding update".to_string(),
                input_hint: None,
            }]))
            .unwrap();
        assert_eq!(encoded["sessionUpdate"], "available_commands_update");
        let commands = encoded["availableCommands"].as_array().unwrap();
        assert_eq!(commands[0]["name"], "compact");
        assert_eq!(commands[1]["name"], "buzz-worklog");
        assert_eq!(commands[1]["description"], "Publish a coding update");
    }

    #[test]
    fn failed_replayed_tool_preserves_result_and_failure_badge_message() {
        let mut projection = AcpToolProjection::default();
        let pending = AgentTimelineItem {
            id: "tool-1".to_string(),
            item_type: "tool".to_string(),
            role: Some("assistant".to_string()),
            title: Some("Terminal".to_string()),
            text: Some("running command".to_string()),
            status: Some("pending".to_string()),
            input: Some(serde_json::json!({ "command": "false" })),
            output: None,
            created_ms: 1,
            merge: "replace".to_string(),
        };
        let _ = acp_tool_update(&pending, &mut projection);
        let failed = AgentTimelineItem {
            text: Some("running command".to_string()),
            status: Some("failed".to_string()),
            output: Some(serde_json::json!({
                "text": "command exited with status 1",
                "isError": true,
            })),
            ..pending
        };
        let encoded = serde_json::to_value(acp_tool_update(&failed, &mut projection)).unwrap();

        assert_eq!(encoded["sessionUpdate"], "tool_call_update");
        assert_eq!(
            encoded["content"][0]["content"]["text"],
            "command exited with status 1"
        );
        assert_eq!(
            encoded["rawOutput"]["message"],
            "command exited with status 1"
        );
    }

    #[test]
    fn usage_update_carries_context_tokens_and_window() {
        let encoded = serde_json::to_value(SessionUpdate::UsageUpdate(UsageUpdate::new(
            53_000, 200_000,
        )))
        .unwrap();
        assert_eq!(encoded["sessionUpdate"], "usage_update");
        assert_eq!(encoded["used"], 53_000);
        assert_eq!(encoded["size"], 200_000);
        assert!(encoded.get("cost").is_none());
    }

    #[test]
    fn prompt_usage_serializes_one_turn_without_session_accumulation() {
        let turn = AgentRunUsage {
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            cached_read_tokens: 3,
            cached_write_tokens: 1,
        };
        let encoded = serde_json::to_value(acp_usage(turn)).unwrap();

        assert_eq!(encoded["inputTokens"], 10);
        assert_eq!(encoded["outputTokens"], 4);
        assert_eq!(encoded["totalTokens"], 14);
        assert_eq!(encoded["cachedReadTokens"], 3);
        assert_eq!(encoded["cachedWriteTokens"], 1);
    }

    #[test]
    fn model_selector_locks_to_the_persisted_model_after_first_message() {
        let models = vec!["model-a".to_string(), "model-b".to_string()];
        let fresh = serde_json::to_value(acp_session_config_options(
            "model-b",
            &models,
            0,
            AcpSessionMode::Interactive,
        ))
        .expect("fresh model options should serialize");
        let locked = serde_json::to_value(acp_session_config_options(
            "model-b",
            &models,
            1,
            AcpSessionMode::ApproveAll,
        ))
        .expect("locked model options should serialize");

        assert_eq!(fresh[0]["currentValue"], "model-b");
        assert_eq!(fresh[0]["options"].as_array().unwrap().len(), 2);
        assert_eq!(fresh[1]["currentValue"], "interactive");
        assert_eq!(locked[0]["currentValue"], "model-b");
        assert_eq!(locked[0]["options"].as_array().unwrap().len(), 1);
        assert_eq!(locked[0]["options"][0]["value"], "model-b");
        assert_eq!(locked[1]["currentValue"], "approve_all");
        assert_eq!(locked[1]["options"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn streamed_message_chunks_keep_the_timeline_item_id() {
        let mut projection = AcpToolProjection::default();
        for (role, item_type, expected_variant) in [
            (Some("user"), "message", "user_message_chunk"),
            (Some("assistant"), "message", "agent_message_chunk"),
            (None, "thinking", "agent_thought_chunk"),
        ] {
            let item = AgentTimelineItem {
                id: format!("stable-{expected_variant}"),
                item_type: item_type.to_string(),
                role: role.map(str::to_string),
                title: None,
                text: Some("delta".to_string()),
                status: None,
                input: None,
                output: None,
                created_ms: 1,
                merge: "append".to_string(),
            };
            let update = timeline_update(&item, &mut projection, true).unwrap();
            let encoded = serde_json::to_value(update).unwrap();
            assert_eq!(encoded["sessionUpdate"], expected_variant);
            assert_eq!(encoded["messageId"], item.id);
        }
    }

    #[test]
    fn compaction_notices_reach_the_client_as_agent_text() {
        let mut projection = AcpToolProjection::default();
        let system_item = |text: &str| AgentTimelineItem {
            id: "system-1".to_string(),
            item_type: "system".to_string(),
            role: Some("system".to_string()),
            title: Some("Progress".to_string()),
            text: Some(text.to_string()),
            status: None,
            input: None,
            output: None,
            created_ms: 1,
            merge: "replace".to_string(),
        };
        let update = timeline_update(
            &system_item("goose is compacting the conversation..."),
            &mut projection,
            false,
        )
        .unwrap();
        let encoded = serde_json::to_value(update).unwrap();
        assert_eq!(encoded["sessionUpdate"], "agent_message_chunk");
        assert_eq!(encoded["content"]["text"], "Compacting…\n");
        assert_eq!(encoded["messageId"], "system-1");
        // Other runtime notices stay out of the ACP stream.
        assert!(timeline_update(&system_item("Thinking hard"), &mut projection, false).is_none());
    }

    #[test]
    fn legacy_allow_all_cannot_bypass_the_acp_caller() {
        // The stale Desktop-owned bypass stays readable only so old files
        // keep loading; it can no longer influence a session's mode.
        let migrated = normalize_config(AgentAcpConfig {
            permission_mode: AgentAcpPermissionMode::AllowAll,
            ..AgentAcpConfig::default()
        })
        .unwrap();
        assert_eq!(migrated.permission_mode, AgentAcpPermissionMode::ReadOnly);
    }

    #[test]
    fn permission_request_exposes_only_one_shot_caller_choices() {
        let request = AgentPermissionRequest {
            request_id: "request-1".to_string(),
            tool_name: "developer__shell".to_string(),
            arguments: serde_json::Map::from_iter([(
                "command".to_string(),
                serde_json::json!("git push"),
            )]),
            prompt: Some("Push this branch?".to_string()),
        };
        let item = AgentTimelineItem {
            id: "permission-request-1".to_string(),
            item_type: "permission".to_string(),
            role: Some("system".to_string()),
            title: Some("Push branch".to_string()),
            text: request.prompt.clone(),
            status: Some("pending".to_string()),
            input: Some(serde_json::Value::Object(request.arguments.clone())),
            output: None,
            created_ms: 1,
            merge: "replace".to_string(),
        };
        let permission = RequestPermissionRequest::new(
            "session-1",
            acp_permission_tool_call(&request, &item).into(),
            acp_permission_options(),
        );
        let encoded = serde_json::to_value(permission).unwrap();

        assert_eq!(encoded["toolCall"]["toolCallId"], "request-1");
        assert_eq!(encoded["toolCall"]["title"], "Push branch");
        assert_eq!(encoded["toolCall"]["kind"], "execute");
        assert_eq!(
            encoded["toolCall"]["content"][0]["content"]["text"],
            "Push this branch?"
        );
        assert_eq!(encoded["toolCall"]["status"], "pending");
        assert_eq!(encoded["toolCall"]["rawInput"]["command"], "git push");
        assert_eq!(
            encoded["options"],
            serde_json::json!([
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "reject_once", "name": "Reject once", "kind": "reject_once" }
            ])
        );
    }

    #[test]
    fn permission_request_without_prompt_previews_the_arguments() {
        let path = std::env::temp_dir().join("notes.md");
        assert!(path.is_absolute(), "the approval fixture must be absolute");
        let path = path.to_string_lossy().into_owned();
        let title = format!("edit: {path}");
        let request = AgentPermissionRequest {
            request_id: "request-2".to_string(),
            tool_name: "edit".to_string(),
            arguments: serde_json::Map::from_iter([
                ("path".to_string(), serde_json::json!(path)),
                (
                    "edits".to_string(),
                    serde_json::json!([{ "oldText": "foo", "newText": "bar" }]),
                ),
            ]),
            prompt: None,
        };
        let item = AgentTimelineItem {
            id: "permission-request-2".to_string(),
            item_type: "permission".to_string(),
            role: Some("system".to_string()),
            title: Some(title.clone()),
            text: None,
            status: Some("pending".to_string()),
            input: Some(serde_json::Value::Object(request.arguments.clone())),
            output: None,
            created_ms: 1,
            merge: "replace".to_string(),
        };
        let permission = RequestPermissionRequest::new(
            "session-1",
            acp_permission_tool_call(&request, &item).into(),
            acp_permission_options(),
        );
        let encoded = serde_json::to_value(permission).unwrap();
        assert_eq!(encoded["toolCall"]["title"], title);
        // Edit approvals render as diffs, the shape ACP clients show inline.
        assert_eq!(encoded["toolCall"]["content"][0]["type"], "diff");
        assert_eq!(encoded["toolCall"]["content"][0]["path"], path);
        assert_eq!(encoded["toolCall"]["content"][0]["oldText"], "foo");
        assert_eq!(encoded["toolCall"]["content"][0]["newText"], "bar");
        // The card links to the file it approves.
        assert_eq!(encoded["toolCall"]["locations"][0]["path"], path);

        // One approval card must not flood the caller with a huge edit.
        let long = "x".repeat(20_000);
        let long_request = AgentPermissionRequest {
            request_id: "request-3".to_string(),
            tool_name: "edit".to_string(),
            arguments: serde_json::Map::from_iter([
                ("path".to_string(), serde_json::json!(path)),
                (
                    "edits".to_string(),
                    serde_json::json!([{ "newText": long }]),
                ),
            ]),
            prompt: None,
        };
        let bounded = serde_json::to_value(acp_permission_tool_call(&long_request, &item)).unwrap();
        let bounded_text = bounded["content"][0]["newText"]
            .as_str()
            .expect("the bounded diff must stay attached");
        assert!(
            bounded_text.chars().count() <= 501,
            "diff text must be bounded, got {}",
            bounded_text.chars().count()
        );
    }

    #[test]
    fn project_trust_chooser_is_fail_closed_and_cannot_be_auto_accepted() {
        let encoded = serde_json::to_value(project_trust_permission_options()).unwrap();

        assert_eq!(
            encoded,
            serde_json::json!([
                {
                    "optionId": "keep_untrusted",
                    "name": "Keep project trust disabled",
                    "kind": "allow_once"
                },
                {
                    "optionId": "trust_project",
                    "name": "Trust this project",
                    "kind": "allow_once"
                },
                { "optionId": "cancel", "name": "Cancel turn", "kind": "reject_once" }
            ])
        );
        assert_eq!(
            project_trust_permission_decision(&RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new("keep_untrusted")
            )),
            Ok(Some(false))
        );
        assert_eq!(
            project_trust_permission_decision(&RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new("trust_project")
            )),
            Ok(Some(true))
        );
        assert_eq!(
            project_trust_permission_decision(&RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new("cancel")
            )),
            Ok(None)
        );
        assert_eq!(
            project_trust_permission_decision(&RequestPermissionOutcome::Cancelled),
            Ok(None)
        );
        assert!(
            project_trust_permission_decision(&RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new("allow_always")
            ))
            .is_err()
        );
    }

    #[test]
    fn project_trust_elicitation_is_session_scoped_reversible_and_defaults_off() {
        let request = project_trust_elicitation_request(
            SessionId::new("session-1"),
            Path::new("/tmp/maple-project"),
        );
        let encoded = serde_json::to_value(request).unwrap();

        assert_eq!(encoded["mode"], "form");
        assert_eq!(encoded["sessionId"], "session-1");
        assert_eq!(
            encoded["requestedSchema"]["required"],
            json!(["trustProject"])
        );
        assert_eq!(
            encoded["requestedSchema"]["properties"]["trustProject"]["type"],
            "boolean"
        );
        assert_eq!(
            encoded["requestedSchema"]["properties"]["trustProject"]["default"],
            false
        );
        assert!(
            encoded["message"]
                .as_str()
                .unwrap()
                .contains("Normal tool permissions still apply")
        );
    }

    #[test]
    fn form_elicitation_is_used_only_when_the_client_advertises_it() {
        let plain = InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1);
        assert!(!client_supports_form_elicitation(&plain));

        let mut form = InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1);
        form.client_capabilities = ClientCapabilities::new()
            .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()));
        assert!(client_supports_form_elicitation(&form));
    }

    #[test]
    fn permission_outcomes_map_fail_closed() {
        assert_eq!(
            acp_permission_decision(&RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new("allow_once")
            )),
            Ok(AgentPermissionDecision::AllowOnce)
        );
        assert_eq!(
            acp_permission_decision(&RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new("reject_once")
            )),
            Ok(AgentPermissionDecision::DenyOnce)
        );
        assert_eq!(
            acp_permission_decision(&RequestPermissionOutcome::Cancelled),
            Ok(AgentPermissionDecision::Cancel)
        );
        assert!(
            acp_permission_decision(&RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new("allow_always")
            ))
            .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn outbound_tracker_releases_credit_only_after_a_socket_write_acknowledgement() {
        use futures_util::SinkExt as _;
        use tokio::io::AsyncReadExt as _;

        let tracker = AcpOutboundTracker::with_limits(1, 1024);
        let cancellation = CancellationToken::new();
        let first = tracker.reserve(1, &cancellation).await.unwrap();
        tracker
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(first);

        let waiting_tracker = Arc::clone(&tracker);
        let waiting_cancellation = cancellation.clone();
        let waiting =
            tokio::spawn(async move { waiting_tracker.reserve(1, &waiting_cancellation).await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#;
        let (writer, mut reader) = tokio::io::duplex(1024);
        let mut sink = Box::pin(tracked_outgoing_lines(writer, Arc::clone(&tracker)));
        sink.send(line.to_string()).await.unwrap();
        let mut written = vec![0_u8; line.len() + 1];
        reader.read_exact(&mut written).await.unwrap();
        assert_eq!(written, format!("{line}\n").into_bytes());

        let second = waiting.await.unwrap().unwrap();
        drop(second);
    }

    #[tokio::test]
    async fn cancelled_permission_retains_credit_until_the_orphan_request_settles() {
        let tracker = AcpOutboundTracker::with_limits(1, 1024);
        let cancellation = CancellationToken::new();
        let first = tracker.reserve(1, &cancellation).await.unwrap();
        let (settled_tx, settled_rx) = tokio::sync::oneshot::channel::<()>();
        retain_cancelled_permission_request(
            async move {
                let _ = settled_rx.await;
            },
            first,
        );

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                tracker.reserve(1, &cancellation),
            )
            .await
            .is_err()
        );

        settled_tx.send(()).unwrap();
        let second = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tracker.reserve(1, &cancellation),
        )
        .await
        .unwrap()
        .unwrap();
        drop(second);
    }

    #[test]
    fn outbound_credit_acknowledges_only_session_updates() {
        assert!(is_session_update_line(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#
        ));
        assert!(!is_session_update_line(
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#
        ));
    }

    #[test]
    fn allowed_project_root_returns_the_canonical_admitted_path() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();

        let admitted =
            ensure_allowed_project_root(&project, &[root.path().to_string_lossy().into_owned()])
                .unwrap();

        assert_eq!(admitted, project.canonicalize().unwrap());
    }

    #[test]
    fn allowed_project_root_rejects_relative_paths() {
        assert!(ensure_allowed_project_root(Path::new("relative/project"), &[]).is_err());
    }

    #[test]
    fn a_config_written_before_the_enabled_flag_was_dropped_still_loads() {
        let root = tempfile::tempdir().unwrap();
        let user_id = "acp-legacy-user";
        let path = config_path(root.path(), user_id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            br#"{"enabled":true,"permissionMode":"read_only","allowedProjectRoots":[],"maxConnections":4}"#,
        )
        .unwrap();

        let config = load_config(root.path(), user_id).unwrap();
        assert_eq!(config.max_connections, 4);
        assert_eq!(config.permission_mode, AgentAcpPermissionMode::ReadOnly);
    }

    #[test]
    fn save_config_writes_atomically_and_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let user_id = "acp-config-user";
        let project_root = root.path().join("project");
        assert!(project_root.is_absolute());
        let config = AgentAcpConfig {
            allowed_project_roots: vec![project_root.to_string_lossy().into_owned()],
            ..AgentAcpConfig::default()
        };

        save_config(root.path(), user_id, &config).unwrap();
        // Overwrite once more: the replacement must not leave a temp file.
        save_config(root.path(), user_id, &config).unwrap();

        let path = config_path(root.path(), user_id).unwrap();
        let leftovers = path
            .parent()
            .unwrap()
            .read_dir()
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert_eq!(load_config(root.path(), user_id).unwrap(), config);
    }

    #[test]
    fn bridge_environment_is_strictly_allowlisted() {
        let filtered = filter_bridge_environment(HashMap::from([
            (
                "BUZZ_RELAY_URL".to_string(),
                "ws://localhost:3000".to_string(),
            ),
            ("UNRELATED_SECRET".to_string(), "nope".to_string()),
        ]));
        assert_eq!(filtered.len(), 1);
        assert!(filtered.contains_key("BUZZ_RELAY_URL"));
    }

    #[test]
    fn arbitrary_stdio_mcp_is_rejected_before_any_process_can_start() {
        let server = McpServer::Stdio(
            McpServerStdio::new("untrusted", "/bin/sh")
                .args(vec!["-c".to_string(), "exit 0".to_string()]),
        );

        assert!(prepare_session_mcp(&HashMap::new(), &[server]).is_err());
    }

    #[test]
    fn prompt_blocks_preserve_buzz_order() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("[Base]\nbase")),
            ContentBlock::Text(TextContent::new("[System]\nsystem")),
        ];
        assert_eq!(
            prompt_text(&blocks).unwrap(),
            "[Base]\nbase\n\n[System]\nsystem"
        );
    }

    #[test]
    fn retained_terminal_results_preserve_all_stop_states() {
        let completed = prompt_result_from_terminal(AgentRunTerminal::Completed).unwrap();
        assert_eq!(
            serde_json::to_value(completed).unwrap()["stopReason"],
            "end_turn"
        );

        let cancelled = prompt_result_from_terminal(AgentRunTerminal::Cancelled).unwrap();
        assert_eq!(
            serde_json::to_value(cancelled).unwrap()["stopReason"],
            "cancelled"
        );

        let failed = prompt_result_from_terminal(AgentRunTerminal::Failed).unwrap();
        assert_eq!(
            serde_json::to_value(failed).unwrap()["stopReason"],
            "end_turn"
        );
    }
}
