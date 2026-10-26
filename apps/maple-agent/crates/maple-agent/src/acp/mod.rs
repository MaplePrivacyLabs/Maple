//! The Agent Client Protocol server (`maple-agent acp`): an editor drives
//! Maple's agent over stdio.
//!
//! Each ACP session is one Maple task the connection holds with a lease
//! (`AgentSurfaceLease`): its runs answer the caller alone, and the
//! desktop's own tools stay out of it.
//! `session/new` creates an ACP task, which no list shows until its first
//! prompt; `session/load` holds an existing task and replays its history.
//! Maple has no session modes: every tool call runs without asking.

mod config;
mod convert;
mod handler;
mod observer;
mod paged;
mod session;
mod shared;
mod transport;

pub use config::{AgentAcpConfig, load_acp_config, save_acp_config};
use config::{AgentAcpStats, normalize_config};
use convert::{
    AcpProjection, COMPACTING_NOTICE, COMPACTION_COMPLETED_NOTICE, MAX_ACP_ERROR_CHARS,
    NOTHING_TO_COMPACT_NOTICE, acp_available_commands, acp_usage, bounded_chars,
    checked_timeline_update, event_error_text, internal_acp_error, outbound_error,
    parse_slash_command, project_trust_elicitation_request, project_trust_permission_decision,
    project_trust_permission_options, prompt_images, prompt_result_from_terminal, prompt_text,
    text_chunk, timeline_update,
};
use handler::MapleAcpHandler;
#[cfg(test)]
use session::find_acp_session;
use session::{
    AcpConnectionContext, AcpProjectTrustResolution, AcpPromptState, AcpSession,
    AcpSessionOperation, UnpublishedAcpSession, canonical_session_id, canonical_session_id_text,
    ensure_allowed_project_root, prepare_session_mcp,
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
    CancelNotification, ConfigOptionUpdate, ContentBlock, DeleteSessionRequest,
    DeleteSessionResponse, ElicitationAction, ElicitationContentValue, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, RequestPermissionRequest, SessionId,
    SessionInfo, SessionInfoUpdate, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse, StopReason, TextContent, ToolCall, ToolCallContent, ToolCallStatus,
    ToolKind, UsageUpdate,
};
use agent_client_protocol::{Agent as AcpAgent, Client, ConnectionTo, Lines};
use futures_util::StreamExt as _;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, RwLock};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

type StartedSessionOperation = (
    Arc<AcpSessionOperation>,
    tokio::sync::OwnedMutexGuard<()>,
    tokio::sync::OwnedMutexGuard<()>,
);

const ACP_CONNECTION_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
/// every open session is cleaned up and the call returns.
pub async fn serve_stdio(
    agent: AgentRuntimeHandle,
    config: AgentAcpConfig,
    runtime_start: SharedRuntimeStart,
) -> Result<(), String> {
    let config = Arc::new(RwLock::new(normalize_config(config)?));
    let stats = Arc::new(AgentAcpStats::default());
    let context = AcpConnectionContext::new(agent, config, stats, runtime_start);
    serve(context, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serve an ACP attachment to a shared host. The connection owns only its
/// subscriptions and temporary client resources, never the host runtime.
pub async fn serve_connection<R, W>(
    agent: AgentRuntimeHandle,
    config: AgentAcpConfig,
    runtime_start: SharedRuntimeStart,
    input: R,
    output: W,
) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let config = Arc::new(RwLock::new(normalize_config(config)?));
    let context = AcpConnectionContext::new(
        agent,
        config,
        Arc::new(AgentAcpStats::default()),
        runtime_start,
    );
    serve(context, input, output).await
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
        _ = context.lifetime.cancelled() => Ok(()),
    };
    context.cleanup().await;
    result
}

/// A prompt the connection admitted, in order with the client's other
/// messages, and runs in the background.
pub(super) struct PromptAdmission {
    pub(super) session_id: String,
    prompt: String,
    images: PromptAttachments,
    cancellation: CancellationToken,
    operation: Arc<AcpSessionOperation>,
}

