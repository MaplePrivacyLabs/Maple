//! The host side: publishes a [`HostBackend`] to connections.
//!
//! One [`HostServer`] serves any number of connections. Each connection
//! subscribes to the host's events before it answers the handshake, so no
//! event is lost between the two; forwards every event with a per-connection
//! sequence; answers requests concurrently so a slow call never delays the
//! keepalive; and closes itself when its outbound queue overflows or the
//! peer goes quiet past the lease.
//!
//! Requests are dispatched by domain to one controller each. A controller
//! is a plain `match` over its domain's request enum; it never grows past
//! that domain.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use maple_agent::agent::{AgentImageUpload, AgentSendMessageRequest, AgentSessionDetail};
use maple_agent::host::{HostBackend, HostBootstrap};
use serde::Serialize;
use serde_json::Value;
use serde_json::value::RawValue;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::carrier::Carrier;
use crate::frame::{CONTROL_CHANNEL, Frame, FrameKind};
use crate::outbound::{self, DEFAULT_MAX_OUTBOUND_BYTES, Outbound};
use crate::rpc::{self, Message, Request as RpcRequest, Response, RpcError, code};
use crate::streams::{
    ATTACHMENT_PURPOSE, Opener, StreamOpen, StreamSenders, close_frame, decode_credit,
};
use crate::uploads::Uploads;
use crate::wire::{
    AttachmentHandle, BootstrapSnapshot, ClientHello, EventEnvelope, Features, HostHello, HostInfo,
    HostRequest, IntegrationRequest, ModelRequest, PROTOCOL_VERSION, ProjectRequest, Request,
    RunRequest, SessionRequest, SessionSnapshot, UPLOAD_STREAMS_FEATURE, UploadRef, decode_request,
    features, has_feature, version_label,
};

/// What the host tells clients about itself in the handshake.
#[derive(Debug, Clone)]
pub struct HostIdentity {
    pub app_version: String,
    /// The git revision this host was built from, when the build knew it.
    pub build: Option<String>,
    pub pcr_environment: String,
}

/// Called once a client's hello is accepted, with what the client claims
/// about itself. The listener uses it to name the paired device.
pub type ClientHelloHook = Arc<dyn Fn(&ClientHello) + Send + Sync>;

#[derive(Clone)]
pub struct HostServerConfig {
    /// Bytes one connection may have queued before it is closed.
    pub max_outbound_bytes: usize,
    /// A connection with no inbound frame for this long is closed.
    pub lease: Duration,
    /// How often the lease is checked.
    pub lease_check: Duration,
    /// Most timeline items in one page.
    pub timeline_page_items: usize,
    /// Approximate serialized bytes in one page; a page stops before
    /// the item that would cross it (at least one item always fits).
    pub timeline_page_bytes: usize,
    pub on_client_hello: Option<ClientHelloHook>,
}

impl Default for HostServerConfig {
    fn default() -> Self {
        Self {
            max_outbound_bytes: DEFAULT_MAX_OUTBOUND_BYTES,
            lease: Duration::from_secs(45),
            lease_check: Duration::from_secs(10),
            timeline_page_items: 200,
            timeline_page_bytes: 1024 * 1024,
            on_client_hello: None,
        }
    }
}

/// Loaded tasks one connection keeps for paging. Loading a ninth drops
/// the one paged least recently.
pub const MAX_KEPT_SNAPSHOTS: usize = 8;

/// Project roots one connection may watch at once.
pub const MAX_WATCHED_ROOTS: usize = 64;

/// How long a closing connection waits for its queued frames (a refusal,
/// an error answer) to reach the peer before the writer is abandoned.
const CLOSE_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

pub struct HostServer {
    host: Arc<dyn HostBackend>,
    info: HostInfo,
    identity: HostIdentity,
    config: HostServerConfig,
    /// Minted once per process; every connection's sequences are scoped
    /// to it.
    generation: String,
}

