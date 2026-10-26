//! Paired control and the A-daemon bridge. No prompt is queued or replayed by
//! this transport; accepted sends and their receipts belong exclusively to B.
use super::{
    manager::{Manager, Node},
    streams,
    update::{
        BlockedReason, InstallCommand, PeerInfo, ProtocolInfo, RemoteInstallAuthority,
        RemoteUpdateStatus, RunningApplication,
    },
    wire,
};
use crate::host::protocol::{self, Message, Request};
use crate::host::subscription::{SubscribeRequest, Subscription};
use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
};
use maple_agent::agent::AgentServiceEvent;
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::mpsc;

async fn read_control_ack(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<(), String> {
    let accepted: Result<(), wire::Refusal> = tokio::time::timeout(
        Duration::from_secs(10),
        wire::read(reader, wire::MAX_PREFACE),
    )
    .await
    .map_err(|_| "Remote handshake timed out")?
    .map_err(|error| match error {
        wire::Error::InvalidFrame | wire::Error::Incompatible => {
            format!("Remote access ended: {error}")
        }
        _ => error.to_string(),
    })?;
    accepted.map_err(wire::Refusal::into_error)
}

pub(super) fn listen(node: Arc<Node>) {
    tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = node.stopped.cancelled() => break,
                _ = connections.join_next(), if !connections.is_empty() => {},
                incoming = node.endpoint.accept() => {
                    let Some(incoming) = incoming else { break; };
                    let Ok(admission_slot) = node.admissions.clone().try_acquire_owned() else { incoming.refuse(); continue; };
                    let node = node.clone();
                    connections.spawn(async move {
                        let accepted = tokio::time::timeout(Duration::from_secs(15), async {
                            let connection = incoming.await.map_err(|_| "Remote handshake failed")?;
                            let peer = connection.remote_id();
                            let (mut tx, mut rx) = connection.accept_bi().await.map_err(|_| "Remote handshake closed")?;
                            let outcome = async {
                            let hello = wire::read_hello(&mut rx).await?;
                            node.ensure_valid().await?;
                            if hello.account != node.account || hello.project != node.project || hello.deployment != node.deployment || hello.channel != crate::profile::PROFILE {
                                return Err(wire::Refusal::terminal("Remote account or deployment mismatch"));
                            }
                            match hello.mode {
                                wire::Mode::Pair { window, admission } => {
                                    node.accept_pairing(peer, window, admission, &mut tx, &mut rx).await?;
                                    let _ = tx.finish();
                                    let _ = tokio::time::timeout(Duration::from_secs(3), tx.stopped()).await;
                                    let names = node.clone();
                                    tokio::spawn(async move { names.refresh_peer_names().await; });
                                    Ok(None)
                                }
                                wire::Mode::ConfirmGrant { window } => {
                                    let answer = {
                                        let store = node.store.read().unwrap();
                                        if store.peers().get(&peer.to_string()).is_some_and(|p| p.controls_this_computer) && store.peers().get(&peer.to_string()).is_some_and(|p| p.grant == window) {
                                            Ok(window)
                                        } else { Err(wire::Refusal::terminal("No current grant matches this pairing")) }
                                    };
                                    wire::write(&mut tx, &answer, wire::MAX_PREFACE).await.map_err(|e| e.to_string())?;
                                    let _ = tx.finish();
                                    let _ = tokio::time::timeout(Duration::from_secs(3), tx.stopped()).await;
                                    Ok(None)
                                }
                                wire::Mode::Control => {
                                    if !node.store.read().unwrap().permits(peer) { return Err(wire::Refusal::terminal("Remote control grant is missing or revoked")); }
                                    let slot = node.controllers.clone().try_acquire_owned().map_err(|_| "Remote connection limit reached")?;
                                    wire::write(&mut tx, &Ok::<(), wire::Refusal>(()), wire::MAX_PREFACE).await.map_err(|e| e.to_string())?;
                                    Ok(Some(slot))
                                }
                            }
                            }.await;
                            // Return a terminal refusal while the stream is
                            // still alive, so a revoked/incompatible peer does
                            // not retry it as a transient network outage.
                            if let Err(error) = &outcome {
                                let _ = wire::write(&mut tx, &Err::<(), _>(error), wire::MAX_PREFACE).await;
                                let _ = tx.finish();
                                let _ = tokio::time::timeout(Duration::from_secs(2), tx.stopped()).await;
                            }
                            outcome.map(|slot| slot.map(|slot| (connection, peer, tx, rx, slot)))
                        }).await;
                        drop(admission_slot);
                        if let Ok(Err(error)) = &accepted { log::debug!("Remote handshake declined: {error}"); }
                        if accepted.is_err() { log::debug!("Remote handshake timed out"); }
                        if let Ok(Ok(Some((connection, peer, tx, rx, _slot)))) = accepted {
                            serve(node, peer, connection, tx, rx).await;
                        }
                    });
                }
            }
        }
        connections.abort_all();
    });
}