enum PromptAttachments {
    Images(Vec<AgentImageUpload>),
    Committed(Vec<String>),
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
            sessions: Mutex::new(HashMap::new()),
            session_operations: Mutex::new(HashMap::new()),
            observer_slots: Arc::new(tokio::sync::Semaphore::new(
                observer::MAX_ACP_SESSION_OPERATIONS,
            )),
            elicitation_slots: Arc::new(tokio::sync::Semaphore::new(
                observer::MAX_ACP_SESSION_OPERATIONS,
            )),
            #[cfg(test)]
            observer_dispatches: std::sync::atomic::AtomicUsize::new(0),
            prompt_states: Mutex::new(HashMap::new()),
            owned_runs: Mutex::new(HashMap::new()),
            observing: AtomicBool::new(false),
            eliciting: Mutex::new(std::collections::HashSet::new()),
            question_responder: Mutex::new(None),
            background_tasks: Mutex::new(tokio::task::JoinSet::new()),
            finalization: Mutex::new(()),
            lifetime: CancellationToken::new(),
            closed: AtomicBool::new(false),
            client_supports_form_elicitation: AtomicBool::new(false),
            outbound: AcpOutboundTracker::new(),
        })
    }

    async fn new_session(
        self: &Arc<Self>,
        cx: &ConnectionTo<Client>,
        request: NewSessionRequest,
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
        let session_servers = prepare_session_mcp(&request.mcp_servers)?;
        let created = self
            .agent
            .create_surface_session(
                AgentCreateSessionRequest {
                    project_root: Some(project_root.to_string_lossy().into_owned()),
                    title: None,
                    model: Some(model.clone()),
                    context_limit: None,
                    mcp_server_names: None,
                },
                session_servers,
            )
            .await
            .map_err(internal_acp_error)?;
        // Own the lease before anything can fail, so an early return
        // discards the new task instead of leaking it.
        let unpublished = UnpublishedAcpSession::new(created.lease);
        let session_id = canonical_session_id_text(&created.detail.session.id)?;
        let (operation, operation_guard, replay_guard) =
            self.start_session_operation(cx, &session_id).await?;
        let finalization = self.finalization.lock().await;
        if self.closed.load(Ordering::SeqCst) || operation.cancellation.is_cancelled() {
            drop(finalization);
            self.remove_session_operation_if_same(&session_id, &operation)
                .await;
            drop(unpublished);
            return Err(agent_client_protocol::Error::internal_error()
                .data("The Maple ACP connection closed while configuring the session"));
        }
        let mut sessions = self.sessions.lock().await;
        let operations = self.session_operations.lock().await;
        if sessions.contains_key(&session_id)
            || !operations
                .get(&session_id)
                .is_some_and(|registered| Arc::ptr_eq(registered, &operation))
        {
            drop(operations);
            drop(sessions);
            drop(finalization);
            self.remove_session_operation_if_same(&session_id, &operation)
                .await;
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
                thinking: None,
                projection: AcpProjection::default(),
                advertised_title: Some(created.detail.session.title.clone()),
                message_count: created.detail.session.message_count,
                created_here: true,
                prompted: false,
                project_root: project_root.clone(),
                project_trust_decision: project_trust.decision,
            },
        );
        drop(operations);
        drop(sessions);
        self.stats.active_sessions.fetch_add(1, Ordering::SeqCst);
        drop(finalization);
        operation.replaying.store(false, Ordering::Release);
        drop(replay_guard);
        drop(operation_guard);
        self.send_available_commands(cx, &session_id, &project_root)
            .await;
        let options = self.refresh_config_options(&session_id).await?;
        Ok(NewSessionResponse::new(session_id).config_options(options))
    }

    async fn retire_session(&self, session_id: &str) {
        let operation = self
            .session_operations
            .lock()
            .await
            .get(session_id)
            .cloned();
        if let Some(operation) = operation {
            self.retire_session_if_same(session_id, &operation).await;
        }
    }

    async fn session_operation_is_current(
        &self,
        session_id: &str,
        operation: &Arc<AcpSessionOperation>,
    ) -> bool {
        self.session_operations
            .lock()
            .await
            .get(session_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, operation))
    }

    async fn retire_session_if_same(&self, session_id: &str, operation: &Arc<AcpSessionOperation>) {
        operation.cancellation.cancel();
        let session = {
            let _finalization = self.finalization.lock().await;
            let mut sessions = self.sessions.lock().await;
            let mut operations = self.session_operations.lock().await;
            if !operations
                .get(session_id)
                .is_some_and(|registered| Arc::ptr_eq(registered, operation))
            {
                return;
            }
            operations.remove(session_id);
            let session = sessions.remove(session_id);
            if session.is_some() {
                self.stats.active_sessions.fetch_sub(1, Ordering::SeqCst);
            }
            session
        };
        if let Some(mut session) = session
            && let Some(lease) = session.lease.take()
        {
            lease.release().await;
        }
    }

    /// Reserve one bounded worker before publishing the operation. Both gates
    /// are acquired before registration, so pushed events cannot overtake the
    /// initial snapshot or disappear before the session enters `sessions`.
    async fn start_session_operation(
        self: &Arc<Self>,
        cx: &ConnectionTo<Client>,
        session_id: &str,
    ) -> Result<StartedSessionOperation, agent_client_protocol::Error> {
        let slot = Arc::clone(&self.observer_slots)
            .try_acquire_owned()
            .map_err(|_| {
                agent_client_protocol::Error::invalid_request()
                    .data("This ACP connection has reached its session limit")
            })?;
        let operation = AcpSessionOperation::new(&self.lifetime);
        let operation_guard = Arc::clone(&operation.gate).lock_owned().await;
        let replay_guard = Arc::clone(&operation.replay_gate).lock_owned().await;
        let _finalization = self.finalization.lock().await;
        if self.closed.load(Ordering::SeqCst) || self.lifetime.is_cancelled() {
            return Err(agent_client_protocol::Error::internal_error()
                .data("The Maple ACP connection is closing"));
        }
        let sessions = self.sessions.lock().await;
        let mut operations = self.session_operations.lock().await;
        if sessions.contains_key(session_id) || operations.contains_key(session_id) {
            return Err(agent_client_protocol::Error::invalid_request()
                .data("This ACP connection already owns or is loading the requested session"));
        }
        operations.insert(session_id.to_owned(), operation.clone());
        drop(operations);
        drop(sessions);
        let mut tasks = self.background_tasks.lock().await;
        while tasks.try_join_next().is_some() {}
        self.spawn_session_observer(
            &mut tasks,
            cx.clone(),
            session_id.to_owned(),
            operation.clone(),
            slot,
        );
        Ok((operation, operation_guard, replay_guard))
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
        operation.cancellation.cancel();
        let mut operations = self.session_operations.lock().await;
        if operations
            .get(session_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, operation))
        {
            operations.remove(session_id);
        }
    }

    async fn load_session(
        self: &Arc<Self>,
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
        let persisted = self
            .agent
            .task_summary(session_id.clone())
            .await
            .map_err(internal_acp_error)?;
        let persisted_root = ensure_allowed_project_root(
            Path::new(&persisted.project_root),
            &config.allowed_project_roots,
        )
        .map_err(|_| {
            agent_client_protocol::Error::invalid_request()
                .data("The requested task is outside the configured project roots")
        })?;
        if persisted_root != project_root {
            return Err(agent_client_protocol::Error::invalid_request()
                .data("The requested task does not exist in the supplied project directory"));
        }
        let available_models = self.available_models().await?;
        if let Some(model) = persisted.model.as_ref()
            && !available_models.iter().any(|available| available == model)
        {
            return Err(agent_client_protocol::Error::invalid_request().data(format!(
                "This Maple Work task uses model '{model}', which is no longer available; the task remains available in Maple Desktop"
            )));
        }
        let session_servers = prepare_session_mcp(&request.mcp_servers)?;
        let protocol_session_id = SessionId::new(session_id.clone());
        let (operation, operation_guard, replay_guard) =
            self.start_session_operation(cx, &session_id).await?;
        let attached = match self
            .agent
            .attach_surface_session_paged(&session_id, session_servers)
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
        if Path::new(&attached.detail.session.project_root) != project_root.as_path() {
            lease.release().await;
            self.remove_session_operation_if_same(&session_id, &operation)
                .await;
            return Err(agent_client_protocol::Error::invalid_request()
                .data("The task's project changed while loading"));
        }
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
        let message_count = attached.detail.session.message_count;
        let finalization = self.finalization.lock().await;
        if self.closed.load(Ordering::SeqCst) || operation.cancellation.is_cancelled() {
            drop(finalization);
            lease.release().await;
            self.remove_session_operation_if_same(&session_id, &operation)
                .await;
            return Err(agent_client_protocol::Error::internal_error().data(
                operation
                    .failure_message()
                    .unwrap_or("The Maple ACP session closed while it was loading"),
            ));
        }
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&session_id)
            || !self
                .session_operation_is_current(&session_id, &operation)
                .await
        {
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
                thinking: None,
                projection: AcpProjection::default(),
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

        let projection = match self
            .replay_paged_history(
                cx,
                &protocol_session_id,
                &session_id,
                &operation.cancellation,
            )
            .await
        {
            Ok(projection) => projection,
            Err(error) => {
                drop(replay_guard);
                drop(operation_guard);
                self.retire_session_if_same(&session_id, &operation).await;
                return Err(operation
                    .failure_message()
                    .map(paged::replay_error)
                    .unwrap_or(error));
            }
        };
        if operation.cancellation.is_cancelled() {
            drop(replay_guard);
            drop(operation_guard);
            self.retire_session_if_same(&session_id, &operation).await;
            return Err(paged::replay_error(
                operation
                    .failure_message()
                    .unwrap_or("Maple ACP history loading was cancelled"),
            ));
        }
        if let Some(session) = self.sessions.lock().await.get_mut(&session_id) {
            session.projection = projection;
        }
        operation.replaying.store(false, Ordering::Release);
        drop(replay_guard);
        drop(operation_guard);
        for question in self.agent.pending_questions().await {
            self.observe(cx, question).await.map_err(outbound_error)?;
        }
        self.send_available_commands(cx, &session_id, &project_root)
            .await;
        Ok(LoadSessionResponse::new()
            .config_options(self.refresh_config_options(&session_id).await?))
    }

    async fn replay_paged_history(
        &self,
        cx: &ConnectionTo<Client>,
        protocol_session: &SessionId,
        task: &str,
        cancellation: &CancellationToken,
    ) -> Result<AcpProjection, agent_client_protocol::Error> {
        self.agent
            .ensure_task_history_ready(task)
            .await
            .map_err(internal_acp_error)?;
        let mut captured = None;
        for _ in 0..4 {
            let initial = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(paged::replay_error("Maple ACP history loading was cancelled")),
                page = self.agent.live_task_page(task,usize::from(paged::ACP_HISTORY_ROWS),paged::ACP_HISTORY_BYTES) => page.map_err(internal_acp_error)?,
            };
            if initial.has_more {
                return Err(paged::replay_error(
                    "The running task has more live history than ACP can replay; load it after the turn finishes",
                ));
            }
            let (account, store) = self
                .agent
                .history_replay_source()
                .map_err(internal_acp_error)?;
            let expected = crate::store::TaskHistoryStamp {
                rows_epoch: initial.stored_marker.0,
                last_seq: initial.stored_marker.1,
            };
            match paged::ReplayHistory::capture(
                &self.agent,
                store,
                account,
                task,
                expected,
                cancellation,
            )
            .await
            {
                Ok(history) => {
                    captured = Some((initial, history));
                    break;
                }
                // A source commit raced the bounded live snapshot, before the
                // capture transaction began. Retry before emitting anything.
                Err(crate::store::StoreError::Conflict) => continue,
                Err(error) => return Err(paged::history_error(error, cancellation)),
            }
        }
        let (initial, history) = captured.ok_or_else(|| {
            paged::replay_error(
                "The task is changing too quickly to capture its history; try loading it again",
            )
        })?;
        let active = initial
            .items
            .iter()
            .filter_map(|item| item.persisted.as_ref())
            .map(|row| paged::replay_row_id(&row.task_id, row.rows_epoch, row.row_seq))
            .collect::<std::collections::HashSet<_>>();
        let mut projection = AcpProjection::default();
        let mut replayed_through = 0;
        loop {
            self.agent
                .verify_generation()
                .await
                .map_err(internal_acp_error)?;
            let Some((mut item, identity)) = history
                .next_row(cancellation)
                .await
                .map_err(|error| paged::history_error(error, cancellation))?
            else {
                break;
            };
            let wire_id = paged::replay_row_id(task, identity.rows_epoch, identity.row_seq);
            replayed_through = identity.row_seq;
            item.persisted = Some(identity);
            item.live = None;
            // Frozen finalized history has no streaming suffix state to retain.
            // Only rows also in the bounded active overlay seed live projection.
            let mut one_row = AcpProjection::default();
            let update = if active.contains(&wire_id) {
                checked_timeline_update(&item, &mut projection, true)
                    .map_err(paged::replay_error)?
            } else {
                timeline_update(&item, &mut one_row, true)
            };
            if let Some(update) = update {
                self.agent
                    .verify_generation()
                    .await
                    .map_err(internal_acp_error)?;
                self.send_session_update(
                    cx,
                    SessionNotification::new(protocol_session.clone(), update),
                    cancellation,
                )
                .await
                .map_err(outbound_error)?;
            }
        }
        // The replay source and this overlay share the captured source stamp.
        // Later commits arrive through the operation's already-registered FIFO
        // mailbox; ordinary appends must not invalidate the completed snapshot.
        for item in initial.items {
            self.agent
                .verify_generation()
                .await
                .map_err(internal_acp_error)?;
            if let Some(update) = checked_timeline_update(&item, &mut projection, true)
                .map_err(paged::replay_error)?
            {
                self.send_session_update(
                    cx,
                    SessionNotification::new(protocol_session.clone(), update),
                    cancellation,
                )
                .await
                .map_err(outbound_error)?;
            }
        }
        if cancellation.is_cancelled() {
            return Err(paged::replay_error(
                "Maple ACP history loading was cancelled",
            ));
        }
        projection.snapshot_revision = initial.timeline_revision;
        projection.note_replayed_history(task, initial.stored_marker.0, replayed_through);
        Ok(projection)
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
        let project = project_root
            .as_ref()
            .map(|root| root.to_string_lossy().into_owned());
        let mut cursor = request.cursor;
        let mut listed = Vec::new();
        loop {
            let query = crate::store::NavigationTaskQuery {
                section: crate::store::TaskSection::Recent,
                project: project.clone(),
                pins: crate::store::PinFilter::Ignore,
                cursor: cursor.clone(),
                limit: paged::ACP_LIST_ROWS - listed.len() as u16,
            };
            let page = tokio::select! {
                biased;
                _ = self.lifetime.cancelled() => return Err(paged::replay_error("The Maple ACP connection closed while listing tasks")),
                result = self.agent.list_tasks_page(query,crate::store::PageTransport::Local) => result.map_err(|error| {
                    if cursor.is_some() {
                        agent_client_protocol::Error::invalid_params().data("Invalid or expired Maple ACP session-list cursor")
                    } else {internal_acp_error(error)}
                })?,
            };
            if page.next_cursor.is_some() && page.next_cursor == cursor {
                return Err(paged::replay_error(
                    "Maple ACP session listing did not advance",
                ));
            }
            for session in page.tasks {
                let Ok(root) = ensure_allowed_project_root(
                    Path::new(&session.project_root),
                    &config.allowed_project_roots,
                ) else {
                    continue;
                };
                let mut info = SessionInfo::new(session.uuid, root).title(session.title);
                if let Some(updated) = chrono::DateTime::from_timestamp_millis(session.updated_ms) {
                    info = info.updated_at(updated.to_rfc3339());
                }
                listed.push(info);
            }
            cursor = page.next_cursor;
            if listed.len() >= usize::from(paged::ACP_LIST_ROWS) || cursor.is_none() {
                break;
            }
            // A disallowed/missing root consumes a keyset position. Refill with
            // another bounded query rather than returning short allowed pages.
        }
        let mut response = ListSessionsResponse::new(listed);
        if let Some(cursor) = cursor {
            response = response.next_cursor(cursor);
        }
        let bytes = serde_json::to_vec(&response)
            .map_err(|_| paged::replay_error("Maple ACP session listing could not be encoded"))?
            .len();
        if bytes > paged::ACP_LIST_BYTES {
            return Err(paged::replay_error(
                "Maple ACP session listing exceeded its byte limit",
            ));
        }
        Ok(response)
    }

    async fn refresh_config_options(
        &self,
        session_id: &str,
    ) -> Result<
        Vec<agent_client_protocol::schema::v1::SessionConfigOption>,
        agent_client_protocol::Error,
    > {
        let summary = self
            .agent
            .task_facts(session_id.to_string())
            .await
            .map_err(internal_acp_error)?;
        let model = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.get_mut(session_id).ok_or_else(|| {
                agent_client_protocol::Error::resource_not_found(Some(session_id.into()))
            })?;
            session.message_count = usize::try_from(summary.message_count)
                .map_err(|_| paged::replay_error("Invalid Maple task message count"))?;
            if let Some(model) = summary.model
                && session.model != model
            {
                session.model = model;
                session.catalog = None;
            }
            session.model.clone()
        };
        let thinking = self
            .agent
            .thinking_state(Some(session_id), &model)
            .await
            .map_err(internal_acp_error)?;
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(session_id).ok_or_else(|| {
            agent_client_protocol::Error::resource_not_found(Some(session_id.into()))
        })?;
        session.thinking = Some(thinking);
        Ok(session.config_options())
    }

    async fn set_config_option(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, agent_client_protocol::Error> {
        let session_id = canonical_session_id(&request.session_id)?;
        if self.closed.load(Ordering::SeqCst) {
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
                let model = model.to_string();
                drop(sessions);
                self.agent
                    .set_task_model(&session_id, &model)
                    .await
                    .map_err(internal_acp_error)?;
                return Ok(SetSessionConfigOptionResponse::new(
                    self.refresh_config_options(&session_id).await?,
                ));
            }
            "thought_level" => {
                let level = crate::agent::ThinkingLevel::parse(selected_value.0.as_ref())
                    .ok_or_else(|| {
                        agent_client_protocol::Error::invalid_params()
                            .data("Unknown thinking level")
                    })?;
                let model = session.model.clone();
                drop(sessions);
                let state = self
                    .agent
                    .thinking_state(Some(&session_id), &model)
                    .await
                    .map_err(internal_acp_error)?;
                if !state.levels.contains(&level) {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("This model does not support that thinking level"));
                }
                self.agent
                    .set_thinking_level(Some(&session_id), &model, level)
                    .await
                    .map_err(internal_acp_error)?;
                return Ok(SetSessionConfigOptionResponse::new(
                    self.refresh_config_options(&session_id).await?,
                ));
            }
            _ => Err(agent_client_protocol::Error::invalid_params()
                .data("Unknown Maple ACP configuration option")),
        }
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
            || operation.cancellation.is_cancelled()
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
        if operation.cancellation.is_cancelled()
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
            images: PromptAttachments::Images(images),
            cancellation,
            operation,
        })
    }

    async fn send_session_update(
        &self,
        cx: &ConnectionTo<Client>,
        mut notification: SessionNotification,
        cancellation: &CancellationToken,
    ) -> Result<(), AcpOutboundSendError> {
        // Chunk on UTF-8 byte boundaries before JSON encoding. Even a control
        // character-heavy chunk stays below the outbound credit/frame caps.
        let text = paged::notification_text(&mut notification.update)
            .filter(|text| text.len() > paged::ACP_TEXT_CHUNK_BYTES)
            .map(std::mem::take);
        if let Some(text) = text {
            let mut offset = 0;
            while offset < text.len() {
                let end = paged::next_text_boundary(&text, offset);
                let mut part = notification.clone();
                *paged::notification_text(&mut part.update)
                    .ok_or(AcpOutboundSendError::UpdateTooLarge)? = text[offset..end].to_string();
                let encoded = serde_json::to_vec(&part)
                    .map_err(|_| AcpOutboundSendError::UpdateTooLarge)?
                    .len();
                let reservation = self.outbound.reserve(encoded, cancellation).await?;
                self.outbound.enqueue(cx, part, reservation)?;
                offset = end;
            }
            return Ok(());
        }
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
        images: PromptAttachments,
        cancellation: &CancellationToken,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        let protocol_session_id = SessionId::new(session_id.to_string());
        if self.closed.load(Ordering::SeqCst) {
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
        let attachments = match images {
            PromptAttachments::Images(images) => self
                .agent
                .import_image_attachments(session_id.to_string(), images)
                .await
                .map_err(internal_acp_error)?
                .into_iter()
                .map(|attachment| attachment.id)
                .collect(),
            PromptAttachments::Committed(ids) => ids,
        };
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
                    attachments,
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
                .data("The Maple Work task was removed outside this ACP connection"));
            }
            Err(error) => return Err(internal_acp_error(error)),
        };
        self.owned_runs
            .lock()
            .await
            .insert(session_id.to_string(), run.run_id.clone());
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
        // A projection failure or disconnected observer never cancels shared
        // work. session/cancel is the explicit task-wide stop operation.
        let _ = (stopped_early, &mut terminal);
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
        let operation = self
            .session_operations
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(agent_client_protocol::Error::invalid_request)?;
        // A failure is told once the run ends failed: Pi may still retry it.
        let mut failure: Option<AgentTimelineItem> = None;
        let stop = loop {
            if overflowed.load(Ordering::Acquire) {
                break StreamStop::Overflowed;
            }
            let event = tokio::select! {
                _ = cancellation.cancelled() => return Ok((PromptResponse::new(StopReason::Cancelled), true)),
                event = events.recv() => event,
            };
            if overflowed.load(Ordering::Acquire) {
                break StreamStop::Overflowed;
            }
            // A tool row may be updated both by this run and by its external
            // agent. Share projection state and serialize each outgoing update
            // with the observer, without holding the gate while waiting for
            // the next model event.
            let _projection = tokio::select! {
                _ = cancellation.cancelled() => return Ok((PromptResponse::new(StopReason::Cancelled), true)),
                guard = operation.replay_gate.lock() => guard,
            };
            if cancellation.is_cancelled()
                || !self
                    .session_operation_is_current(&session_id, &operation)
                    .await
            {
                return Ok((PromptResponse::new(StopReason::Cancelled), true));
            }
            let update = match event {
                Some(AgentRunEvent::TimelineItem(item)) if item.item_type == "error" => {
                    failure = Some(item);
                    None
                }
                Some(AgentRunEvent::TimelineItem(item)) => {
                    let update = self
                        .sessions
                        .lock()
                        .await
                        .get_mut(&session_id)
                        .map(|session| {
                            checked_timeline_update(&item, &mut session.projection, false)
                        })
                        .transpose();
                    match update {
                        Ok(update) => update.flatten(),
                        Err(message) => {
                            operation.fail_observer(observer::ObserverFailure::ProjectionLimit);
                            return Err(paged::replay_error(message));
                        }
                    }
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
                            .data("Maple Work run ended without a terminal result")),
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
                "The ACP event stream fell behind. Reload this session to see the shared task."
            }
            StreamStop::UpdateTooLarge => {
                "An update exceeded the ACP transport limit. The shared task continues."
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
        if self.sessions.lock().await.contains_key(session_id) {
            let _ = self.agent.cancel_session(session_id).await;
        }
    }

    async fn cleanup(&self) {
        self.question_responder.lock().await.take();
        let deadline = tokio::time::Instant::now() + ACP_CONNECTION_CLEANUP_TIMEOUT;
        {
            // Linearize closure with the last new-session commit. A task that
            // reaches finalization after this point observes closed and
            // discards its new task instead of committing it.
            let _finalization = self.finalization.lock().await;
            self.closed.store(true, Ordering::SeqCst);
            self.lifetime.cancel();
        }
        // Stop projections and pending admissions, not already admitted runs.
        for state in self.prompt_states.lock().await.values() {
            state.cancellation.cancel();
        }
        // End every lease's hold before awaiting anything else: nothing is
        // accepted under one once this returns.
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