impl HostServer {
    pub fn new(
        host: Arc<dyn HostBackend>,
        info: HostInfo,
        identity: HostIdentity,
        config: HostServerConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            host,
            info,
            identity,
            config,
            generation: uuid::Uuid::new_v4().to_string(),
        })
    }

    /// Serve one connection until it ends, with no identity check on the
    /// hello. For carriers that authenticated nothing: tests and loopback.
    pub async fn serve(self: Arc<Self>, carrier: Carrier) -> Result<(), String> {
        self.serve_with_peer(carrier, None, CancellationToken::new())
            .await
    }

    /// Serve one connection whose transport authenticated the peer as
    /// `peer` (the device's static public key). The hello must claim the
    /// same key, so the identity in the protocol is the identity the
    /// handshake proved. Cancelling `cancel` ends the connection.
    pub async fn serve_with_peer(
        self: Arc<Self>,
        carrier: Carrier,
        peer: Option<String>,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let Carrier {
            mut sink,
            mut stream,
        } = carrier;
        let (out, mut queue) = outbound::channel(self.config.max_outbound_bytes);
        let connection = Arc::new(Connection {
            server: Arc::clone(&self),
            out,
            senders: StreamSenders::new(Opener::Host),
            uploads: Uploads::default(),
            client_features: OnceLock::new(),
            snapshots: Mutex::new(Vec::new()),
            watched_roots: Mutex::new(HashMap::new()),
            ready: AtomicBool::new(false),
            ready_notify: Notify::new(),
            last_activity: std::sync::Mutex::new(Instant::now()),
            peer,
            closed: cancel,
            close_reason: std::sync::Mutex::new(None),
        });

        // Subscribe before the handshake so nothing is missed between the
        // two; the forwarder holds events until the client is ready. An
        // overflow closes the connection right here.
        let mut events = self.host.subscribe();
        let forwarder = {
            let connection = Arc::clone(&connection);
            tokio::spawn(async move {
                connection.ready_notify.notified().await;
                let mut seq: u64 = 0;
                while let Some(event) = events.recv().await {
                    seq += 1;
                    let envelope = EventEnvelope { seq, event };
                    if let Err(error) = connection.notify(crate::wire::EVENT_METHOD, &envelope) {
                        connection.close(&error);
                        break;
                    }
                }
            })
        };

        // The writer owns the carrier's sink and the queue's draining end
        // and nothing else, so it cannot keep the connection alive. It
        // ends when the connection closes, after writing what was queued.
        let mut writer = {
            let closed = connection.closed.clone();
            tokio::spawn(async move {
                loop {
                    let frame = tokio::select! {
                        frame = queue.recv() => frame,
                        _ = closed.cancelled() => {
                            while let Some(frame) = queue.try_recv() {
                                if sink.send(frame).await.is_err() {
                                    break;
                                }
                            }
                            None
                        }
                    };
                    let Some(frame) = frame else { break };
                    if sink.send(frame).await.is_err() {
                        break;
                    }
                }
                sink.close().await;
            })
        };

        let lease = {
            let connection = Arc::clone(&connection);
            let lease = self.config.lease;
            let check = self.config.lease_check;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(check).await;
                    if connection.idle_for() > lease {
                        connection.close("lease expired");
                        return;
                    }
                }
            })
        };

        let reason = loop {
            let frame = tokio::select! {
                frame = stream.recv() => frame,
                _ = connection.closed.cancelled() => None,
            };
            let Some(frame) = frame else {
                connection.close("peer closed");
                break connection
                    .close_reason()
                    .unwrap_or_else(|| "peer closed".to_string());
            };
            connection.touch();
            if let Err(error) = connection.on_frame(frame) {
                connection.close(&error);
                break error;
            }
        };
        forwarder.abort();
        lease.abort();
        // Let queued frames (a refusal, an error answer) reach the peer; a
        // peer that stopped reading does not get to hold the writer.
        if tokio::time::timeout(CLOSE_FLUSH_TIMEOUT, &mut writer)
            .await
            .is_err()
        {
            writer.abort();
        }
        connection.release_watches().await;
        log::debug!("host connection ended: {reason}");
        Ok(())
    }
}

