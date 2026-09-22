//! The client side: a [`HostBackend`] over one connection to a host.
//!
//! [`RemoteHostBackend`] speaks the wire to a [`crate::server::HostServer`]
//! and presents the same trait the local host does, so the UI drives it
//! without knowing where it runs. One instance is one connection; when the
//! connection ends the instance is dead, and whoever owns it reconnects
//! with a fresh one and treats the change as a resync.
//!
//! Delivery: every event carries the connection's sequence. A gap means
//! the host dropped this client's queue or something in between lost
//! frames; the backend then publishes [`HostEvent::Resync`] so the UI
//! re-reads what it shows, rather than trusting the stream. Liveness is
//! an application ping on its own budget; a request timeout is an
//! operation failure, never proof the connection is dead.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use maple_agent::agent::{
    AgentCreateSessionRequest, AgentDesktopQueueSnapshot, AgentImageUpload, AgentIntegration,
    AgentMcpServer, AgentProjectRootRegistration, AgentProjectTrustStatus, AgentRuntimeStatus,
    AgentSendMessageRequest, AgentSessionDetail, AgentSessionIntegrationKind,
    AgentSessionMcpServer, AgentSessionSummary, AgentSlashCommand, AgentStartRequest,
    AgentSubagent, AgentTaskState, RecentProjectRoot, SideQuestionTurn,
};
use maple_agent::host::{
    ContextUsage, DirectorySuggestion, HostBackend, HostBootstrap, HostEvent, HostEventHub, HostId,
    HostSessionDefaults, UsageSummary,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::carrier::Carrier;
use crate::frame::{CONTROL_CHANNEL, Frame, FrameKind};
use crate::outbound::{self, DEFAULT_MAX_OUTBOUND_BYTES, Outbound};
use crate::rpc::{self, Message, Request as RpcRequest, RpcError};
use crate::streams::{
    ATTACHMENT_PURPOSE, MAX_IMAGE_BYTES, Opener, StreamOpen, StreamReceivers, StreamResult,
    StreamSenders, UPLOAD_PURPOSE, close_frame, decode_credit,
};
use crate::wire::{
    AttachmentHandle, BootstrapSnapshot, ClientHello, EVENT_METHOD, EventEnvelope, HostHello,
    HostRequest, IntegrationRequest, ModelRequest, PROTOCOL_VERSION, ProjectRequest, RunRequest,
    SessionRequest, SessionSnapshot, TimelinePage, UPLOAD_STREAMS_FEATURE, UploadRef, has_feature,
};

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// The handshake must complete within this.
    pub connect_timeout: Duration,
    /// Default budget for one request.
    pub request_timeout: Duration,
    /// Budget for a runtime start, which has its own long timeout on the
    /// host.
    pub long_request_timeout: Duration,
    /// Application ping period.
    pub ping_interval: Duration,
    /// A ping unanswered for this long counts as a miss.
    pub ping_timeout: Duration,
    /// Consecutive misses before the connection is declared dead.
    pub ping_misses: u32,
    /// Timeline items requested per page.
    pub timeline_page_items: usize,
    pub max_outbound_bytes: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(60),
            long_request_timeout: Duration::from_secs(90),
            ping_interval: Duration::from_secs(10),
            ping_timeout: Duration::from_secs(15),
            ping_misses: 2,
            timeline_page_items: 200,
            max_outbound_bytes: DEFAULT_MAX_OUTBOUND_BYTES,
        }
    }
}

type Pending = oneshot::Sender<Result<Value, RpcError>>;

/// Why a remote client cannot set up a curated integration.
const SETUP_IS_LOCAL: &str = "set up integrations on the host itself";

/// Most timeline items reserved up front on the host's announced length.
const MAX_PREALLOCATED_ITEMS: usize = 4096;

