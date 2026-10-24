//! One bounded request/reply per QUIC stream. The initial authenticated stream
//! carries ordered events only. No transfer owns the connection's dispatch loop.
use super::wire::{self, Command, Method};
use crate::host::protocol::Message;
use iroh::endpoint::{Connection, QuicTransportConfig, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use zeroize::Zeroizing;

pub const MAX_REQUESTS: usize = 40;
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
// Attachments may take longer than control traffic on a slow link, but cannot
// retain an admitted stream indefinitely.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_CONTROL: usize = 64 * 1024;

pub fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        // One event/handshake stream plus bounded request streams. The aggregate
        // window exceeds all stream windows, so unread bulk streams cannot use
        // up the flow-control credit needed by another stream.
        .max_concurrent_bidi_streams(41u32.into())
        .max_concurrent_uni_streams(0u32.into())
        .stream_receive_window((512u32 * 1024).into())
        .receive_window((32u32 * 1024 * 1024).into())
        .build()
}

pub struct Capacity {
    ordinary: Arc<Semaphore>,
    urgent: Arc<Semaphore>,
    ranges: Arc<Semaphore>,
}

impl Capacity {
    pub fn new() -> Self {
        Self {
            ordinary: Arc::new(Semaphore::new(32)),
            urgent: Arc::new(Semaphore::new(2)),
            ranges: Arc::new(Semaphore::new(4)),
        }
    }
    pub fn acquire(&self, method: Method) -> Result<OwnedSemaphorePermit, String> {
        let pool = if matches!(method, Method::ReadAttachment | Method::ReadRow) {
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub id: u64,
    pub method: Method,
    pub send_id: Option<String>,
}

impl Header {
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
    if matches!(method, Method::PeerInfo | Method::RemoteUpdateInstall) {
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
            result: Err("Remote message is too large. Try fewer or smaller images.".into()),
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

async fn finish_reply(rx: &mut RecvStream, deadline: Duration) -> Result<(), String> {
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
mod tests;