/// One client connection.
struct Connection {
    server: Arc<HostServer>,
    out: Outbound,
    /// Streams this host opens: attachment reads.
    senders: StreamSenders,
    /// Streams the client opens: images ahead of `run.send`. Dropped
    /// with the connection.
    uploads: Uploads,
    /// What the client's hello advertised.
    client_features: OnceLock<Features>,
    /// Snapshots the client pages through, least recently paged first.
    /// Replaced by the next load of the same task; at most
    /// [`MAX_KEPT_SNAPSHOTS`].
    snapshots: Mutex<Vec<(String, Arc<AgentSessionDetail>)>>,
    /// Project roots this connection asked the host to watch, with how
    /// many times each, so teardown can balance every watch.
    watched_roots: Mutex<HashMap<String, usize>>,
    ready: AtomicBool,
    ready_notify: Notify,
    last_activity: std::sync::Mutex<Instant>,
    /// The device key the transport proved, when it proved one.
    peer: Option<String>,
    closed: CancellationToken,
    close_reason: std::sync::Mutex<Option<String>>,
}

impl Connection {
    fn touch(&self) {
        *self
            .last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
    }

    /// End the connection. The first reason given is the one kept.
    fn close(&self, reason: &str) {
        let mut close_reason = self
            .close_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if close_reason.is_none() {
            log::debug!("closing host connection: {reason}");
            *close_reason = Some(reason.to_string());
            self.closed.cancel();
        }
    }

    fn client_has(&self, feature: &str) -> bool {
        self.client_features
            .get()
            .is_some_and(|features| has_feature(features, feature))
    }

