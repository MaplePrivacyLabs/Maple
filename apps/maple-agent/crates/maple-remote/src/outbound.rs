//! The bounded outbound queue in front of a carrier.
//!
//! Every frame a side sends goes through one queue that counts queued
//! bytes. A frame that would push the count past the limit closes the
//! connection: the peer has stopped draining, and the host must never
//! wait on a client. The peer comes back through its ordinary reconnect
//! and resync path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::sync::mpsc;

use crate::frame::{Frame, HEADER_BYTES, max_payload_bytes};

/// Default byte limit for one connection's outbound queue.
pub const DEFAULT_MAX_OUTBOUND_BYTES: usize = 64 * 1024 * 1024;

struct Shared {
    queued_bytes: AtomicUsize,
    limit: usize,
    overflowed: AtomicBool,
}

/// The sending handle. Cheap to clone; every clone shares the budget.
#[derive(Clone)]
pub struct Outbound {
    tx: mpsc::UnboundedSender<Frame>,
    shared: Arc<Shared>,
}

/// The draining end, owned by the writer task.
pub struct OutboundQueue {
    rx: mpsc::UnboundedReceiver<Frame>,
    shared: Arc<Shared>,
}

/// Create a queue with a byte limit.
pub fn channel(max_bytes: usize) -> (Outbound, OutboundQueue) {
    let (tx, rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        queued_bytes: AtomicUsize::new(0),
        limit: max_bytes,
        overflowed: AtomicBool::new(false),
    });
    (
        Outbound {
            tx,
            shared: Arc::clone(&shared),
        },
        OutboundQueue { rx, shared },
    )
}

impl Outbound {
    /// Queue a frame. Fails when the frame is larger than the peer would
    /// accept, when the queue is closed, or when the frame would overflow
    /// the budget; an overflow also marks the connection for closing.
    pub fn try_send(&self, frame: Frame) -> Result<(), String> {
        let limit = max_payload_bytes(frame.channel);
        if frame.payload.len() > limit {
            return Err(format!(
                "frame of {} bytes on channel {} exceeds the {limit} byte limit",
                frame.payload.len(),
                frame.channel
            ));
        }
        let bytes = frame.payload.len() + HEADER_BYTES;
        let queued = self.shared.queued_bytes.fetch_add(bytes, Ordering::AcqRel) + bytes;
        if queued > self.shared.limit {
            self.shared.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
            self.shared.overflowed.store(true, Ordering::Release);
            return Err(format!(
                "outbound queue over its {} byte limit; closing the connection",
                self.shared.limit
            ));
        }
        self.tx.send(frame).map_err(|_| {
            self.shared.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
            "connection closed".to_string()
        })
    }

    /// True once a frame overflowed the budget.
    pub fn overflowed(&self) -> bool {
        self.shared.overflowed.load(Ordering::Acquire)
    }
}

impl OutboundQueue {
    /// The next frame to write, with its bytes released from the budget.
    pub async fn recv(&mut self) -> Option<Frame> {
        let frame = self.rx.recv().await?;
        self.release(&frame);
        Some(frame)
    }

    /// A frame already queued, without waiting for one.
    pub fn try_recv(&mut self) -> Option<Frame> {
        let frame = self.rx.try_recv().ok()?;
        self.release(&frame);
        Some(frame)
    }

    fn release(&self, frame: &Frame) {
        self.shared
            .queued_bytes
            .fetch_sub(frame.payload.len() + HEADER_BYTES, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn overflow_refuses_the_frame_and_marks_the_connection() {
        let (out, mut queue) = channel(100);
        out.try_send(Frame::control(vec![0u8; 50])).unwrap();
        assert!(!out.overflowed());
        assert!(out.try_send(Frame::control(vec![0u8; 60])).is_err());
        assert!(out.overflowed());
        // Draining releases the budget.
        assert_eq!(queue.recv().await.unwrap().payload.len(), 50);
        out.try_send(Frame::control(vec![0u8; 60])).unwrap();
        drop(queue);
        assert!(out.try_send(Frame::control("late")).is_err());
    }

    #[tokio::test]
    async fn an_oversized_frame_fails_at_the_sender_without_overflowing() {
        use crate::frame::{FrameKind, MAX_CONTROL_FRAME_BYTES, MAX_STREAM_FRAME_BYTES};
        let (out, mut queue) = channel(usize::MAX);
        let error = out
            .try_send(Frame::control(vec![0u8; MAX_CONTROL_FRAME_BYTES + 1]))
            .unwrap_err();
        assert!(error.contains("exceeds"), "{error}");
        assert!(!out.overflowed(), "a refused frame is not an overflow");
        assert!(
            out.try_send(Frame {
                channel: 4,
                kind: FrameKind::Data,
                payload: vec![0u8; MAX_STREAM_FRAME_BYTES + 1].into(),
            })
            .is_err()
        );
        out.try_send(Frame::control(vec![0u8; MAX_CONTROL_FRAME_BYTES]))
            .unwrap();
        assert_eq!(
            queue.recv().await.unwrap().payload.len(),
            MAX_CONTROL_FRAME_BYTES,
            "only the frame within the limit was queued"
        );
    }
}