/// Attachment streams the host opens to answer `session.read_attachment`,
/// paired with the request that asked. A request registers its waiter
/// before it goes out; a stream for a request nobody waits on is refused.
struct AttachmentReads {
    receivers: StreamReceivers<u64>,
    waiters: std::sync::Mutex<HashMap<u64, oneshot::Sender<StreamResult>>>,
}

impl Default for AttachmentReads {
    fn default() -> Self {
        Self {
            receivers: StreamReceivers::new(MAX_IMAGE_BYTES),
            waiters: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl AttachmentReads {
    /// Wait for the stream that answers `request_id`.
    fn expect(&self, request_id: u64) -> oneshot::Receiver<StreamResult> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(request_id, tx);
        rx
    }

    fn resolve(&self, request_id: u64, result: StreamResult) {
        let waiter = self
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&request_id);
        if let Some(waiter) = waiter {
            let _ = waiter.send(result);
        }
    }

    /// The request failed or gave up waiting: forget its waiter and any
    /// stream already opened for it, so late frames are dropped.
    fn abandon(&self, request_id: u64) {
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&request_id);
        self.receivers.abandon(&request_id);
    }

    /// A frame on a host-opened channel. Returns the frame to send back:
    /// credit, or a `Close` refusing a stream the host should stop.
    fn on_frame(&self, frame: &Frame) -> Option<Frame> {
        match frame.kind {
            FrameKind::Open => {
                let open: StreamOpen = match serde_json::from_slice(&frame.payload) {
                    Ok(open) => open,
                    Err(error) => {
                        return Some(close_frame(frame.channel, Some(&error.to_string())));
                    }
                };
                let expected = open.request_id.filter(|request_id| {
                    self.waiters
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_key(request_id)
                });
                let Some(request_id) = expected.filter(|_| open.purpose == ATTACHMENT_PURPOSE)
                else {
                    return Some(close_frame(
                        frame.channel,
                        Some("no request awaits this stream"),
                    ));
                };
                match self.receivers.accept(frame.channel, request_id, open.len) {
                    Ok(()) => None,
                    Err(reason) => {
                        self.resolve(request_id, Err(reason.clone()));
                        Some(close_frame(frame.channel, Some(&reason)))
                    }
                }
            }
            FrameKind::Data => match self.receivers.on_data(frame.channel, &frame.payload) {
                Ok(credit) => credit,
                Err((request_id, reason)) => {
                    self.resolve(request_id, Err(reason.clone()));
                    Some(close_frame(frame.channel, Some(&reason)))
                }
            },
            FrameKind::Close => {
                if let Some((request_id, result)) =
                    self.receivers.on_close(frame.channel, &frame.payload)
                {
                    self.resolve(request_id, result);
                }
                None
            }
            // The host does not grant credit on its own stream.
            FrameKind::Credit => None,
        }
    }