    fn close_reason(&self) -> Option<String> {
        self.close_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The connection is gone: take back every watch it placed.
    async fn release_watches(&self) {
        let watched = std::mem::take(&mut *self.watched_roots.lock().await);
        for (root, count) in watched {
            for _ in 0..count {
                if let Err(error) = self.host().unwatch_project_root(root.clone()).await {
                    log::debug!("cannot unwatch {root} at teardown: {error}");
                }
            }
        }
    }

    fn notify<T: Serialize>(&self, method: &str, params: &T) -> Result<(), String> {
        let params = serde_json::to_value(params).map_err(|error| error.to_string())?;
        let message = Message::Notification(rpc::Notification::new(method, params));
        self.send_message(&message)
    }

    fn send_message(&self, message: &Message) -> Result<(), String> {
        let bytes = rpc::encode(message)?;
        self.out.try_send(Frame::control(bytes))
    }

    fn respond(&self, response: Response) {
        if let Err(error) = self.send_message(&Message::Response(response)) {
            self.close(&error);
        }
    }

    /// Answer a request with JSON that is already encoded.
    fn respond_ok(&self, id: u64, answer: &RawValue) {
        if let Err(error) = rpc::encode_result(id, answer)
            .and_then(|bytes| self.out.try_send(Frame::control(bytes)))
        {
            self.close(&error);
        }
    }

    /// Route one inbound frame. Requests are answered on their own task.
    fn on_frame(self: &Arc<Self>, frame: Frame) -> Result<(), String> {
        if frame.channel != CONTROL_CHANNEL {
            return self.on_stream_frame(frame);
        }
        let message = rpc::decode(&frame.payload)?;
        match message {
            Message::Request(request) => {
                let connection = Arc::clone(self);
                tokio::spawn(async move { connection.handle(request).await });
            }
            Message::Notification(notification) => {
                log::debug!("ignoring notification {}", notification.method);
            }
            Message::Response(_) => {
                log::debug!("ignoring a response from the client");
            }
        }
        Ok(())
    }

    /// Route a frame on a stream channel by which side opened it.
    fn on_stream_frame(&self, frame: Frame) -> Result<(), String> {
        match Opener::of_channel(frame.channel) {
            // A stream this host is sending on: the client's flow control
            // and its acknowledgement or refusal.
            Opener::Host => match frame.kind {
                FrameKind::Credit => {
                    if let Some(credit) = decode_credit(&frame.payload) {
                        self.senders.credit(frame.channel, credit);
                    }
                }
                FrameKind::Close => self.senders.on_close(frame.channel, &frame.payload),
                FrameKind::Open | FrameKind::Data => {}
            },
            // A stream the client is sending on: an upload. Before the
            // handshake, or from a client that did not advertise uploads,
            // an open is refused and anything else is dropped.
            Opener::Client => {
                let reply = if self.client_has(UPLOAD_STREAMS_FEATURE) {
                    self.uploads.on_frame(&frame)
                } else if frame.kind == FrameKind::Open {
                    Some(close_frame(
                        frame.channel,
                        Some("uploads need a handshake that advertises uploadStreams"),
                    ))
                } else {
                    None
                };
                if let Some(reply) = reply {
                    self.out.try_send(reply)?;
                }
            }
        }
        Ok(())
    }

    async fn handle(self: Arc<Self>, request: RpcRequest) {
        let id = request.id;
        let decoded = match decode_request(&request.method, request.params) {
            Ok(decoded) => decoded,
            Err(crate::wire::DecodeError::UnknownMethod(method)) => {
                self.respond(Response::err(
                    id,
                    RpcError::new(code::METHOD_NOT_FOUND, format!("unknown method {method}")),
                ));
                return;
            }
            Err(crate::wire::DecodeError::InvalidParams(message)) => {
                self.respond(Response::err(
                    id,
                    RpcError::new(code::INVALID_PARAMS, message),
                ));
                return;
            }
        };
        if !self.ready.load(Ordering::Acquire) {
            match decoded {
                Request::Host(HostRequest::Hello(hello)) => self.handle_hello(id, hello),
                _ => self.respond(Response::err(
                    id,
                    RpcError::new(code::NOT_READY, "send host.hello first"),
                )),
            }
            return;
        }
        // A hello after the handshake is an ordinary request, and the host
        // controller refuses it: the hook and the ready state ran once.
        let result = match decoded {
            Request::Host(request) => self.host_controller(request).await,
            Request::Project(request) => self.project_controller(request).await,
            Request::Session(request) => self.session_controller(id, request).await,
            Request::Run(request) => self.run_controller(request).await,
            Request::Model(request) => self.model_controller(request).await,
            Request::Integration(request) => self.integration_controller(request).await,
        };
        match result {
            Ok(answer) => self.respond_ok(id, &answer),
            Err(error) => self.respond(Response::err(id, error)),
        }
    }

    fn handle_hello(&self, id: u64, mut hello: ClientHello) {
        // The name is what the client claims; keep it short and printable
        // before it reaches a log or the device list.
        hello.device.name = crate::devices::clean_device_name(&hello.device.name);
        let identity = &self.server.identity;
        let refusal = if hello.protocol != PROTOCOL_VERSION {
            Some(format!(
                "protocol {} is not supported; this host speaks {PROTOCOL_VERSION}. Update the client or the host.",
                hello.protocol
            ))
        } else if hello.pcr_environment != identity.pcr_environment {
            Some(format!(
                "the client is built for the {} environment and this host for {}",
                hello.pcr_environment, identity.pcr_environment
            ))
        } else if self
            .peer
            .as_ref()
            .is_some_and(|peer| peer != &hello.device.public_key)
        {
            Some("the hello names a different device than the one that connected".to_string())
        } else {
            None
        };
        if let Some(message) = refusal {
            self.respond(Response::err(
                id,
                RpcError::new(code::HANDSHAKE_REFUSED, message),
            ));
            self.close("handshake refused");
            return;
        }
        log::info!(
            "client {} ({}) connected running maple-agent {}",
            hello.device.name,
            hello.device.public_key,
            version_label(&hello.app_version, hello.build.as_deref())
        );
        let answer = HostHello {
            protocol: PROTOCOL_VERSION,
            app_version: identity.app_version.clone(),
            build: identity.build.clone(),
            pcr_environment: identity.pcr_environment.clone(),
            generation: self.server.generation.clone(),
            seq: 0,
            features: features(),
            host: self.server.info.clone(),
        };
        let answer = match encode_answer(&answer) {
            Ok(answer) => answer,
            Err(error) => {
                self.respond(Response::err(id, error));
                return;
            }
        };
        // Two hellos in flight at once: the first to get here wins.
        if self
            .ready
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.respond(Response::err(
                id,
                RpcError::new(code::INVALID_REQUEST, "hello was already sent"),
            ));
            return;
        }
        let _ = self.client_features.set(hello.features.clone());
        self.respond_ok(id, &answer);
        self.ready_notify.notify_one();
        if let Some(hook) = &self.server.config.on_client_hello {
            hook(&hello);
        }
    }

