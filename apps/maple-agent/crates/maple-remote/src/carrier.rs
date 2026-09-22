//! A bidirectional frame transport.
//!
//! The server and the client are written against this trait. Tests and
//! the local loopback use [`in_process_pair`]; the WebSocket carrier with
//! Noise inside is a separate implementation that delivers the same
//! frames.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::frame::Frame;

/// The sending half of a carrier.
#[async_trait]
pub trait FrameSink: Send + 'static {
    /// Deliver one frame. `Err` means the carrier is gone.
    async fn send(&mut self, frame: Frame) -> Result<(), String>;
    /// Close the carrier for good.
    async fn close(&mut self);
}

/// The receiving half of a carrier.
#[async_trait]
pub trait FrameStream: Send + 'static {
    /// The next frame, or `None` once the peer closed.
    async fn recv(&mut self) -> Option<Frame>;
}

/// Both halves, before they are split.
pub struct Carrier {
    pub sink: Box<dyn FrameSink>,
    pub stream: Box<dyn FrameStream>,
}

struct ChannelSink(Option<mpsc::Sender<Frame>>);

#[async_trait]
impl FrameSink for ChannelSink {
    async fn send(&mut self, frame: Frame) -> Result<(), String> {
        match &self.0 {
            Some(tx) => tx
                .send(frame)
                .await
                .map_err(|_| "peer closed the carrier".to_string()),
            None => Err("carrier closed".to_string()),
        }
    }

    async fn close(&mut self) {
        self.0 = None;
    }
}

struct ChannelStream(mpsc::Receiver<Frame>);

#[async_trait]
impl FrameStream for ChannelStream {
    async fn recv(&mut self) -> Option<Frame> {
        self.0.recv().await
    }
}

/// Two connected carriers in one process. Frames sent on one arrive on
/// the other. `buffer` frames may be in flight each way.
pub fn in_process_pair(buffer: usize) -> (Carrier, Carrier) {
    let (a_to_b_tx, a_to_b_rx) = mpsc::channel(buffer);
    let (b_to_a_tx, b_to_a_rx) = mpsc::channel(buffer);
    (
        Carrier {
            sink: Box::new(ChannelSink(Some(a_to_b_tx))),
            stream: Box::new(ChannelStream(b_to_a_rx)),
        },
        Carrier {
            sink: Box::new(ChannelSink(Some(b_to_a_tx))),
            stream: Box::new(ChannelStream(a_to_b_rx)),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_cross_the_pair_both_ways_and_close_ends_the_stream() {
        let (mut a, mut b) = in_process_pair(4);
        a.sink.send(Frame::control("to b")).await.unwrap();
        b.sink.send(Frame::control("to a")).await.unwrap();
        assert_eq!(b.stream.recv().await.unwrap().payload, "to b");
        assert_eq!(a.stream.recv().await.unwrap().payload, "to a");
        a.sink.close().await;
        assert!(b.stream.recv().await.is_none());
        assert!(a.sink.send(Frame::control("late")).await.is_err());
    }
}