async fn serve(
    node: Arc<Node>,
    peer: EndpointId,
    connection: Connection,
    mut tx: SendStream,
    _rx: RecvStream,
) {
    let _questions = node.backend.question_responder();
    let mut events = node.events.subscribe();
    let mut revoked = node.revoked.subscribe();
    let capacity = Arc::new(streams::Capacity::new());
    let subscription = Arc::new(RwLock::new(Subscription::default()));
    let event_filter = subscription.clone();
    let mut requests = tokio::task::JoinSet::new();
    let request_node = node.clone();
    let request_connection = connection.clone();
    requests.spawn(async move {
        streams::serve_requests(request_connection, move |tx, rx| {
            let node = request_node.clone();
            let capacity = capacity.clone();
            let subscription = subscription.clone();
            async move {
                if let Err(error) = serve_request(node, peer, tx, rx, capacity, subscription).await
                {
                    log::debug!("Remote request stream ended: {error}");
                }
            }
        })
        .await;
    });
    // A slow event consumer cannot hold the request accept/dispatch loop.
    let event_node = node.clone();
    let _ = tx.set_priority(1);
    let mut event_writer = tokio::task::JoinSet::new();
    event_writer.spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    event_node.ensure_valid().await?;
                    send(&mut tx, &Message::Resync).await?;
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            };
            event_node.ensure_valid().await?;
            if !event_node.store.read().unwrap().permits(peer)
                || matches!(&*event, AgentServiceEvent::AccountChanged(_))
            {
                return Ok::<(), String>(());
            }
            if !event_belongs_to_account(&event, &event_node.account) {
                continue;
            }
            let allowed = event_filter
                .read()
                .map_err(|_| "Remote subscription unavailable")?
                .allows(&event);
            if remote_event(&event) && allowed {
                send_event(&mut tx, &event).await?;
            }
        }
    });
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = node.stopped.cancelled() => break,
            _ = connection.closed() => break,
            _ = event_writer.join_next() => break,
            _ = tick.tick() => {
                if !node.valid().await || !node.store.read().unwrap().permits(peer) { break; }
            }
            event = revoked.recv() => {
                if event.is_err() || event.ok().is_some_and(|key| key == peer || key == node.endpoint.id()) { break; }
            }
            _ = requests.join_next() => break,
        }
    }
    event_writer.abort_all();
    connection.close(0u32.into(), b"remote stream closed");
    // Closing streams stops incomplete transfers. An already admitted mutation
    // still finishes on B, as with local IPC; A never resends it.
    while requests.join_next().await.is_some() {}
}