    fn host(&self) -> &Arc<dyn HostBackend> {
        &self.server.host
    }

    /// Encode a controller result once, mapping host errors to RPC errors.
    fn ok<T: Serialize>(value: Result<T, String>) -> Result<Answer, RpcError> {
        let value = value.map_err(RpcError::host)?;
        encode_answer(&value)
    }

    async fn host_controller(&self, request: HostRequest) -> Result<Answer, RpcError> {
        let host = self.host();
        match request {
            HostRequest::Hello(_) => Err(RpcError::new(
                code::INVALID_REQUEST,
                "hello was already sent",
            )),
            HostRequest::Ping => Self::ok(Ok(serde_json::Map::<String, Value>::new())),
            HostRequest::Bootstrap => {
                let bootstrap = host.bootstrap().await.map_err(RpcError::host)?;
                Self::ok(Ok(self.snapshot_bootstrap(bootstrap).await))
            }
            HostRequest::StartRuntime { request } => Self::ok(host.start_runtime(request).await),
            HostRequest::StopRuntime => Self::ok(host.stop_runtime().await),
            HostRequest::SessionDefaults => Self::ok(host.session_defaults().await),
            HostRequest::SetSessionDefaults { defaults } => {
                Self::ok(host.set_session_defaults(defaults).await)
            }
            HostRequest::SaveDefaultModel { model } => {
                Self::ok(host.save_default_model(model).await)
            }
            HostRequest::UsageSummary => Self::ok(host.usage_summary().await),
            HostRequest::ContextUsage { session_id, model } => {
                Self::ok(host.context_usage(session_id, model).await)
            }
            HostRequest::ToolSummaries { session_id } => {
                Self::ok(host.tool_summaries(session_id).await)
            }
            HostRequest::StoreToolSummary {
                session_id,
                item_id,
                summary,
            } => Self::ok(host.store_tool_summary(session_id, item_id, summary).await),
        }
    }

    async fn project_controller(&self, request: ProjectRequest) -> Result<Answer, RpcError> {
        let host = self.host();
        match request {
            ProjectRequest::RecentRoots => Self::ok(host.recent_project_roots().await),
            ProjectRequest::SelectRoot { path } => Self::ok(host.select_project_root(path).await),
            ProjectRequest::RemoveRoot { path, fallback } => {
                Self::ok(host.remove_project_root(path, fallback).await)
            }
            ProjectRequest::SuggestDirectories { query } => {
                Self::ok(host.suggest_directories(query).await)
            }
            ProjectRequest::Watch { path } => {
                let mut watched = self.watched_roots.lock().await;
                if !watched.contains_key(&path) && watched.len() >= MAX_WATCHED_ROOTS {
                    return Err(RpcError::new(
                        code::INVALID_REQUEST,
                        format!("a connection may watch at most {MAX_WATCHED_ROOTS} project roots"),
                    ));
                }
                host.watch_project_root(path.clone())
                    .await
                    .map_err(RpcError::host)?;
                *watched.entry(path).or_default() += 1;
                Self::ok(Ok(()))
            }
            ProjectRequest::Unwatch { path } => {
                let mut watched = self.watched_roots.lock().await;
                match watched.get_mut(&path) {
                    Some(count) if *count > 1 => *count -= 1,
                    Some(_) => {
                        watched.remove(&path);
                    }
                    // Never watched here: nothing to balance.
                    None => return Self::ok(Ok(())),
                }
                Self::ok(host.unwatch_project_root(path).await)
            }
            ProjectRequest::Trust { path } => Self::ok(host.project_trust(path).await),
            ProjectRequest::SetTrust { path, trusted } => {
                Self::ok(host.set_project_trust(path, trusted).await)
            }
        }
    }