    /// The connection ended: every waiter fails.
    fn fail_all(&self, reason: &str) {
        self.receivers.clear();
        let waiters = std::mem::take(
            &mut *self
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (_, waiter) in waiters {
            let _ = waiter.send(Err(reason.to_string()));
        }
    }
}

pub struct RemoteHostBackend {
    id: HostId,
    hello: HostHello,
    config: ClientConfig,
    out: Outbound,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    events: Arc<HostEventHub>,
    /// Streams this client opens: uploads ahead of `run.send`.
    senders: StreamSenders,
    /// Streams the host opens: attachment reads.
    attachments: AttachmentReads,
    closed: watch::Sender<Option<String>>,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl RemoteHostBackend {
    /// Open a connection: run the handshake and start the reader and the
    /// keepalive. Fails when the host refuses the hello.
    pub async fn connect(
        carrier: Carrier,
        hello: ClientHello,
        config: ClientConfig,
    ) -> Result<Arc<Self>, String> {
        let Carrier {
            mut sink,
            mut stream,
        } = carrier;
        let (out, mut queue) = outbound::channel(config.max_outbound_bytes);
        let (closed_tx, _) = watch::channel(None);
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<Option<Frame>>();
        // The reader task only moves frames; demultiplexing needs the
        // backend, which does not exist until the handshake answered.
        let reader = tokio::spawn(async move {
            loop {
                let frame = stream.recv().await;
                let ended = frame.is_none();
                if inbound_tx.send(frame).is_err() || ended {
                    break;
                }
            }
        });
        let writer = tokio::spawn(async move {
            while let Some(frame) = queue.recv().await {
                if sink.send(frame).await.is_err() {
                    break;
                }
            }
            sink.close().await;
        });

        // Handshake, by hand: the demultiplexer is not running yet.
        let hello_request = RpcRequest::new(
            1,
            "host.hello",
            serde_json::to_value(&hello).map_err(|error| error.to_string())?,
        );
        out.try_send(Frame::control(rpc::encode(&Message::Request(
            hello_request,
        ))?))?;
        let answer = tokio::time::timeout(config.connect_timeout, async {
            loop {
                match inbound_rx.recv().await.flatten() {
                    Some(frame) if frame.channel == CONTROL_CHANNEL => {
                        if let Message::Response(response) = rpc::decode(&frame.payload)? {
                            return Ok::<_, String>(response);
                        }
                    }
                    Some(_) => continue,
                    None => return Err("the host closed the connection".to_string()),
                }
            }
        })
        .await
        .map_err(|_| "the host did not answer the handshake in time".to_string())??;
        let host_hello: HostHello = match (answer.result, answer.error) {
            (Some(value), _) => serde_json::from_value(value).map_err(|error| error.to_string())?,
            (None, Some(error)) => return Err(error.message),
            (None, None) => return Err("empty handshake answer".to_string()),
        };
        if host_hello.protocol != PROTOCOL_VERSION {
            return Err(format!(
                "the host speaks protocol {}; this client speaks {PROTOCOL_VERSION}",
                host_hello.protocol
            ));
        }

        let this = Arc::new(Self {
            id: HostId::new(host_hello.host.id.clone()),
            hello: host_hello,
            config,
            out,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(2),
            events: Arc::new(HostEventHub::default()),
            senders: StreamSenders::new(Opener::Client),
            attachments: AttachmentReads::default(),
            closed: closed_tx,
            tasks: std::sync::Mutex::new(vec![reader, writer]),
        });
        let demux = {
            let this = Arc::clone(&this);
            tokio::spawn(async move {
                let mut expected_seq: u64 = this.hello.seq + 1;
                while let Some(Some(frame)) = inbound_rx.recv().await {
                    this.on_frame(frame, &mut expected_seq).await;
                }
                this.mark_closed("the host closed the connection").await;
            })
        };
        let pinger = {
            let this = Arc::clone(&this);
            tokio::spawn(async move { this.keepalive().await })
        };
        this.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([demux, pinger]);
        Ok(this)
    }

    /// What the host said about itself.
    pub fn host_hello(&self) -> &HostHello {
        &self.hello
    }

    /// The host's package version and, when its build knew it, the git
    /// revision it was built from.
    pub fn host_version(&self) -> (String, Option<String>) {
        (self.hello.app_version.clone(), self.hello.build.clone())
    }

    /// Resolves with the reason once the connection is gone.
    pub fn closed(&self) -> watch::Receiver<Option<String>> {
        self.closed.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.borrow().is_some()
    }

    /// End the connection now.
    pub async fn close(&self) {
        self.mark_closed("closed by the client").await;
    }

    async fn mark_closed(&self, reason: &str) {
        if self.closed.borrow().is_some() {
            return;
        }
        self.closed.send_replace(Some(reason.to_string()));
        for (_, pending) in self.pending.lock().await.drain() {
            let _ = pending.send(Err(RpcError::host(reason)));
        }
        self.senders.fail_all(reason);
        self.attachments.fail_all(reason);
        let tasks = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for task in tasks {
            task.abort();
        }
    }

    async fn on_frame(&self, frame: Frame, expected_seq: &mut u64) {
        if frame.channel != CONTROL_CHANNEL {
            match Opener::of_channel(frame.channel) {
                // A stream this client is sending on: the host's flow
                // control and its acknowledgement or refusal.
                Opener::Client => match frame.kind {
                    FrameKind::Credit => {
                        if let Some(credit) = decode_credit(&frame.payload) {
                            self.senders.credit(frame.channel, credit);
                        }
                    }
                    FrameKind::Close => self.senders.on_close(frame.channel, &frame.payload),
                    FrameKind::Open | FrameKind::Data => {}
                },
                Opener::Host => {
                    if let Some(reply) = self.attachments.on_frame(&frame)
                        && let Err(error) = self.out.try_send(reply)
                    {
                        self.mark_closed(&error).await;
                    }
                }
            }
            return;
        }
        let message = match rpc::decode(&frame.payload) {
            Ok(message) => message,
            Err(error) => {
                log::debug!("dropping undecodable control frame: {error}");
                return;
            }
        };
        match message {
            Message::Response(response) => {
                if let Some(pending) = self.pending.lock().await.remove(&response.id) {
                    let _ = pending.send(match (response.result, response.error) {
                        (Some(value), _) => Ok(value),
                        (None, Some(error)) => Err(error),
                        (None, None) => Ok(Value::Null),
                    });
                }
            }
            Message::Notification(notification) if notification.method == EVENT_METHOD => {
                match serde_json::from_value::<EventEnvelope>(notification.params) {
                    Ok(envelope) => {
                        if envelope.seq != *expected_seq {
                            log::warn!(
                                "host event sequence jumped from {} to {}; resyncing",
                                *expected_seq,
                                envelope.seq
                            );
                            self.events.publish(HostEvent::Resync);
                        }
                        *expected_seq = envelope.seq + 1;
                        self.events.publish(envelope.event);
                    }
                    Err(error) => log::debug!("dropping undecodable event: {error}"),
                }
            }
            Message::Notification(notification) => {
                log::debug!("ignoring notification {}", notification.method);
            }
            Message::Request(request) => {
                log::debug!("ignoring request {} from the host", request.method);
            }
        }
    }

    async fn keepalive(&self) {
        let mut misses = 0;
        loop {
            tokio::time::sleep(self.config.ping_interval).await;
            if self.is_closed() {
                return;
            }
            let ping = self.call_with_timeout::<HostRequest, Value>(
                &HostRequest::Ping,
                self.config.ping_timeout,
            );
            match ping.await {
                Ok(_) => misses = 0,
                Err(error) => {
                    misses += 1;
                    log::debug!("ping missed ({misses}): {error}");
                    if misses >= self.config.ping_misses {
                        self.mark_closed("the host stopped answering").await;
                        return;
                    }
                }
            }
        }
    }

    async fn call<T: Serialize, R: DeserializeOwned>(&self, request: &T) -> Result<R, String> {
        self.call_with_timeout(request, self.config.request_timeout)
            .await
    }

    async fn call_with_timeout<T: Serialize, R: DeserializeOwned>(
        &self,
        request: &T,
        timeout: Duration,
    ) -> Result<R, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let value = self.call_with_id(id, request, timeout).await?;
        serde_json::from_value(value).map_err(|error| format!("bad answer from the host: {error}"))
    }