async fn serve_request(
    node: Arc<Node>,
    peer: EndpointId,
    mut tx: SendStream,
    mut rx: RecvStream,
    capacity: Arc<streams::Capacity>,
    subscription: Arc<RwLock<Subscription>>,
) -> Result<(), String> {
    let header = match capacity.read_header(&mut rx).await {
        Ok(header) => header,
        Err(reason) => return Err(reason),
    };
    tx.set_priority(header.method.priority())
        .map_err(|_| "Remote reply closed")?;
    let _slot = match capacity.acquire(header.method) {
        Ok(slot) => slot,
        Err(error) => {
            if header.method == wire::Method::UploadAttachment {
                return super::uploads::refuse(&mut tx, error).await;
            }
            return streams::reply(
                &mut tx,
                &Message::Response {
                    id: header.id,
                    result: Err(error),
                },
            )
            .await;
        }
    };
    let id = header.id;
    let method = header.method;
    if method == wire::Method::UploadAttachment {
        if header.send_id.is_some() {
            return super::uploads::refuse(
                &mut tx,
                "Attachment uploads cannot carry a prompt receipt".into(),
            )
            .await;
        }
        return super::uploads::serve(node, peer, id, &mut tx, &mut rx).await;
    }
    let request = match header.body(&mut rx).await {
        Ok(request) => request,
        Err(reason) => {
            return streams::reply(
                &mut tx,
                &Message::Response {
                    id,
                    result: Err(reason),
                },
            )
            .await;
        }
    };
    // Recheck after the potentially slow upload, before any backend effect.
    node.ensure_valid().await?;
    if !node.store.read().unwrap().permits(peer) {
        return Err("Remote access revoked".into());
    }
    if request.method == wire::Method::RemoteUpdateInstall {
        return serve_install(node, peer, &mut tx, request).await;
    }
    let id = request.id;
    let result = if matches!(
        request.method,
        wire::Method::PeerInfo | wire::Method::RemoteUpdateRefresh
    ) {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Empty {}
        if request.send_id.is_some() || serde_json::from_value::<Empty>(request.params).is_err() {
            Err("Invalid remote information request".into())
        } else {
            let update = if request.method == wire::Method::RemoteUpdateRefresh {
                // This response is tied to a new check claimed by the current
                // execution desktop, never to its cached status heartbeat.
                node.backend
                    .remote_updates
                    .request_refresh(node.account.clone(), peer.to_string())
                    .await
            } else {
                node.backend
                    .remote_updates
                    .status(&node.account, &peer.to_string())
                    .await
            };
            if request.method == wire::Method::RemoteUpdateRefresh {
                match update {
                    Ok(status) => serde_json::to_value(peer_information(Ok(status))?)
                        .map_err(|_| "Remote information unavailable".into()),
                    Err(error) => Err(error),
                }
            } else {
                serde_json::to_value(peer_information(update)?)
                    .map_err(|_| "Remote information unavailable".into())
            }
        }
    } else if request.method == wire::Method::Subscribe {
        set_subscription(&subscription, &node.account, request.params)
    } else {
        crate::host::dispatch_remote_with_receipt(
            &node.backend,
            Request {
                id,
                method: if request.method == wire::Method::TaskBootstrap {
                    "start_runtime".into()
                } else {
                    request.method.name()
                },
                send_id: request.send_id,
                params: if request.method == wire::Method::TaskBootstrap {
                    serde_json::json!([node.account, null])
                } else {
                    request.params
                },
            },
            node.receipts.clone(),
            &node.session,
            &node.account,
        )
        .await
    };
    if node.valid().await && node.store.read().unwrap().permits(peer) {
        let result = streams::encode_result(method, result);
        streams::reply_with_limit(
            &mut tx,
            &Message::Response { id, result },
            method.reply_limit(),
        )
        .await?;
    }
    Ok(())
}

fn peer_information(update: Result<RemoteUpdateStatus, String>) -> Result<PeerInfo, String> {
    // A damaged/unavailable receipt journal removes installation authority,
    // not the independently baked identity. Never expose its private error.
    let info = PeerInfo {
        protocol: ProtocolInfo::current(),
        running: RunningApplication::current(),
        update: update.unwrap_or_else(|_| RemoteUpdateStatus {
            blocked: Some(BlockedReason::CoordinatorUnavailable),
            ..Default::default()
        }),
    };
    info.validate()?;
    Ok(info)
}

/// A response is not permission to stop until QUIC confirms its delivery.
/// Dropping this guard on cancellation or failed delivery leaves no queued job.
struct PendingInstallReply {
    broker: Arc<crate::remote_updates::Broker>,
    authority: RemoteInstallAuthority,
    operation_id: String,
    completed: bool,
}

impl Drop for PendingInstallReply {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let broker = self.broker.clone();
        let authority = self.authority.clone();
        let operation_id = self.operation_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = broker.cancel_pending(&authority, &operation_id).await;
            });
        }
    }
}