    async fn session_controller(
        self: &Arc<Self>,
        request_id: u64,
        request: SessionRequest,
    ) -> Result<Answer, RpcError> {
        let host = self.host();
        match request {
            SessionRequest::List { project_root } => {
                Self::ok(host.list_sessions(project_root).await)
            }
            SessionRequest::Create { request } => Self::ok(host.create_session(request).await),
            SessionRequest::Load { session_id } => {
                let detail = host
                    .load_session(session_id)
                    .await
                    .map_err(RpcError::host)?;
                Self::ok(Ok(self.snapshot_session(detail).await))
            }
            SessionRequest::Timeline {
                session_id,
                offset,
                limit,
            } => Self::ok(self.timeline_page(session_id, offset, limit).await),
            SessionRequest::Rename { session_id, title } => {
                Self::ok(host.rename_session(session_id, title).await)
            }
            SessionRequest::SetState { session_id, state } => {
                Self::ok(host.set_session_state(session_id, state).await)
            }
            SessionRequest::Delete { session_id } => {
                Self::ok(host.delete_session(session_id).await)
            }
            SessionRequest::Compact { session_id } => {
                Self::ok(host.compact_session(session_id).await)
            }
            SessionRequest::Subagents { session_id } => {
                Self::ok(host.session_subagents(session_id).await)
            }
            SessionRequest::CancelExternalAgent {
                session_id,
                agent_id,
            } => Self::ok(host.cancel_external_agent(session_id, agent_id).await),
            SessionRequest::SetPermissionMode { session_id, mode } => {
                Self::ok(host.set_permission_mode(session_id, mode).await)
            }
            SessionRequest::SetWebEnabled {
                session_id,
                enabled,
            } => Self::ok(host.set_session_web_enabled(session_id, enabled).await),
            SessionRequest::ReadAttachment {
                session_id,
                attachment_id,
            } => {
                let bytes = host
                    .read_image_attachment(session_id, attachment_id)
                    .await
                    .map_err(RpcError::host)?;
                let mut sender = self
                    .senders
                    .open(
                        &self.out,
                        StreamOpen {
                            purpose: ATTACHMENT_PURPOSE.to_string(),
                            request_id: Some(request_id),
                            upload_id: None,
                            mime: None,
                            len: Some(bytes.len() as u64),
                        },
                    )
                    .map_err(RpcError::host)?;
                let handle = AttachmentHandle {
                    stream: sender.channel(),
                    len: bytes.len() as u64,
                };
                // The bytes follow the answer; the receiver pairs them
                // through the request id in the open frame.
                let connection = Arc::clone(self);
                tokio::spawn(async move {
                    if let Err(error) = sender.send_all(&bytes).await {
                        log::debug!("attachment stream ended early: {error}");
                        if connection.out.overflowed() {
                            connection.close(&error);
                        }
                    }
                });
                Self::ok(Ok(handle))
            }
        }
    }

