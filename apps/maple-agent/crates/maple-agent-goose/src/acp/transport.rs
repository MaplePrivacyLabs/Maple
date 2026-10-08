//! The ACP transport: framing limits on the way in, and credit-tracked
//! backpressure on the way out.

use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::{Client, ConnectionTo};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

pub(super) const MAX_ACP_FRAME_BYTES: usize = 10 * 1024 * 1024;
const MAX_ACP_OUTBOUND_EVENTS_IN_FLIGHT: usize = 256;
const MAX_ACP_OUTBOUND_BYTES_IN_FLIGHT: usize = 4 * 1024 * 1024;
const ACP_OUTBOUND_FRAME_OVERHEAD_BYTES: usize = 256;
pub(super) static NEXT_ACP_MESSAGE_ID: AtomicUsize = AtomicUsize::new(1);

pub(super) struct BoundedLineReader<R> {
    inner: R,
    bytes_since_newline: usize,
    eof: CancellationToken,
}

impl<R> BoundedLineReader<R> {
    pub(super) fn new(inner: R, eof: CancellationToken) -> Self {
        Self {
            inner,
            bytes_since_newline: 0,
            eof,
        }
    }
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for BoundedLineReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let previous_len = buf.filled().len();
        match std::pin::Pin::new(&mut self.inner).poll_read(cx, buf) {
            std::task::Poll::Ready(Ok(())) => {
                if buf.filled().len() == previous_len {
                    self.eof.cancel();
                }
                for byte in &buf.filled()[previous_len..] {
                    if *byte == b'\n' {
                        self.bytes_since_newline = 0;
                    } else {
                        self.bytes_since_newline = self.bytes_since_newline.saturating_add(1);
                        if self.bytes_since_newline > MAX_ACP_FRAME_BYTES {
                            return std::task::Poll::Ready(Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "ACP frame exceeds the 10 MiB limit",
                            )));
                        }
                    }
                }
                std::task::Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

pub(super) fn is_session_update_line(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|message| {
            message
                .get("method")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("session/update")
}

pub(super) fn tracked_outgoing_lines<W>(
    writer: W,
    outbound: Arc<AcpOutboundTracker>,
) -> impl futures_util::Sink<String, Error = std::io::Error> + Send
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    futures_util::sink::unfold(
        (writer, outbound),
        |(mut writer, outbound), line: String| async move {
            use tokio::io::AsyncWriteExt as _;

            let session_update = is_session_update_line(&line);
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            if session_update {
                // Credits return only after the real local socket accepted the
                // complete notification. A peer that stops reading therefore
                // backpressures Maple instead of growing ACP's internal queues.
                outbound.acknowledge_session_update();
            }
            Ok((writer, outbound))
        },
    )
}
pub(super) struct AcpOutboundTracker {
    event_slots: Arc<Semaphore>,
    byte_slots: Arc<Semaphore>,
    pub(super) pending: std::sync::Mutex<VecDeque<AcpOutboundReservation>>,
}

pub(super) struct AcpOutboundReservation {
    _event: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

#[derive(Debug)]
pub(super) enum AcpOutboundSendError {
    UpdateTooLarge,
    Cancelled,
    Transport(agent_client_protocol::Error),
}
impl AcpOutboundTracker {
    pub(super) fn new() -> Arc<Self> {
        Self::with_limits(
            MAX_ACP_OUTBOUND_EVENTS_IN_FLIGHT,
            MAX_ACP_OUTBOUND_BYTES_IN_FLIGHT,
        )
    }

    pub(super) fn with_limits(event_limit: usize, byte_limit: usize) -> Arc<Self> {
        Arc::new(Self {
            event_slots: Arc::new(Semaphore::new(event_limit)),
            byte_slots: Arc::new(Semaphore::new(byte_limit)),
            pending: std::sync::Mutex::new(VecDeque::new()),
        })
    }

    pub(super) async fn reserve(
        &self,
        encoded_bytes: usize,
        cancellation: &CancellationToken,
    ) -> Result<AcpOutboundReservation, AcpOutboundSendError> {
        let charged_bytes = encoded_bytes.saturating_add(ACP_OUTBOUND_FRAME_OVERHEAD_BYTES);
        let Ok(charged_bytes) = u32::try_from(charged_bytes) else {
            return Err(AcpOutboundSendError::UpdateTooLarge);
        };
        if charged_bytes as usize > MAX_ACP_OUTBOUND_BYTES_IN_FLIGHT {
            return Err(AcpOutboundSendError::UpdateTooLarge);
        }

        let event = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AcpOutboundSendError::Cancelled),
            permit = Arc::clone(&self.event_slots).acquire_owned() => {
                permit.map_err(|_| AcpOutboundSendError::Cancelled)?
            }
        };
        let bytes = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AcpOutboundSendError::Cancelled),
            permit = Arc::clone(&self.byte_slots).acquire_many_owned(charged_bytes) => {
                permit.map_err(|_| AcpOutboundSendError::Cancelled)?
            }
        };
        Ok(AcpOutboundReservation {
            _event: event,
            _bytes: bytes,
        })
    }

    pub(super) fn enqueue(
        &self,
        cx: &ConnectionTo<Client>,
        notification: SessionNotification,
        reservation: AcpOutboundReservation,
    ) -> Result<(), AcpOutboundSendError> {
        // Serialize reservation order with the protocol enqueue. The socket
        // writer can then release one exact FIFO credit for each written
        // session/update line, even when several ACP sessions stream together.
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.push_back(reservation);
        if let Err(error) = cx.send_notification(notification) {
            pending.pop_back();
            return Err(AcpOutboundSendError::Transport(error));
        }
        Ok(())
    }

    fn acknowledge_session_update(&self) {
        let reservation = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        if reservation.is_none() {
            log::warn!("Maple ACP wrote an untracked session/update notification");
        }
        drop(reservation);
    }
}