async fn serve_install(
    node: Arc<Node>,
    peer: EndpointId,
    tx: &mut SendStream,
    request: wire::Command,
) -> Result<(), String> {
    let id = request.id;
    let command: Result<InstallCommand, _> = serde_json::from_value(request.params);
    let command = match command {
        Ok(command) if request.send_id.is_none() => command,
        _ => {
            return streams::reply_with_limit(
                tx,
                &Message::Response {
                    id,
                    result: Err("Invalid remote update request".into()),
                },
                wire::MAX_MANAGEMENT_REPLY,
            )
            .await;
        }
    };
    // These refusals precede Broker admission. Reply explicitly so the client
    // does not confuse a known rejection with a lost installation receipt.
    if command.request.validate().is_err() {
        return install_refused(tx, id, "Invalid remote update request").await;
    }
    let authority = match RemoteInstallAuthority::capture(&node, peer, command.expected_grant) {
        Ok(authority) => authority,
        Err(_) => {
            return install_refused(
                tx,
                id,
                "Remote update authorization changed. Reopen this computer.",
            )
            .await;
        }
    };
    if authority.ensure_current().await.is_err() {
        return install_refused(
            tx,
            id,
            "Remote update authorization changed. Reopen this computer.",
        )
        .await;
    }
    let mut pending = PendingInstallReply {
        broker: node.backend.remote_updates.clone(),
        authority,
        operation_id: command.request.operation_id.clone(),
        completed: false,
    };
    let accepted = pending
        .broker
        .request_install(pending.authority.clone(), command.request)
        .await;
    let admitted = accepted.is_ok();
    if admitted {
        pending.authority.ensure_current().await?;
    }
    let result = match accepted {
        Ok(value) => {
            serde_json::to_value(value).map_err(|_| "Remote update receipt unavailable".into())
        }
        Err(error) => Err(if error.contains("in progress") || error.contains("busy") {
            "The remote computer is busy".into()
        } else if error.contains("ready") || error.contains("Open Maple") {
            "The remote computer is not ready to install this update".into()
        } else {
            "The remote computer refused the update before accepting it".into()
        }),
    };
    streams::reply_with_limit(
        tx,
        &Message::Response { id, result },
        wire::MAX_MANAGEMENT_REPLY,
    )
    .await?;
    if admitted {
        pending.authority.ensure_current().await?;
        pending
            .broker
            .acknowledge(&pending.authority, &pending.operation_id)
            .await?;
        pending.completed = true;
    }
    Ok(())
}

async fn install_refused(
    tx: &mut SendStream,
    id: u64,
    message: &'static str,
) -> Result<(), String> {
    streams::reply_with_limit(
        tx,
        &Message::Response {
            id,
            result: Err(message.to_owned()),
        },
        wire::MAX_MANAGEMENT_REPLY,
    )
    .await
}

fn set_subscription(
    subscription: &RwLock<Subscription>,
    account: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (requested_account, requested): (String, SubscribeRequest) =
        serde_json::from_value(params).map_err(|_| "Invalid remote subscription")?;
    if requested_account != account {
        return Err("Invalid remote subscription".into());
    }
    subscription
        .write()
        .map_err(|_| "Remote subscription unavailable")?
        .set(requested)
        .map(|()| serde_json::Value::Null)
}

fn event_belongs_to_account(event: &AgentServiceEvent, account: &str) -> bool {
    match event {
        AgentServiceEvent::ProjectsChanged { user_id, .. }
        | AgentServiceEvent::ProjectRemoved { user_id, .. }
        | AgentServiceEvent::HistoryReplaced { user_id, .. } => user_id == account,
        _ => true,
    }
}