    async fn run_controller(&self, request: RunRequest) -> Result<Answer, RpcError> {
        let host = self.host();
        match request {
            RunRequest::Send { request, uploads } => {
                let request = self.resolve_uploads(request, uploads)?;
                Self::ok(host.send_message(request).await)
            }
            RunRequest::Cancel { run_id } => Self::ok(host.cancel_run(run_id).await),
            RunRequest::CancelQueued {
                session_id,
                queue_id,
            } => Self::ok(host.cancel_queued_message(session_id, queue_id).await),
            RunRequest::BeginQueuedEdit {
                session_id,
                queue_id,
            } => Self::ok(host.begin_queued_message_edit(session_id, queue_id).await),
            RunRequest::EndQueuedEdit {
                session_id,
                queue_id,
            } => Self::ok(host.end_queued_message_edit(session_id, queue_id).await),
            RunRequest::AnswerQuestion { request_id, answer } => {
                Self::ok(host.answer_question(request_id, answer).await)
            }
            RunRequest::PermissionRespond {
                session_id,
                request_id,
                allow,
            } => Self::ok(host.permission_respond(session_id, request_id, allow).await),
            RunRequest::AskSideQuestion {
                session_id,
                request_id,
                prior,
                question,
            } => Self::ok(
                host.ask_side_question(session_id, request_id, prior, question)
                    .await,
            ),
            RunRequest::SummarizeToolCall {
                session_id,
                tool_name,
                input,
                output_text,
            } => Self::ok(
                host.summarize_tool_call(session_id, tool_name, input, output_text)
                    .await,
            ),
            RunRequest::SummarizeThinking {
                session_id,
                thinking_text,
            } => Self::ok(host.summarize_thinking(session_id, thinking_text).await),
        }
    }