    /// Send a request under a caller-chosen id and wait for its answer.
    /// The caller picks the id when it must pair a stream with it.
    async fn call_with_id<T: Serialize>(
        &self,
        id: u64,
        request: &T,
        timeout: Duration,
    ) -> Result<Value, String> {
        if let Some(reason) = self.closed.borrow().clone() {
            return Err(reason);
        }
        let (method, params) = crate::wire::encode_request(request)?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let message = Message::Request(RpcRequest::new(id, method.clone(), params));
        if let Err(error) =
            rpc::encode(&message).and_then(|bytes| self.out.try_send(Frame::control(bytes)))
        {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(error.message),
            Ok(Err(_)) => Err("the connection ended".to_string()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("{method} timed out"))
            }
        }
    }

    /// Stream each image to the host ahead of `run.send`, one after the
    /// other, and return what the request names them by. A failed upload
    /// fails the whole send.
    async fn upload_attachments(
        &self,
        attachments: Vec<AgentImageUpload>,
    ) -> Result<Vec<UploadRef>, String> {
        if attachments.is_empty() {
            return Ok(Vec::new());
        }
        if !has_feature(&self.hello.features, UPLOAD_STREAMS_FEATURE) {
            return Err("this host cannot receive image attachments; update the host".to_string());
        }
        let mut uploads = Vec::with_capacity(attachments.len());
        for AgentImageUpload { name, data_url } in attachments {
            // Decode once and let the data URL go, so only the bytes
            // stay in memory while they stream.
            let (mime, bytes) = decode_data_url(&data_url)?;
            drop(data_url);
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err(format!("{name} is too large (max 10MB)"));
            }
            let upload_id = uuid::Uuid::new_v4().to_string();
            let mut sender = self.senders.open(
                &self.out,
                StreamOpen {
                    purpose: UPLOAD_PURPOSE.to_string(),
                    request_id: None,
                    upload_id: Some(upload_id.clone()),
                    mime: Some(mime),
                    len: Some(bytes.len() as u64),
                },
            )?;
            let bytes = &bytes;
            let transfer = async move {
                sender.send_all(bytes).await?;
                sender.wait_for_ack().await
            };
            tokio::time::timeout(self.config.long_request_timeout, transfer)
                .await
                .map_err(|_| format!("uploading {name} timed out"))?
                .map_err(|error| format!("uploading {name} failed: {error}"))?;
            uploads.push(UploadRef { upload_id, name });
        }
        Ok(uploads)
    }

    /// Fetch every page of a snapshot's timeline. `expected_len` is the
    /// host's word and only sizes the first allocation, within reason.
    async fn page_timeline(
        &self,
        session_id: &str,
        expected_len: usize,
    ) -> Result<Vec<maple_agent::agent::AgentTimelineItem>, String> {
        let mut items = Vec::with_capacity(expected_len.min(MAX_PREALLOCATED_ITEMS));
        loop {
            let page: TimelinePage = self
                .call(&SessionRequest::Timeline {
                    session_id: session_id.to_string(),
                    offset: items.len(),
                    limit: self.config.timeline_page_items,
                })
                .await?;
            let received = page.items.len();
            items.extend(page.items);
            if !page.has_more || received == 0 {
                break;
            }
        }
        Ok(items)
    }
}

