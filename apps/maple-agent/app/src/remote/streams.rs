//! One bounded request/reply per QUIC stream. The initial authenticated stream
//! carries ordered events only. No transfer owns the connection's dispatch loop.
use super::wire::{self, Command, Method};
use crate::host::protocol::Message;
use iroh::endpoint::{Connection, QuicTransportConfig, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};
use zeroize::Zeroizing;

pub const MAX_ORDINARY_REQUESTS: usize = 32;
pub const MAX_URGENT_REQUESTS: usize = 2;
pub const MAX_RANGE_REQUESTS: usize = 4;
pub const MAX_BULK_REQUESTS: usize = 2;
pub const MAX_REQUESTS: usize =
    MAX_ORDINARY_REQUESTS + MAX_URGENT_REQUESTS + MAX_RANGE_REQUESTS + MAX_BULK_REQUESTS;
// Limit concurrent header readers independently of admitted RPCs. Requests
// queue briefly behind a burst, under one queue-and-parse deadline. A malicious
// peer withholding headers can delay classification only until that deadline.
pub const MAX_PENDING_HEADERS: usize = 8;
pub const MAX_REQUEST_TASKS: usize = MAX_REQUESTS + MAX_PENDING_HEADERS;
const MAX_BIDI_STREAMS: u32 = MAX_REQUEST_TASKS as u32 + 1;
const STREAM_RECEIVE_WINDOW: u32 = 512 * 1024;
const CONNECTION_RECEIVE_WINDOW: u32 = 32 * 1024 * 1024;
const _: () = assert!(CONNECTION_RECEIVE_WINDOW > MAX_BIDI_STREAMS * STREAM_RECEIVE_WINDOW);
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const FOLDERS_UNCONFIRMED: &str =
    "Could not confirm folder browsing support. Try again, or update Maple on the remote computer.";
const FOLDERS_UNSUPPORTED: &str = "Update Maple on the remote computer to browse or create folders. Existing projects and chats remain available.";
// Attachments may take longer than control traffic on a slow link, but cannot
// retain an admitted stream indefinitely.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_CONTROL: usize = 64 * 1024;

pub fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        // One event/handshake stream plus bounded request streams. The aggregate
        // window exceeds all stream windows, so unread bulk streams cannot use
        // up the flow-control credit needed by another stream.
        .max_concurrent_bidi_streams(MAX_BIDI_STREAMS.into())
        .max_concurrent_uni_streams(0u32.into())
        .stream_receive_window(STREAM_RECEIVE_WINDOW.into())
        .receive_window(CONNECTION_RECEIVE_WINDOW.into())
        .build()
}

/// Bound work before classification too: withheld headers occupy tasks until
/// their queue/read deadline. Closing the connection stops admission, while
/// already admitted effects drain exactly once.
pub(super) async fn serve_requests<F, Fut>(connection: Connection, mut serve: F)
where
    F: FnMut(SendStream, RecvStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut calls = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = calls.join_next(), if !calls.is_empty() => {},
            incoming = connection.accept_bi(), if calls.len() < MAX_REQUEST_TASKS => {
                let Ok((tx, rx)) = incoming else { break; };
                calls.spawn(serve(tx, rx));
            }
        }
    }
    while calls.join_next().await.is_some() {}
}

pub struct Capacity {
    ordinary: Arc<Semaphore>,
    urgent: Arc<Semaphore>,
    ranges: Arc<Semaphore>,
    bulk: Arc<Semaphore>,
    headers: Arc<Semaphore>,
}

impl Capacity {
    pub fn new() -> Self {
        Self {
            ordinary: Arc::new(Semaphore::new(MAX_ORDINARY_REQUESTS)),
            urgent: Arc::new(Semaphore::new(MAX_URGENT_REQUESTS)),
            ranges: Arc::new(Semaphore::new(MAX_RANGE_REQUESTS)),
            bulk: Arc::new(Semaphore::new(MAX_BULK_REQUESTS)),
            headers: Arc::new(Semaphore::new(MAX_PENDING_HEADERS)),
        }
    }
    pub async fn read_header(&self, rx: &mut RecvStream) -> Result<Header, String> {
        self.read_header_with_timeout(rx, IO_TIMEOUT).await
    }
    async fn read_header_with_timeout(
        &self,
        rx: &mut RecvStream,
        deadline: Duration,
    ) -> Result<Header, String> {
        tokio::time::timeout(deadline, async {
            let _slot = self
                .headers
                .acquire()
                .await
                .map_err(|_| "Remote header admission closed")?;
            wire::read(rx, wire::MAX_PREFACE)
                .await
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|_| "Remote request header timed out".to_string())?
    }
    pub fn acquire(&self, method: Method) -> Result<OwnedSemaphorePermit, String> {
        let pool = if method == Method::UploadAttachment {
            &self.bulk
        } else if matches!(method, Method::ReadAttachment | Method::ReadRow) {
            &self.ranges
        } else if method.urgent() {
            &self.urgent
        } else {
            &self.ordinary
        };
        pool.clone()
            .try_acquire_owned()
            .map_err(|_| "Remote computer is busy; try again".into())
    }
}

/// One authenticated bridge connection. Only immutable feature support is
/// cached here, never an account/grant, update offer or installation status.
/// Reconnecting constructs a new client and discovers support again.
pub struct Client {
    connection: Connection,
    project_folders: OnceCell<bool>,
    discovery_timeout: Duration,
}

impl Client {
    pub fn new(connection: Connection) -> Self {
        Self {
            connection,
            project_folders: OnceCell::new(),
            discovery_timeout: DISCOVERY_TIMEOUT,
        }
    }