    /// Replace the upload ids a `run.send` names with the images the
    /// client streamed, consuming them. Images inline in the request are
    /// refused: they would be capped by the control frame limit.
    fn resolve_uploads(
        &self,
        mut request: AgentSendMessageRequest,
        uploads: Vec<UploadRef>,
    ) -> Result<AgentSendMessageRequest, RpcError> {
        if !request.attachments.is_empty() {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "this host takes images as upload streams, not inline in run.send; update the client",
            ));
        }
        if uploads.is_empty() {
            return Ok(request);
        }
        if !self.client_has(UPLOAD_STREAMS_FEATURE) {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "run.send names uploads but the client did not advertise uploadStreams",
            ));
        }
        let ids: Vec<String> = uploads
            .iter()
            .map(|upload| upload.upload_id.clone())
            .collect();
        let images = self
            .uploads
            .take_all(&ids)
            .map_err(|reason| RpcError::new(code::INVALID_PARAMS, reason))?;
        request.attachments = uploads
            .into_iter()
            .zip(images)
            .map(|(upload, image)| AgentImageUpload {
                name: upload.name,
                data_url: image.data_url(),
            })
            .collect();
        Ok(request)
    }

    async fn model_controller(&self, request: ModelRequest) -> Result<Answer, RpcError> {
        let host = self.host();
        match request {
            ModelRequest::List => Self::ok(host.available_model_ids().await),
            ModelRequest::SupportsVision { model } => {
                Self::ok(host.model_supports_vision(model).await)
            }
            ModelRequest::SlashCommands { working_dir } => {
                Self::ok(host.list_slash_commands(working_dir).await)
            }
            ModelRequest::ResolveSlashCommand {
                working_dir,
                command,
                args,
            } => Self::ok(host.resolve_slash_command(working_dir, command, args).await),
        }
    }

    async fn integration_controller(
        &self,
        request: IntegrationRequest,
    ) -> Result<Answer, RpcError> {
        let host = self.host();
        match request {
            IntegrationRequest::ListSessionMcp { session_id } => {
                Self::ok(host.list_session_mcp_servers(session_id).await)
            }
            IntegrationRequest::SetSessionMcp {
                session_id,
                name,
                kind,
                enabled,
            } => Self::ok(
                host.set_session_mcp_server_enabled(session_id, name, kind, enabled)
                    .await,
            ),
            IntegrationRequest::ListMcp => Self::ok(host.list_mcp_servers().await),
            IntegrationRequest::SaveMcp { servers } => {
                Self::ok(host.save_mcp_servers(servers).await)
            }
            IntegrationRequest::List => Self::ok(host.list_integrations().await),
            IntegrationRequest::SetEnabled { id, enabled } => {
                Self::ok(host.set_integration_enabled(id, enabled).await)
            }
        }
    }

    /// Keep a loaded task for paging and answer with its timeline stripped.
    async fn snapshot_session(&self, detail: AgentSessionDetail) -> SessionSnapshot {
        let timeline_len = detail.timeline.len();
        let detail = Arc::new(detail);
        self.keep_snapshot(&detail.session.id, Arc::clone(&detail))
            .await;
        let mut stripped = (*detail).clone();
        stripped.timeline = Vec::new();
        SessionSnapshot {
            detail: stripped,
            timeline_len,
        }
    }

    /// Remember `detail` as the most recently used snapshot, dropping the
    /// least recently used one past [`MAX_KEPT_SNAPSHOTS`].
    async fn keep_snapshot(&self, session_id: &str, detail: Arc<AgentSessionDetail>) {
        let mut snapshots = self.snapshots.lock().await;
        snapshots.retain(|(id, _)| id != session_id);
        snapshots.push((session_id.to_string(), detail));
        if snapshots.len() > MAX_KEPT_SNAPSHOTS {
            snapshots.remove(0);
        }
    }

    /// The kept snapshot of `session_id`, marked most recently used.
    async fn kept_snapshot(&self, session_id: &str) -> Option<Arc<AgentSessionDetail>> {
        let mut snapshots = self.snapshots.lock().await;
        let index = snapshots.iter().position(|(id, _)| id == session_id)?;
        let entry = snapshots.remove(index);
        let detail = Arc::clone(&entry.1);
        snapshots.push(entry);
        Some(detail)
    }

    async fn snapshot_bootstrap(&self, mut bootstrap: HostBootstrap) -> BootstrapSnapshot {
        let mut latest_timeline_len = 0;
        if let Some(latest) = bootstrap.latest.take() {
            let snapshot = self.snapshot_session(latest).await;
            latest_timeline_len = snapshot.timeline_len;
            bootstrap.latest = Some(snapshot.detail);
        }
        BootstrapSnapshot {
            bootstrap,
            latest_timeline_len,
        }
    }

    /// One page of a kept snapshot, bounded by item count and bytes. A
    /// task that was never loaded on this connection is loaded first.
    /// Each item is serialized once, to measure it, and that text is what
    /// the answer carries.
    async fn timeline_page(
        &self,
        session_id: String,
        offset: usize,
        limit: usize,
    ) -> Result<EncodedTimelinePage, String> {
        let detail = match self.kept_snapshot(&session_id).await {
            Some(detail) => detail,
            None => {
                let detail = self.host().load_session(session_id.clone()).await?;
                let detail = Arc::new(detail);
                self.keep_snapshot(&session_id, Arc::clone(&detail)).await;
                detail
            }
        };
        let config = &self.server.config;
        let limit = limit.clamp(1, config.timeline_page_items);
        let mut items: Vec<Box<RawValue>> = Vec::new();
        let mut bytes = 0usize;
        for item in detail.timeline.iter().skip(offset) {
            let json = serde_json::to_string(item).map_err(|error| error.to_string())?;
            if !items.is_empty()
                && (items.len() >= limit || bytes + json.len() > config.timeline_page_bytes)
            {
                break;
            }
            bytes += json.len();
            items.push(RawValue::from_string(json).map_err(|error| error.to_string())?);
        }
        let has_more = offset + items.len() < detail.timeline.len();
        Ok(EncodedTimelinePage { items, has_more })
    }
}

/// A controller's answer: the result's JSON, encoded once.
type Answer = Box<RawValue>;

fn encode_answer<T: Serialize>(value: &T) -> Result<Answer, RpcError> {
    serde_json::to_string(value)
        .and_then(RawValue::from_string)
        .map_err(|error| RpcError::host(error.to_string()))
}

/// [`crate::wire::TimelinePage`] with its items already encoded; the same
/// JSON on the wire.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EncodedTimelinePage {
    items: Vec<Box<RawValue>>,
    has_more: bool,
}