pub(super) fn remote_event(event: &AgentServiceEvent) -> bool {
    // Exhaustive on purpose: new local events need an explicit remote decision.
    match event {
        AgentServiceEvent::RuntimeStatus(_)
        | AgentServiceEvent::ProjectsChanged { .. }
        | AgentServiceEvent::ProjectRemoved { .. }
        | AgentServiceEvent::Question { .. }
        | AgentServiceEvent::QuestionResolved { .. }
        | AgentServiceEvent::ThinkingChanged { .. }
        | AgentServiceEvent::SessionCreated(_)
        | AgentServiceEvent::SessionUpdated { .. }
        | AgentServiceEvent::TimelineItem { .. }
        | AgentServiceEvent::Run { .. }
        | AgentServiceEvent::SideQuestion { .. } => true,
        AgentServiceEvent::HistoryReplaced { .. }
        | AgentServiceEvent::PersistenceFailureChanged { .. } => true,
        AgentServiceEvent::StoreStateChanged { .. }
        | AgentServiceEvent::AccountChanged(_)
        | AgentServiceEvent::RemoteAccessEnded(_)
        | AgentServiceEvent::HostReconnecting
        | AgentServiceEvent::HostReconnected
        | AgentServiceEvent::HostDisconnected => false,
    }
}