    pub(super) async fn upload(
        &self,
        id: u64,
        user: String,
        request: crate::backend::ImportAttachmentRequest,
        prepared: crate::backend::attachments::PreparedImport,
    ) -> Result<Message, String> {
        super::uploads::call(&self.connection, id, user, request, prepared).await
    }

    pub async fn call(
        &self,
        command: Command,
        authorize_folders: impl std::future::Future<Output = Result<(), String>>,
    ) -> Result<Message, String> {
        if matches!(
            command.method,
            Method::BrowseProjectFolders | Method::CreateProjectFolder
        ) {
            // Bound waiting for another caller's discovery as well as this
            // caller's probe. Failed probes leave the cell uninitialized, so
            // a temporary failure cannot permanently disable the picker.
            let supported = tokio::time::timeout(
                self.discovery_timeout,
                self.project_folders.get_or_try_init(|| async {
                    peer_info(&self.connection)
                        .await
                        .map(|info| info.supports_project_folders())
                        .ok_or_else(|| FOLDERS_UNCONFIRMED.to_owned())
                }),
            )
            .await
            .map_err(|_| FOLDERS_UNCONFIRMED.to_owned())??;
            if !supported {
                return Err(FOLDERS_UNSUPPORTED.into());
            }
            // Discovery may have waited on the network. Cached support is
            // never authority: recheck account and grant before dispatch.
            authorize_folders.await?;
        }
        call(&self.connection, command).await
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub id: u64,
    pub method: Method,
    pub send_id: Option<String>,
}

impl Header {
    #[cfg(test)]
    pub async fn read(rx: &mut RecvStream) -> Result<Self, String> {
        tokio::time::timeout(IO_TIMEOUT, wire::read(rx, wire::MAX_PREFACE))
            .await
            .map_err(|_| "Remote request header timed out")?
            .map_err(|e| e.to_string())
    }
    pub async fn body(self, rx: &mut RecvStream) -> Result<Command, String> {
        let params = wire::read_progress(
            rx,
            body_limit(self.method),
            transfer_timeout(self.method),
            false,
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(Command {
            id: self.id,
            method: self.method,
            send_id: self.send_id,
            params,
        })
    }
}

fn body_limit(method: Method) -> usize {
    if matches!(
        method,
        Method::PeerInfo
            | Method::RemoteUpdateInstall
            | Method::UploadAttachment
            | Method::AttachmentUploadStatus
            | Method::CancelAttachmentUpload
    ) {
        wire::MAX_PREFACE
    } else if method.urgent() {
        MAX_CONTROL
    } else {
        wire::MAX_FRAME
    }
}

/// Optional management discovery never inherits ordinary RPC's unbounded wait
/// for the first response byte. A legacy peer may reject only this stream.
pub(crate) async fn peer_info(connection: &Connection) -> Option<super::update::PeerInfo> {
    peer_info_with_timeout(connection, Duration::from_secs(5)).await
}

async fn peer_info_with_timeout(
    connection: &Connection,
    timeout: Duration,
) -> Option<super::update::PeerInfo> {
    let response = tokio::time::timeout(
        timeout,
        call(
            connection,
            Command {
                id: 1,
                method: Method::PeerInfo,
                send_id: None,
                params: serde_json::json!({}),
            },
        ),
    )
    .await
    .ok()?
    .ok()?;
    let Message::Response {
        result: Ok(value), ..
    } = response
    else {
        return None;
    };
    let info: super::update::PeerInfo = serde_json::from_value(value).ok()?;
    info.validate().ok()?;
    Some(info)
}

fn transfer_timeout(method: Method) -> Duration {
    if method.urgent() {
        IO_TIMEOUT
    } else {
        TRANSFER_TIMEOUT
    }
}

// Ranged payloads are already base64 in the transport-neutral store response.
pub fn encode_result(
    _method: Method,
    result: Result<serde_json::Value, String>,
) -> Result<serde_json::Value, String> {
    result
}
pub fn decode_result(_method: Method, message: Message) -> Message {
    message
}

pub async fn reply(tx: &mut SendStream, message: &Message) -> Result<(), String> {
    reply_with_limit(tx, message, wire::MAX_FRAME).await
}

pub async fn reply_with_limit(
    tx: &mut SendStream,
    message: &Message,
    limit: usize,
) -> Result<(), String> {
    let bytes = Zeroizing::new(match wire::encode(message, limit) {
        Ok(bytes) => bytes,
        Err(error) => match message {
            Message::Response { id, .. } => wire::encode(
                &Message::Response {
                    id: *id,
                    result: Err("Remote reply is too large to transfer".into()),
                },
                wire::MAX_FRAME,
            )
            .map_err(|e| e.to_string())?,
            _ => return Err(error.to_string()),
        },
    });
    wire::write_progress(tx, &bytes, TRANSFER_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    tx.finish().map_err(|_| "Remote reply closed")?;
    let stopped = tokio::time::timeout(TRANSFER_TIMEOUT, tx.stopped())
        .await
        .map_err(|_| "Remote reply transfer timed out")?
        .map_err(|_| "Remote reply disconnected")?;
    if stopped.is_some() {
        return Err("Remote consumer rejected the reply".into());
    }
    Ok(())
}

pub async fn call(connection: &Connection, command: Command) -> Result<Message, String> {
    let id = command.id;
    // Reject size before transmitting anything. A later transport failure is
    // uncertain and must disconnect/query the receipt, never report not-sent.
    let Ok(body) = wire::encode(&command.params, body_limit(command.method)) else {
        return Ok(Message::Response {
            id,
            result: Err("Remote request is too large. Try a smaller message.".into()),
        });
    };
    let body = Zeroizing::new(body);
    let header = Header {
        id,
        method: command.method,
        send_id: command.send_id,
    };
    let (mut tx, mut rx) = tokio::time::timeout(IO_TIMEOUT, connection.open_bi())
        .await
        .map_err(|_| "Remote stream capacity timed out")?
        .map_err(|_| "Remote connection closed")?;
    tx.set_priority(header.method.priority())
        .map_err(|_| "Remote stream closed")?;
    tokio::time::timeout(IO_TIMEOUT, wire::write(&mut tx, &header, wire::MAX_PREFACE))
        .await
        .map_err(|_| "Remote request header timed out")?
        .map_err(|e| e.to_string())?;
    wire::write_progress(&mut tx, &body, transfer_timeout(header.method))
        .await
        .map_err(|e| e.to_string())?;
    tx.finish().map_err(|_| "Remote request closed")?;
    let response = wire::read_progress::<Message>(
        &mut rx,
        header.method.reply_limit(),
        TRANSFER_TIMEOUT,
        true,
    );
    let response = if header.method.urgent() {
        tokio::time::timeout(IO_TIMEOUT, response)
            .await
            .map_err(|_| "Remote control reply timed out")?
    } else {
        response.await
    };
    let message = match response {
        Ok(message @ Message::Response { id: received, .. }) if received == id => message,
        Ok(message @ Message::Closed { .. }) => message,
        Err(error @ (wire::Error::Disconnected | wire::Error::TimedOut)) => {
            return Err(error.to_string());
        }
        _ => {
            return Ok(Message::Closed {
                reason: "Invalid remote reply; reconnect with compatible Maple versions".into(),
            });
        }
    };
    // A framed read alone does not consume QUIC FIN. Dropping rx at that point
    // can send STOP_SENDING even for a valid response, so the server cannot
    // distinguish normal receipt from rejection before activating an install.
    // Only EOF belongs after the one bounded reply; reject extra data promptly.
    finish_reply(&mut rx, IO_TIMEOUT).await?;
    Ok(message)
}

pub(super) async fn finish_reply(rx: &mut RecvStream, deadline: Duration) -> Result<(), String> {
    let mut trailing = [0u8; 1];
    let count = tokio::time::timeout(deadline, tokio::io::AsyncReadExt::read(rx, &mut trailing))
        .await
        .map_err(|_| "Remote reply did not finish")?
        .map_err(|_| "Remote reply disconnected")?;
    if count != 0 {
        return Err("Remote reply contained unexpected extra data".into());
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests;

#[cfg(test)]
mod folder_tests;