#[async_trait]
impl HostBackend for RemoteHostBackend {
    fn id(&self) -> &HostId {
        &self.id
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent> {
        self.events.subscribe()
    }

    async fn bootstrap(&self) -> Result<HostBootstrap, String> {
        let snapshot: BootstrapSnapshot = self.call(&HostRequest::Bootstrap).await?;
        let mut bootstrap = snapshot.bootstrap;
        if let Some(latest) = bootstrap.latest.as_mut() {
            latest.timeline = self
                .page_timeline(&latest.session.id, snapshot.latest_timeline_len)
                .await?;
        }
        Ok(bootstrap)
    }

    async fn start_runtime(
        &self,
        request: Option<AgentStartRequest>,
    ) -> Result<AgentRuntimeStatus, String> {
        self.call_with_timeout(
            &HostRequest::StartRuntime { request },
            self.config.long_request_timeout,
        )
        .await
    }

    async fn stop_runtime(&self) -> Result<AgentRuntimeStatus, String> {
        self.call(&HostRequest::StopRuntime).await
    }

    async fn recent_project_roots(&self) -> Result<Vec<RecentProjectRoot>, String> {
        self.call(&ProjectRequest::RecentRoots).await
    }

    async fn select_project_root(
        &self,
        path: String,
    ) -> Result<AgentProjectRootRegistration, String> {
        self.call(&ProjectRequest::SelectRoot { path }).await
    }

    async fn remove_project_root(
        &self,
        path: String,
        fallback: Option<String>,
    ) -> Result<(), String> {
        self.call(&ProjectRequest::RemoveRoot { path, fallback })
            .await
    }

    async fn suggest_directories(&self, query: String) -> Result<Vec<DirectorySuggestion>, String> {
        self.call(&ProjectRequest::SuggestDirectories { query })
            .await
    }

    async fn watch_project_root(&self, path: String) -> Result<(), String> {
        self.call(&ProjectRequest::Watch { path }).await
    }

    async fn unwatch_project_root(&self, path: String) -> Result<(), String> {
        self.call(&ProjectRequest::Unwatch { path }).await
    }

    async fn project_trust(&self, path: String) -> Result<AgentProjectTrustStatus, String> {
        self.call(&ProjectRequest::Trust { path }).await
    }

    async fn set_project_trust(
        &self,
        path: String,
        trusted: bool,
    ) -> Result<AgentProjectTrustStatus, String> {
        self.call(&ProjectRequest::SetTrust { path, trusted }).await
    }

    async fn list_sessions(
        &self,
        project_root: Option<String>,
    ) -> Result<Vec<AgentSessionSummary>, String> {
        self.call(&SessionRequest::List { project_root }).await
    }

    async fn create_session(
        &self,
        request: Option<AgentCreateSessionRequest>,
    ) -> Result<AgentSessionDetail, String> {
        self.call(&SessionRequest::Create { request }).await
    }

    async fn load_session(&self, session_id: String) -> Result<AgentSessionDetail, String> {
        let snapshot: SessionSnapshot = self
            .call(&SessionRequest::Load {
                session_id: session_id.clone(),
            })
            .await?;
        let mut detail = snapshot.detail;
        detail.timeline = self
            .page_timeline(&session_id, snapshot.timeline_len)
            .await?;
        Ok(detail)
    }

    async fn rename_session(
        &self,
        session_id: String,
        title: String,
    ) -> Result<AgentSessionSummary, String> {
        self.call(&SessionRequest::Rename { session_id, title })
            .await
    }

    async fn set_session_state(
        &self,
        session_id: String,
        state: AgentTaskState,
    ) -> Result<AgentSessionSummary, String> {
        self.call(&SessionRequest::SetState { session_id, state })
            .await
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        self.call(&SessionRequest::Delete { session_id }).await
    }

    async fn compact_session(&self, session_id: String) -> Result<(), String> {
        self.call(&SessionRequest::Compact { session_id }).await
    }

    async fn session_subagents(&self, session_id: String) -> Result<Vec<AgentSubagent>, String> {
        self.call(&SessionRequest::Subagents { session_id }).await
    }

    async fn cancel_external_agent(
        &self,
        session_id: String,
        agent_id: String,
    ) -> Result<(), String> {
        self.call(&SessionRequest::CancelExternalAgent {
            session_id,
            agent_id,
        })
        .await
    }

    async fn set_permission_mode(&self, session_id: String, mode: String) -> Result<(), String> {
        self.call(&SessionRequest::SetPermissionMode { session_id, mode })
            .await
    }

    async fn set_session_web_enabled(
        &self,
        session_id: String,
        enabled: bool,
    ) -> Result<AgentSessionSummary, String> {
        self.call(&SessionRequest::SetWebEnabled {
            session_id,
            enabled,
        })
        .await
    }

    async fn context_usage(
        &self,
        session_id: String,
        model: Option<String>,
    ) -> Result<Option<ContextUsage>, String> {
        self.call(&HostRequest::ContextUsage { session_id, model })
            .await
    }

    async fn read_image_attachment(
        &self,
        session_id: String,
        attachment_id: String,
    ) -> Result<Vec<u8>, String> {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = SessionRequest::ReadAttachment {
            session_id,
            attachment_id,
        };
        // The open frame precedes the answer on the same ordered carrier,
        // so the waiter must exist before the request goes out.
        let receiver = self.attachments.expect(request_id);
        // The host may have opened the stream before its answer failed or
        // the wait ran out; whatever it opened for this request goes too.
        let value = match self
            .call_with_id(request_id, &request, self.config.request_timeout)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                self.attachments.abandon(request_id);
                return Err(error);
            }
        };
        let handle: AttachmentHandle =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        match tokio::time::timeout(self.config.request_timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("attachment transfer was cut off".to_string()),
            Err(_) => {
                self.attachments.abandon(request_id);
                Err(format!(
                    "attachment transfer on stream {} timed out",
                    handle.stream
                ))
            }
        }
    }

    async fn send_message(&self, mut request: AgentSendMessageRequest) -> Result<String, String> {
        let attachments = std::mem::take(&mut request.attachments);
        let uploads = self.upload_attachments(attachments).await?;
        self.call(&RunRequest::Send { request, uploads }).await
    }

    async fn cancel_run(&self, run_id: String) -> Result<(), String> {
        self.call(&RunRequest::Cancel { run_id }).await
    }

    async fn cancel_queued_message(
        &self,
        session_id: String,
        queue_id: String,
    ) -> Result<AgentDesktopQueueSnapshot, String> {
        self.call(&RunRequest::CancelQueued {
            session_id,
            queue_id,
        })
        .await
    }

    async fn begin_queued_message_edit(
        &self,
        session_id: String,
        queue_id: String,
    ) -> Result<(), String> {
        self.call(&RunRequest::BeginQueuedEdit {
            session_id,
            queue_id,
        })
        .await
    }

    async fn end_queued_message_edit(
        &self,
        session_id: String,
        queue_id: String,
    ) -> Result<(), String> {
        self.call(&RunRequest::EndQueuedEdit {
            session_id,
            queue_id,
        })
        .await
    }

    async fn answer_question(&self, request_id: String, answer: String) -> Result<bool, String> {
        self.call(&RunRequest::AnswerQuestion { request_id, answer })
            .await
    }

    async fn permission_respond(
        &self,
        session_id: String,
        request_id: String,
        allow: bool,
    ) -> Result<(), String> {
        self.call(&RunRequest::PermissionRespond {
            session_id,
            request_id,
            allow,
        })
        .await
    }

    async fn ask_side_question(
        &self,
        session_id: String,
        request_id: String,
        prior: Vec<SideQuestionTurn>,
        question: String,
    ) -> Result<(), String> {
        self.call(&RunRequest::AskSideQuestion {
            session_id,
            request_id,
            prior,
            question,
        })
        .await
    }

    async fn summarize_tool_call(
        &self,
        session_id: String,
        tool_name: String,
        input: Option<Value>,
        output_text: String,
    ) -> Result<Option<String>, String> {
        self.call(&RunRequest::SummarizeToolCall {
            session_id,
            tool_name,
            input,
            output_text,
        })
        .await
    }

    async fn summarize_thinking(
        &self,
        session_id: String,
        thinking_text: String,
    ) -> Result<Option<String>, String> {
        self.call(&RunRequest::SummarizeThinking {
            session_id,
            thinking_text,
        })
        .await
    }

    async fn tool_summaries(&self, session_id: String) -> Result<HashMap<String, String>, String> {
        self.call(&HostRequest::ToolSummaries { session_id }).await
    }

    async fn store_tool_summary(
        &self,
        session_id: String,
        item_id: String,
        summary: String,
    ) -> Result<(), String> {
        self.call(&HostRequest::StoreToolSummary {
            session_id,
            item_id,
            summary,
        })
        .await
    }

    async fn available_model_ids(&self) -> Result<Vec<String>, String> {
        self.call(&ModelRequest::List).await
    }

    async fn model_supports_vision(&self, model: String) -> Result<Option<bool>, String> {
        self.call(&ModelRequest::SupportsVision { model }).await
    }

    async fn list_slash_commands(
        &self,
        working_dir: Option<String>,
    ) -> Result<Vec<AgentSlashCommand>, String> {
        self.call(&ModelRequest::SlashCommands { working_dir })
            .await
    }

    async fn resolve_slash_command(
        &self,
        working_dir: Option<String>,
        command: String,
        args: String,
    ) -> Result<Option<String>, String> {
        self.call(&ModelRequest::ResolveSlashCommand {
            working_dir,
            command,
            args,
        })
        .await
    }

    async fn list_session_mcp_servers(
        &self,
        session_id: String,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        self.call(&IntegrationRequest::ListSessionMcp { session_id })
            .await
    }

    async fn set_session_mcp_server_enabled(
        &self,
        session_id: String,
        name: String,
        kind: AgentSessionIntegrationKind,
        enabled: bool,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        self.call(&IntegrationRequest::SetSessionMcp {
            session_id,
            name,
            kind,
            enabled,
        })
        .await
    }

    async fn list_mcp_servers(&self) -> Result<Vec<AgentMcpServer>, String> {
        self.call(&IntegrationRequest::ListMcp).await
    }

    async fn save_mcp_servers(
        &self,
        servers: Vec<AgentMcpServer>,
    ) -> Result<Vec<AgentMcpServer>, String> {
        self.call(&IntegrationRequest::SaveMcp { servers }).await
    }

    async fn list_integrations(&self) -> Result<Vec<AgentIntegration>, String> {
        self.call(&IntegrationRequest::List).await
    }

    async fn set_integration_enabled(
        &self,
        id: String,
        enabled: bool,
    ) -> Result<Vec<AgentIntegration>, String> {
        self.call(&IntegrationRequest::SetEnabled { id, enabled })
            .await
    }

    /// The permission flow behind a setup runs on the host's own screen;
    /// there is no wire method for it, so answer here.
    async fn setup_integration(&self, _id: String) -> Result<Vec<AgentIntegration>, String> {
        Err(SETUP_IS_LOCAL.to_string())
    }

    async fn session_defaults(&self) -> Result<HostSessionDefaults, String> {
        self.call(&HostRequest::SessionDefaults).await
    }

    async fn set_session_defaults(&self, defaults: HostSessionDefaults) -> Result<(), String> {
        self.call(&HostRequest::SetSessionDefaults { defaults })
            .await
    }

    async fn save_default_model(&self, model: String) -> Result<(), String> {
        self.call(&HostRequest::SaveDefaultModel { model }).await
    }

    async fn usage_summary(&self) -> Result<UsageSummary, String> {
        self.call(&HostRequest::UsageSummary).await
    }
}

/// The media type and bytes of a `data:<mime>;base64,<data>` URL, the
/// shape the composer builds and the runtime stores.
fn decode_data_url(data_url: &str) -> Result<(String, Vec<u8>), String> {
    use base64::Engine as _;
    let (header, data) = data_url
        .split_once(',')
        .ok_or_else(|| "Image attachment must be a base64 data URL".to_string())?;
    let mime = header
        .strip_prefix("data:")
        .and_then(|value| value.strip_suffix(";base64"))
        .filter(|mime| !mime.is_empty())
        .ok_or_else(|| "Image attachment must be a base64 data URL".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| "Image attachment is not valid base64".to_string())?;
    if bytes.is_empty() {
        return Err("Image attachment cannot be empty".to_string());
    }
    Ok((mime.to_string(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_urls_decode_to_their_type_and_bytes() {
        let (mime, bytes) = decode_data_url("data:image/png;base64,AQID").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, [1, 2, 3]);
        for bad in [
            "AQID",
            "http://x,AQID",
            "data:image/png,AQID",
            "data:;base64,AQID",
            "data:image/png;base64,",
            "data:image/png;base64,!!",
        ] {
            assert!(decode_data_url(bad).is_err(), "{bad}");
        }
    }
}