async fn send(tx: &mut SendStream, message: &Message) -> Result<(), String> {
    tokio::time::timeout(
        Duration::from_secs(10),
        wire::write(tx, message, wire::MAX_FRAME),
    )
    .await
    .map_err(|_| "Remote consumer stopped reading")?
    .map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
enum BorrowedEvent<'a> {
    Event(&'a AgentServiceEvent),
}

fn encode_event(event: &AgentServiceEvent) -> Result<zeroize::Zeroizing<Vec<u8>>, String> {
    const EVENT_LIMIT: usize = 4 * 1024 * 1024;
    match crate::framing::encode(&BorrowedEvent::Event(event), EVENT_LIMIT) {
        Ok(bytes) => Ok(bytes),
        Err(crate::framing::EncodeError::TooLarge) => {
            crate::framing::encode(&Message::Resync, EVENT_LIMIT)
                .map_err(|_| "Could not encode remote resync".into())
        }
        Err(crate::framing::EncodeError::Invalid) => Err("Could not encode remote event".into()),
    }
}

async fn send_event(tx: &mut SendStream, event: &AgentServiceEvent) -> Result<(), String> {
    let bytes = encode_event(event)?;
    tokio::time::timeout(Duration::from_secs(10), wire::write_encoded(tx, &bytes))
        .await
        .map_err(|_| "Remote consumer stopped reading")?
        .map_err(|error| error.to_string())
}

pub struct Link {
    pub node: Arc<Node>,
    pub peer: EndpointId,
    pub connection: Connection,
    pub rx: RecvStream,
}

impl Manager {
    pub async fn connect(self: &Arc<Self>, peer: &str) -> Result<Link, String> {
        let peer: EndpointId = peer
            .parse()
            .map_err(|_| "Invalid remote computer identity")?;
        let node = self.active().await?;
        if !node
            .store
            .read()
            .unwrap()
            .peers()
            .get(&peer.to_string())
            .is_some_and(|p| p.can_connect)
        {
            return Err("Remote access ended: pair this computer before connecting".into());
        }
        let (address, _) = node.destination(peer).await?;
        let connection = tokio::time::timeout(
            Duration::from_secs(10),
            node.endpoint.connect(address, wire::ALPN),
        )
        .await
        .map_err(|_| "Remote computer is not reachable")?
        .map_err(|_| "Remote computer is not reachable")?;
        if connection.remote_id() != peer {
            return Err("Remote identity changed".into());
        }
        let (mut tx, mut rx) = connection
            .open_bi()
            .await
            .map_err(|_| "Remote connection closed")?;
        wire::write(&mut tx, &node.hello(wire::Mode::Control), wire::MAX_PREFACE)
            .await
            .map_err(|e| e.to_string())?;
        read_control_ack(&mut rx).await?;
        node.ensure_valid().await?;
        tx.finish().map_err(|_| "Remote handshake closed")?;
        Ok(Link {
            node,
            peer,
            connection,
            rx,
        })
    }
}

pub async fn bridge(
    connection: crate::host::ipc::Connection,
    mut link: Link,
) -> Result<bool, String> {
    let folder_grant = link
        .node
        .store
        .read()
        .map_err(|_| "Remote grant unavailable")?
        .peers()
        .get(&link.peer.to_string())
        .filter(|peer| peer.can_connect)
        .map(|peer| peer.grant)
        .ok_or("Remote access ended: pair this computer before connecting")?;
    let client = Arc::new(streams::Client::new(link.connection.clone()));
    let (mut local_rx, mut local_tx) = tokio::io::split(connection);
    let (frames, mut frame_rx) = mpsc::channel(16);
    let mut revoked = link.node.revoked.subscribe();
    let mut readers = tokio::task::JoinSet::new();
    readers.spawn(async move {
        loop {
            let message = match wire::read::<Message>(&mut link.rx, wire::MAX_FRAME).await {
                Ok(Message::Event(event)) if !remote_event(&event) => Message::Closed {
                    reason: "Unexpected remote event; update both computers".into(),
                },
                Ok(message @ (Message::Event(_) | Message::Closed { .. } | Message::Resync)) => {
                    message
                }
                Ok(Message::Response { .. }) => Message::Closed {
                    reason: "Unexpected reply on the remote event stream".into(),
                },
                Err(wire::Error::Disconnected) => break,
                Err(error) => Message::Closed {
                    reason: error.to_string(),
                },
            };
            let terminal = matches!(message, Message::Closed { .. });
            if frames.send(message).await.is_err() || terminal {
                break;
            }
        }
    });
    let (requests, mut request_rx) = mpsc::channel(1);
    readers.spawn(async move {
        while let Ok(request) = protocol::read::<Request>(&mut local_rx).await {
            if requests.send(request).await.is_err() {
                break;
            }
        }
    });
    let capacity = streams::Capacity::new();
    let mut calls = tokio::task::JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let result = async {
        loop {
            tokio::select! {
                _ = link.node.stopped.cancelled() => return Ok(false),
                _ = link.connection.closed() => return Ok(false),
                _ = tick.tick() => link.node.ensure_valid().await?,
                change = revoked.recv() => {
                    if change.is_err() || change.ok() == Some(link.peer) { return Ok(false); }
                }
                Some(completed) = calls.join_next(), if !calls.is_empty() => {
                    let frame = completed.map_err(|_| "Remote request interrupted")?;
                    link.node.ensure_valid().await?;
                    let terminal = matches!(frame, Message::Closed { .. });
                    local_send(&mut local_tx, &frame).await?;
                    if terminal { return Ok(true); }
                }
                frame = frame_rx.recv() => {
                    let Some(frame) = frame else { return Ok(false); };
                    link.node.ensure_valid().await?;
                    let terminal = matches!(frame, Message::Closed { .. });
                    local_send(&mut local_tx, &frame).await?;
                    if terminal { return Ok(true); }
                }
                request = request_rx.recv() => {
                    let Some(request) = request else { return Ok(true); };
                    if request.method == "detach" {
                        local_send(&mut local_tx, &Message::Response { id: request.id, result: Ok(serde_json::Value::Null) }).await?;
                        return Ok(true);
                    }
                    let selected = match request.method.as_str() {
                        "start_runtime" => Ok(wire::Method::TaskBootstrap),
                        "import_attachment" => Ok(wire::Method::UploadAttachment),
                        // Binary upload is selected only by the local import
                        // bridge. Remote metadata contains no local source path.
                        "upload_attachment" => Err("Use the local attachment importer".into()),
                        _ => wire::Method::parse(&request.method),
                    };
                    let method = match selected {
                        Ok(method) => method,
                        Err(error) => { local_send(&mut local_tx, &Message::Response { id: request.id, result: Err(error) }).await?; continue; }
                    };
                    link.node.ensure_valid().await?;
                    let slot = match capacity.acquire(method) {
                        Ok(slot) => slot,
                        Err(error) => {
                            local_send(&mut local_tx, &Message::Response { id: request.id, result: Err(error) }).await?;
                            continue;
                        }
                    };
                    let client = client.clone();
                    let node = link.node.clone();
                    let peer = link.peer;
                    calls.spawn(async move {
                        let _slot = slot;
                        let id = request.id;
                        let send_id = request.send_id.clone();
                        let authorize_folders = async {
                            node.ensure_valid().await?;
                            if !node.store.read().map_err(|_| "Remote grant unavailable")?.peers()
                                .get(&peer.to_string())
                                .is_some_and(|current| current.can_connect && current.grant == folder_grant) {
                                return Err("Remote access changed. Reopen this computer.".into());
                            }
                            Ok(())
                        };
                        if method == wire::Method::UploadAttachment {
                            let result = async {
                                let (user, import): (String, crate::backend::ImportAttachmentRequest) = serde_json::from_value(request.params)
                                    .map_err(|_| "Invalid attachment import parameters")?;
                                if user != node.account { return Err("Remote request account mismatch".into()); }
                                node.backend.authorize_account(&user)?;
                                let prepared = crate::backend::attachments::prepare(&import).await?;
                                authorize_folders.await?;
                                client.upload(id, user, import, prepared).await
                            }.await;
                            return match result {
                                Ok(frame) => frame,
                                Err(error) => Message::Response { id, result: Err(error) },
                            };
                        }
                        match client.call(wire::Command { id, method, send_id: request.send_id, params: request.params }, authorize_folders).await {
                            Ok(frame) => streams::decode_result(method, frame),
                            Err(error) => Message::Response { id, result: Err(match send_id {
                                Some(id) => format!("Send outcome unknown ({id}). {error}. Checking the host receipt is safe; resending is not."),
                                None => error,
                            }) },
                        }
                    });
                }
            }
        }
    }.await;
    readers.abort_all();
    link.connection.close(0u32.into(), b"viewer disconnected");
    result
}

async fn local_send(
    tx: &mut (impl tokio::io::AsyncWrite + Unpin),
    message: &Message,
) -> Result<(), String> {
    tokio::time::timeout(streams::IO_TIMEOUT, protocol::write(tx, message))
        .await
        .map_err(|_| "Local remote viewer stopped reading")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn peer_info_journal_failure_keeps_baked_identity_and_blocks_installation() {
        let private_error = "journal-canary-account/private-path";
        let info = peer_information(Err(private_error.into())).unwrap();
        assert_eq!(info.running, RunningApplication::current());
        assert_eq!(info.protocol, ProtocolInfo::current());
        assert_eq!(
            info.update.state,
            super::super::update::UpdateState::Unknown
        );
        assert_eq!(
            info.update.blocked,
            Some(BlockedReason::CoordinatorUnavailable)
        );
        assert!(info.update.offered.is_none());
        assert!(info.update.operation.is_none());
        let response = serde_json::to_string(&info).unwrap();
        assert!(!response.contains(private_error));
        assert!(!response.contains("private-path"));
    }

    #[test]
    fn peer_info_preserves_successful_status_including_password_block() {
        let status = RemoteUpdateStatus {
            state: super::super::update::UpdateState::Current,
            blocked: Some(BlockedReason::PasswordProtected),
            ..Default::default()
        };
        let info = peer_information(Ok(status.clone())).unwrap();
        assert_eq!(info.update, status);
        assert_eq!(info.running, RunningApplication::current());
    }

    #[tokio::test]
    async fn delayed_remote_subscribe_cannot_steal_a_newer_navigation_intent() {
        const A: &str = "01234567-89ab-7def-8123-456789abcdef";
        const B: &str = "11234567-89ab-7def-8123-456789abcdef";
        let subscription = Arc::new(RwLock::new(Subscription::default()));
        let (release, delayed) = tokio::sync::oneshot::channel::<()>();
        let old_subscription = subscription.clone();
        let old_stream = tokio::spawn(async move {
            delayed.await.unwrap();
            set_subscription(
                &old_subscription,
                "account",
                serde_json::json!([
                    "account", {"intent":1,"tasks":[A]}
                ]),
            )
            .unwrap();
        });
        set_subscription(
            &subscription,
            "account",
            serde_json::json!([
                "account", {"intent":2,"tasks":[B]}
            ]),
        )
        .unwrap();
        // An invalid account cannot poison the sequence before the old stream
        // resumes, even if it requests the largest possible intent.
        assert!(
            set_subscription(
                &subscription,
                "account",
                serde_json::json!([
                    "other-account", {"intent":u64::MAX,"tasks":[A]}
                ])
            )
            .is_err()
        );
        release.send(()).unwrap();
        old_stream.await.unwrap();
        let subscription = subscription.read().unwrap();
        for (task, expected) in [(A, false), (B, true)] {
            let event = AgentServiceEvent::Run {
                session_id: task.into(),
                run_id: "run".into(),
                event: maple_agent::agent::AgentRunEvent::SubagentActivity {
                    id: "worker".into(),
                    tool: "read".into(),
                },
            };
            assert_eq!(subscription.allows(&event), expected);
        }
    }

    #[test]
    fn oversized_live_event_requests_paged_resync_without_a_giant_frame() {
        let item = serde_json::from_value(serde_json::json!({
            "id":"call", "itemType":"tool_call", "input":{"large":"\u{0000}".repeat(1024*1024)},
            "createdMs":1, "merge":"upsert"
        }))
        .unwrap();
        let event = AgentServiceEvent::TimelineItem {
            session_id: "01234567-89ab-7def-8123-456789abcdef".into(),
            run_id: Some("run".into()),
            item,
        };
        let encoded = encode_event(&event).unwrap();
        assert!(encoded.len() < 64);
        assert!(matches!(
            serde_json::from_slice::<Message>(&encoded).unwrap(),
            Message::Resync
        ));
    }

    #[test]
    fn history_epoch_changes_cross_the_remote_event_boundary() {
        let event = AgentServiceEvent::HistoryReplaced {
            user_id: "account".into(),
            session_id: "01234567-89ab-7def-8123-456789abcdef".into(),
            rows_epoch: 2,
        };
        assert!(remote_event(&event));
        let message: Message = serde_json::from_slice(&encode_event(&event).unwrap()).unwrap();
        assert!(
            matches!(message,Message::Event(event) if matches!(*event,AgentServiceEvent::HistoryReplaced{rows_epoch:2,..}))
        );
    }

    #[test]
    fn history_epoch_events_are_scoped_before_remote_delivery() {
        let own = AgentServiceEvent::HistoryReplaced {
            user_id: "alice".into(),
            session_id: "01234567-89ab-7def-8123-456789abcdef".into(),
            rows_epoch: 2,
        };
        let other = AgentServiceEvent::HistoryReplaced {
            user_id: "bob".into(),
            session_id: "11234567-89ab-7def-8123-456789abcdef".into(),
            rows_epoch: 7,
        };
        // An untouched subscription and a summary-only subscription both
        // accept epoch events. Neither can bypass the account filter.
        let mut subscription = Subscription::default();
        for configured in [false, true] {
            if configured {
                subscription
                    .set(SubscribeRequest {
                        intent: 1,
                        tasks: Vec::new(),
                    })
                    .unwrap();
            }
            assert!(subscription.allows(&own));
            assert!(subscription.allows(&other));
            assert!(event_belongs_to_account(&own, "alice"));
            assert!(!event_belongs_to_account(&other, "alice"));
            assert!(!event_belongs_to_account(&own, "bob"));
            assert!(event_belongs_to_account(&other, "bob"));
        }
    }

    #[tokio::test]
    async fn control_handshake_retries_disconnect_but_not_invalid_frames_or_revocation() {
        let (mut tx, mut rx) = tokio::io::duplex(1024);
        tx.write_u32(32).await.unwrap();
        tx.write_all(b"{\"Ok\":").await.unwrap();
        drop(tx);
        assert_eq!(
            read_control_ack(&mut rx).await.unwrap_err(),
            "Remote computer disconnected"
        );
        let (mut tx, mut rx) = tokio::io::duplex(1024);
        wire::write(
            &mut tx,
            &serde_json::json!({"unexpected": true}),
            wire::MAX_PREFACE,
        )
        .await
        .unwrap();
        assert!(
            read_control_ack(&mut rx)
                .await
                .unwrap_err()
                .starts_with("Remote access ended:")
        );
        let (mut tx, mut rx) = tokio::io::duplex(1024);
        wire::write(
            &mut tx,
            &Err::<(), _>(wire::Refusal::terminal("Revoked")),
            wire::MAX_PREFACE,
        )
        .await
        .unwrap();
        assert_eq!(
            read_control_ack(&mut rx).await.unwrap_err(),
            "Remote access ended: Revoked"
        );
        wire::write(&mut tx, &Ok::<(), wire::Refusal>(()), wire::MAX_PREFACE)
            .await
            .unwrap();
        assert!(read_control_ack(&mut rx).await.is_ok());
    }
}
