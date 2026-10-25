//! Wire 7 attachment uploads. Metadata is bounded JSON; payload bytes occupy a
//! separate low-priority QUIC stream and never an ordinary RPC/control frame.
use super::{manager::Node, streams, wire};
use crate::{
    backend::attachments::{self, ImportAttachmentRequest, PreparedImport},
    host::protocol::Message,
};
use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
};
use maple_agent::store::{
    AttachmentUploadMetadata, AttachmentUploadStart, AttachmentUploadWriter, StoredAttachment,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;

const CHUNK: usize = 64 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
// The idle bound permits useful slow links; the total bound prevents a peer
// retaining a bulk slot forever by supplying one byte before each idle timeout.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Upload {
    pub user: String,
    pub session_id: String,
    pub operation_id: String,
    pub metadata: AttachmentUploadMetadata,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", content = "attachment", rename_all = "camelCase")]
enum Admission {
    Ready,
    Complete(StoredAttachment),
}

pub(super) async fn call(
    connection: &Connection,
    id: u64,
    user: String,
    request: ImportAttachmentRequest,
    prepared: PreparedImport,
) -> Result<Message, String> {
    let upload = Upload {
        user,
        session_id: request.session_id,
        operation_id: request.operation_id,
        metadata: prepared.metadata,
    };
    // Query the stable upload operation after a lost acknowledgement; never
    // treat a transport error as permission to replay a prompt.
    tokio::time::timeout(TOTAL_TIMEOUT, async {
        let (mut tx, mut rx) = tokio::time::timeout(streams::IO_TIMEOUT, connection.open_bi())
            .await
            .map_err(|_| "Attachment stream capacity timed out")?
            .map_err(|_| "Remote connection closed")?;
        tx.set_priority(-5)
            .map_err(|_| "Attachment stream closed")?;
        let header = streams::Header {
            id,
            method: wire::Method::UploadAttachment,
            send_id: None,
        };
        tokio::time::timeout(streams::IO_TIMEOUT, async {
            wire::write(&mut tx, &header, wire::MAX_PREFACE).await?;
            wire::write(&mut tx, &upload, wire::MAX_PREFACE).await
        })
        .await
        .map_err(|_| "Attachment metadata timed out")?
        .map_err(|e| e.to_string())?;
        let admission = wire::read_progress::<Result<Admission, String>>(
            &mut rx,
            wire::MAX_MANAGEMENT_REPLY,
            IDLE_TIMEOUT,
            false,
        )
        .await
        .map_err(|e| e.to_string())??;
        match admission {
            Admission::Complete(attachment) => {
                tx.finish().map_err(|_| "Attachment stream closed")?;
                streams::finish_reply(&mut rx, streams::IO_TIMEOUT).await?;
                return Ok(Message::Response {
                    id,
                    result: serde_json::to_value(attachment)
                        .map_err(|_| "Could not encode attachment".into()),
                });
            }
            Admission::Ready => {}
        }
        let mut source = tokio::fs::File::from_std(prepared.file);
        let mut buffer = vec![0u8; CHUNK];
        let mut remaining = upload.metadata.size;
        while remaining > 0 {
            let count = usize::try_from(remaining.min(CHUNK as u64)).unwrap();
            let count = source
                .read(&mut buffer[..count])
                .await
                .map_err(|_| "Could not read staged attachment")?;
            if count == 0 {
                return Err("Staged attachment changed during upload".to_string());
            }
            tokio::time::timeout(IDLE_TIMEOUT, tx.write_all(&buffer[..count]))
                .await
                .map_err(|_| "Attachment upload stalled")?
                .map_err(|_| "Attachment upload disconnected")?;
            remaining -= count as u64;
        }
        // A growing staged file is not silently accepted either.
        if source
            .read(&mut buffer[..1])
            .await
            .map_err(|_| "Could not read staged attachment")?
            != 0
        {
            return Err("Staged attachment changed during upload".into());
        }
        tx.finish().map_err(|_| "Attachment upload closed")?;
        let response: Message =
            wire::read_progress(&mut rx, wire::MAX_MANAGEMENT_REPLY, IDLE_TIMEOUT, false)
                .await
                .map_err(|e| e.to_string())?;
        if !matches!(&response, Message::Response { id: received, .. } if *received == id) {
            return Err("Invalid attachment acknowledgement".into());
        }
        streams::finish_reply(&mut rx, streams::IO_TIMEOUT).await?;
        Ok(response)
    })
    .await
    .map_err(|_| "Attachment upload timed out")?
}

async fn read_metadata(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    deadline: Duration,
) -> Result<Upload, String> {
    // Metadata is tiny. Its *total* budget must not restart with every byte.
    tokio::time::timeout(deadline, wire::read(reader, wire::MAX_PREFACE))
        .await
        .map_err(|_| "Attachment metadata timed out")?
        .map_err(|error| error.to_string())
}

pub(super) async fn serve(
    node: Arc<Node>,
    peer: EndpointId,
    id: u64,
    tx: &mut SendStream,
    rx: &mut RecvStream,
) -> Result<(), String> {
    let begin = async {
        let upload = read_metadata(rx, streams::IO_TIMEOUT).await?;
        if upload.user != node.account {
            return Err("Remote request account mismatch".to_string());
        }
        let _account = node.backend.account_gate.read().await;
        node.ensure_valid().await?;
        let (store, account) = attachments::context(&node.backend, &upload.user).await?;
        let begin_store = store.clone();
        let begin_node = node.clone();
        let start = tokio::task::spawn_blocking(move || {
            let grant = begin_node
                .store
                .lock()
                .map_err(|_| "Remote grant unavailable".to_string())?;
            if !grant.permits(peer) {
                return Err("Remote access revoked".into());
            }
            let grant_id = grant
                .peers()
                .get(&peer.to_string())
                .ok_or("Remote access revoked")?
                .grant;
            let start = begin_store
                .begin_attachment_upload(
                    account,
                    &upload.session_id,
                    &upload.operation_id,
                    &upload.metadata,
                )
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((grant_id, start))
        })
        .await
        .map_err(|_| "Attachment worker did not complete")??;
        Ok::<_, String>((store, start))
    }
    .await;
    let (store, (grant_id, start)) = match begin {
        Ok(value) => value,
        Err(error) => {
            let _ = rx.stop(1u32.into());
            return send_admission(tx, Err(error), true).await;
        }
    };
    check_receiver(&node, peer, grant_id).await?;
    let writer = match start {
        AttachmentUploadStart::Complete(attachment) => {
            return send_admission(tx, Ok(Admission::Complete(attachment)), true).await;
        }
        AttachmentUploadStart::Writing(writer) => *writer,
    };
    send_admission(tx, Ok(Admission::Ready), false).await?;
    let result = async {
        let writer = tokio::time::timeout(TOTAL_TIMEOUT, receive_bytes(rx, writer))
            .await
            .map_err(|_| "Attachment upload timed out")??;
        let _account = node.backend.account_gate.read().await;
        node.ensure_valid().await?;
        node.backend.authorize_account(&node.account)?;
        let node = node.clone();
        tokio::task::spawn_blocking(move || {
            // Serialize publication with local revoke/disable. No remote policy
            // or account lock was held while bytes crossed the network.
            let grant = node
                .store
                .lock()
                .map_err(|_| "Remote grant unavailable".to_string())?;
            finish_authorized(
                writer,
                &store,
                &grant,
                peer,
                grant_id,
                node.stopped.is_cancelled(),
            )
        })
        .await
        .map_err(|_| "Attachment worker did not complete")?
    }
    .await;
    if result.is_err() {
        let _ = rx.stop(1u32.into());
    }
    // As with ordinary RPC replies, a completed effect does not authorize
    // exposing its receipt to a peer whose account/grant ended while it ran.
    check_receiver(&node, peer, grant_id).await?;
    let result = result.and_then(|attachment| {
        serde_json::to_value(attachment).map_err(|_| "Could not encode attachment".into())
    });
    streams::reply_with_limit(
        tx,
        &Message::Response { id, result },
        wire::MAX_MANAGEMENT_REPLY,
    )
    .await
}

async fn check_receiver(
    node: &Node,
    peer: EndpointId,
    admitted_grant: [u8; 16],
) -> Result<(), String> {
    node.ensure_valid().await?;
    let policy = node.store.lock().map_err(|_| "Remote grant unavailable")?;
    if policy.permits(peer)
        && policy
            .peers()
            .get(&peer.to_string())
            .is_some_and(|current| current.grant == admitted_grant)
    {
        Ok(())
    } else {
        Err("Remote access revoked".into())
    }
}

fn finish_authorized(
    writer: AttachmentUploadWriter,
    store: &maple_agent::store::Store,
    policy: &super::store::Store,
    peer: EndpointId,
    admitted_grant: [u8; 16],
    stopped: bool,
) -> Result<StoredAttachment, String> {
    if stopped
        || !policy.permits(peer)
        || !policy
            .peers()
            .get(&peer.to_string())
            .is_some_and(|current| current.grant == admitted_grant)
    {
        return Err("Remote access revoked".into());
    }
    writer.finish(store).map_err(|error| error.to_string())
}

pub(super) async fn refuse(tx: &mut SendStream, error: String) -> Result<(), String> {
    send_admission(tx, Err(error), true).await
}

async fn send_admission(
    tx: &mut SendStream,
    value: Result<Admission, String>,
    finish: bool,
) -> Result<(), String> {
    tokio::time::timeout(
        streams::IO_TIMEOUT,
        wire::write(tx, &value, wire::MAX_MANAGEMENT_REPLY),
    )
    .await
    .map_err(|_| "Attachment admission timed out")?
    .map_err(|e| e.to_string())?;
    if finish {
        tx.finish().map_err(|_| "Attachment admission closed")?;
        tokio::time::timeout(streams::IO_TIMEOUT, tx.stopped())
            .await
            .map_err(|_| "Attachment admission timed out")?
            .map_err(|_| "Attachment admission closed")?;
    }
    Ok(())
}

async fn receive_bytes(
    rx: &mut RecvStream,
    mut writer: AttachmentUploadWriter,
) -> Result<AttachmentUploadWriter, String> {
    let mut buffer = vec![0u8; CHUNK];
    let mut cancellation = tokio::time::interval(Duration::from_millis(250));
    loop {
        if writer.is_cancelled() {
            return Err("Attachment upload cancelled".into());
        }
        let count = {
            let read = tokio::time::timeout(IDLE_TIMEOUT, AsyncReadExt::read(rx, &mut buffer));
            tokio::pin!(read);
            loop {
                tokio::select! {
                    result = &mut read => break result.map_err(|_| "Attachment upload stalled")?.map_err(|_| "Attachment upload disconnected")?,
                    _ = cancellation.tick() => if writer.is_cancelled() { return Err("Attachment upload cancelled".into()); }
                }
            }
        };
        if count == 0 {
            return Ok(writer);
        }
        // Writer enforces declared length, digest and cancellation. Only one
        // bounded chunk exists in memory and no SQLite transaction spans a read.
        (writer, buffer) = tokio::task::spawn_blocking(move || {
            writer
                .write_chunk(&buffer[..count])
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((writer, buffer))
        })
        .await
        .map_err(|_| "Attachment worker did not complete")??;
    }
}

#[cfg(test)]
mod tests;
