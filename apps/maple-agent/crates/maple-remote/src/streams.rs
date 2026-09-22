//! Binary streams on channels 1 and up, with credit-based flow control.
//!
//! Either side opens a stream with an `Open` frame, sends `Data` frames
//! while it holds credit, and ends with `Close`. Clients open odd channels
//! and hosts even ones, so neither side's numbering collides with the
//! other's. The receiver starts the sender with [`INITIAL_CREDIT`] frames
//! and grants more as it consumes, so one slow transfer can never fill the
//! connection's outbound queue. The receiver answers with its own `Close`
//! once it has the bytes, or earlier to refuse them, so a sender that
//! needs an acknowledgement can wait for one.
//!
//! The host sends attachments this way (`session.read_attachment`) and
//! the client sends uploads ahead of `run.send` (see [`crate::uploads`]).
//! A PTY would use the same frames with its own `purpose`.
//!
//! This module holds what both directions share: the sender, and a
//! collector that gathers one stream's bytes under a limit. How a stream
//! is paired with the request or upload it belongs to is each side's own.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::frame::{Frame, FrameKind, MAX_STREAM_FRAME_BYTES};
use crate::outbound::Outbound;

/// Frames a sender may have in flight when a stream opens.
pub const INITIAL_CREDIT: u32 = 16;
/// The receiver grants this many more frames each time it has consumed
/// this many.
pub const CREDIT_REFILL: u32 = 8;

/// The host's own limit per image, which bounds a stream in either
/// direction.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// `purpose` of a stream the host opens to answer `session.read_attachment`.
pub const ATTACHMENT_PURPOSE: &str = "attachment";
/// `purpose` of a stream the client opens to upload an image ahead of
/// `run.send`.
pub const UPLOAD_PURPOSE: &str = "upload";

/// Payload of an `Open` frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamOpen {
    /// What the bytes are: [`ATTACHMENT_PURPOSE`] or [`UPLOAD_PURPOSE`].
    pub purpose: String,
    /// For an attachment: the JSON-RPC request this stream answers, so
    /// the receiver can pair the bytes with the response that names the
    /// channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    /// For an upload: the id the client minted, which its `run.send`
    /// names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
    /// For an upload: the media type of the bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// Total bytes, when known up front. The receiver holds the sender to
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
}

/// Payload of a `Close` frame that ends or refuses a stream early.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamClose {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Which side opened a channel. Clients take the odd channels and hosts
/// the even ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opener {
    Client,
    Host,
}

impl Opener {
    pub fn of_channel(channel: u16) -> Self {
        if channel % 2 == 1 {
            Self::Client
        } else {
            Self::Host
        }
    }

    fn first_channel(self) -> u16 {
        match self {
            Self::Client => 1,
            Self::Host => 2,
        }
    }
}

fn credit_frame(channel: u16, credit: u32) -> Frame {
    Frame {
        channel,
        kind: FrameKind::Credit,
        payload: Bytes::copy_from_slice(&credit.to_be_bytes()),
    }
}

pub fn decode_credit(payload: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = payload.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// A `Close` frame: empty to end or acknowledge a stream, or naming the
/// error that ends it early.
pub fn close_frame(channel: u16, error: Option<&str>) -> Frame {
    let payload = match error {
        Some(error) => serde_json::to_vec(&StreamClose {
            error: Some(error.to_string()),
        })
        .unwrap_or_default(),
        None => Vec::new(),
    };
    Frame {
        channel,
        kind: FrameKind::Close,
        payload: Bytes::from(payload),
    }
}

/// The error in a `Close` payload, if it names one.
fn close_error(payload: &[u8]) -> Option<String> {
    if payload.is_empty() {
        return None;
    }
    serde_json::from_slice::<StreamClose>(payload)
        .ok()
        .and_then(|close| close.error)
}

// ---- Sending side -------------------------------------------------------------

/// What the peer tells a sender about its stream.
enum Signal {
    Credit(u32),
    /// The peer closed the stream: acknowledged it, or refused it with
    /// the error.
    Closed(Result<(), String>),
}

type Signals = Arc<Mutex<HashMap<u16, mpsc::UnboundedSender<Signal>>>>;

/// One open stream the local side is sending on.
pub struct StreamSender {
    channel: u16,
    out: Outbound,
    signals: mpsc::UnboundedReceiver<Signal>,
    registry: Signals,
    available: u32,
    /// The `Close` frame went out; dropping the sender sends nothing.
    finished: bool,
}

impl StreamSender {
    pub fn channel(&self) -> u16 {
        self.channel
    }

    /// Send every byte in frames of at most [`MAX_STREAM_FRAME_BYTES`],
    /// waiting for credit between frames, then close the stream. Fails
    /// when the peer closes the stream first.
    pub async fn send_all(&mut self, bytes: &[u8]) -> Result<(), String> {
        for chunk in bytes.chunks(MAX_STREAM_FRAME_BYTES) {
            while self.available == 0 {
                match self.signals.recv().await {
                    Some(Signal::Credit(credit)) => self.available += credit,
                    Some(Signal::Closed(Err(error))) => return Err(error),
                    Some(Signal::Closed(Ok(()))) => {
                        return Err("the receiver closed the stream early".to_string());
                    }
                    None => return Err("the connection ended".to_string()),
                }
            }
            self.out.try_send(Frame {
                channel: self.channel,
                kind: FrameKind::Data,
                payload: Bytes::copy_from_slice(chunk),
            })?;
            self.available -= 1;
        }
        self.out.try_send(close_frame(self.channel, None))?;
        self.finished = true;
        Ok(())
    }

    /// Wait for the peer's `Close`: its acknowledgement that it took the
    /// bytes, or the error it refused them with.
    pub async fn wait_for_ack(mut self) -> Result<(), String> {
        loop {
            match self.signals.recv().await {
                Some(Signal::Credit(_)) => continue,
                Some(Signal::Closed(outcome)) => return outcome,
                None => return Err("the connection ended".to_string()),
            }
        }
    }
}

impl Drop for StreamSender {
    /// A sender dropped mid-stream tells the peer to forget the bytes,
    /// so a timed-out transfer does not sit in the peer's limits until
    /// the connection ends.
    fn drop(&mut self) {
        if !self.finished {
            let _ = self
                .out
                .try_send(close_frame(self.channel, Some("the sender gave up")));
        }
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.channel);
    }
}

/// The streams one connection is sending, keyed by channel, so the peer's
/// `Credit` and `Close` frames reach the right sender.
pub struct StreamSenders {
    opener: Opener,
    next_channel: AtomicU16,
    signals: Signals,
}

impl StreamSenders {
    pub fn new(opener: Opener) -> Self {
        Self {
            opener,
            next_channel: AtomicU16::new(opener.first_channel()),
            signals: Arc::default(),
        }
    }

    /// Open a stream: queues the `Open` frame and returns the sender, which
    /// starts with [`INITIAL_CREDIT`].
    pub fn open(&self, out: &Outbound, open: StreamOpen) -> Result<StreamSender, String> {
        let payload = serde_json::to_vec(&open).map_err(|error| error.to_string())?;
        let channel = self.allocate_channel();
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        self.signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(channel, signal_tx);
        if let Err(error) = out.try_send(Frame {
            channel,
            kind: FrameKind::Open,
            payload: Bytes::from(payload),
        }) {
            self.signals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&channel);
            return Err(error);
        }
        Ok(StreamSender {
            channel,
            out: out.clone(),
            signals: signal_rx,
            registry: Arc::clone(&self.signals),
            available: INITIAL_CREDIT,
            finished: false,
        })
    }

    fn allocate_channel(&self) -> u16 {
        loop {
            // Stepping by two keeps this side's parity; wrapping past
            // u16::MAX keeps it too, since 65536 is even.
            let channel = self.next_channel.fetch_add(2, Ordering::Relaxed);
            if channel != 0 {
                return channel;
            }
        }
    }

    /// This side's parity.
    pub fn opener(&self) -> Opener {
        self.opener
    }

    fn signal(&self, channel: u16, signal: Signal) {
        let mut signals = self
            .signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sender) = signals.get(&channel)
            && sender.send(signal).is_err()
        {
            signals.remove(&channel);
        }
    }

    /// The peer granted `credit` more frames on `channel`.
    pub fn credit(&self, channel: u16, credit: u32) {
        self.signal(channel, Signal::Credit(credit));
    }

    /// The peer closed `channel`: acknowledged the bytes, or refused them.
    pub fn on_close(&self, channel: u16, payload: &[u8]) {
        let outcome = match close_error(payload) {
            Some(error) => Err(error),
            None => Ok(()),
        };
        self.signal(channel, Signal::Closed(outcome));
    }

    /// The connection ended: every sender still waiting fails.
    pub fn fail_all(&self, reason: &str) {
        let signals = std::mem::take(
            &mut *self
                .signals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (_, sender) in signals {
            let _ = sender.send(Signal::Closed(Err(reason.to_string())));
        }
    }
}

// ---- Receiving side -----------------------------------------------------------

struct Collector<K> {
    key: K,
    bytes: Vec<u8>,
    declared_len: Option<u64>,
    consumed_since_credit: u32,
}

/// The collected bytes of one stream, or why it ended early.
pub type StreamResult = Result<Vec<u8>, String>;

/// The streams one connection is receiving, keyed by channel. Bytes are
/// collected whole under a byte limit and the length the sender declared;
/// a stream that crosses either is dropped and its key returned, so the
/// side can tell the sender. `K` names what a stream belongs to: the
/// request it answers on the client, the upload id on the host.
pub struct StreamReceivers<K> {
    open: Mutex<HashMap<u16, Collector<K>>>,
    max_bytes: usize,
}

impl<K: Clone + PartialEq> StreamReceivers<K> {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            open: Mutex::new(HashMap::new()),
            max_bytes,
        }
    }

    /// Start collecting `channel` under `key`. Refuses a declared length
    /// over the limit and a channel already collecting.
    pub fn accept(&self, channel: u16, key: K, len: Option<u64>) -> Result<(), String> {
        if let Some(len) = len
            && len > self.max_bytes as u64
        {
            return Err(format!(
                "{len} bytes exceeds the {} byte limit",
                self.max_bytes
            ));
        }
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if open.contains_key(&channel) {
            return Err(format!("channel {channel} is already open"));
        }
        open.insert(
            channel,
            Collector {
                key,
                bytes: Vec::with_capacity(len.unwrap_or(0) as usize),
                declared_len: len,
                consumed_since_credit: 0,
            },
        );
        Ok(())
    }

    /// A `Data` frame arrived. Returns a credit frame to send back when the
    /// sender has earned more, or the key and reason of a stream that
    /// crossed its limit, which is dropped.
    pub fn on_data(&self, channel: u16, payload: &[u8]) -> Result<Option<Frame>, (K, String)> {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(collector) = open.get_mut(&channel) else {
            return Ok(None);
        };
        let total = collector.bytes.len() + payload.len();
        let over = if collector
            .declared_len
            .is_some_and(|declared| total as u64 > declared)
        {
            Some(format!(
                "more bytes than the {} declared",
                collector.declared_len.unwrap_or(0)
            ))
        } else if total > self.max_bytes {
            Some(format!("exceeds the {} byte limit", self.max_bytes))
        } else {
            None
        };
        if let Some(reason) = over {
            let collector = open.remove(&channel).expect("just found");
            return Err((collector.key, reason));
        }
        collector.bytes.extend_from_slice(payload);
        collector.consumed_since_credit += 1;
        if collector.consumed_since_credit >= CREDIT_REFILL {
            collector.consumed_since_credit = 0;
            return Ok(Some(credit_frame(channel, CREDIT_REFILL)));
        }
        Ok(None)
    }

    /// A `Close` frame arrived: the bytes are complete, the sender
    /// reported an error, or the stream ended short of its declared
    /// length.
    pub fn on_close(&self, channel: u16, payload: &[u8]) -> Option<(K, StreamResult)> {
        let collector = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&channel)?;
        let result = match close_error(payload) {
            Some(error) => Err(error),
            None => match collector.declared_len {
                Some(declared) if declared != collector.bytes.len() as u64 => Err(format!(
                    "ended after {} of the {declared} bytes declared",
                    collector.bytes.len()
                )),
                _ => Ok(collector.bytes),
            },
        };
        Some((collector.key, result))
    }

    /// Drop whatever is collecting under `key`, so late frames on that
    /// channel are dropped instead of kept.
    pub fn abandon(&self, key: &K) {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, collector| collector.key != *key);
    }

    /// Streams still collecting.
    pub fn open_count(&self) -> usize {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// The connection ended: drop every open stream.
    pub fn clear(&self) {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbound;

    fn open(len: Option<u64>) -> StreamOpen {
        StreamOpen {
            purpose: ATTACHMENT_PURPOSE.into(),
            request_id: Some(9),
            upload_id: None,
            mime: None,
            len,
        }
    }

    #[tokio::test]
    async fn a_stream_crosses_with_credit_and_collects_whole() {
        let (out, mut queue) = outbound::channel(usize::MAX);
        let senders = StreamSenders::new(Opener::Host);
        let receivers = StreamReceivers::<u64>::new(MAX_IMAGE_BYTES);
        let payload = vec![7u8; MAX_STREAM_FRAME_BYTES * 20 + 5];
        let mut sender = senders
            .open(&out, open(Some(payload.len() as u64)))
            .unwrap();
        let channel = sender.channel();
        let sending = tokio::spawn(async move {
            sender.send_all(&payload).await?;
            sender.wait_for_ack().await
        });

        // Pump frames from the sender's queue into the receiver, returning
        // credit and the final acknowledgement the way the peer would.
        let delivered = loop {
            let frame = queue.recv().await.expect("frame");
            match frame.kind {
                FrameKind::Open => {
                    let open: StreamOpen = serde_json::from_slice(&frame.payload).unwrap();
                    receivers
                        .accept(frame.channel, open.request_id.unwrap(), open.len)
                        .unwrap();
                }
                FrameKind::Data => {
                    if let Some(credit) = receivers.on_data(frame.channel, &frame.payload).unwrap()
                    {
                        senders.credit(credit.channel, decode_credit(&credit.payload).unwrap());
                    }
                }
                FrameKind::Close => {
                    let delivered = receivers.on_close(frame.channel, &frame.payload);
                    senders.on_close(frame.channel, &[]);
                    break delivered;
                }
                FrameKind::Credit => unreachable!(),
            }
        };
        sending.await.unwrap().unwrap();
        assert_eq!(channel, 2, "hosts open even channels");
        assert_eq!(Opener::of_channel(channel), Opener::Host);
        let (key, bytes) = delivered.unwrap();
        assert_eq!(key, 9);
        let bytes = bytes.unwrap();
        assert_eq!(bytes.len(), MAX_STREAM_FRAME_BYTES * 20 + 5);
        assert!(bytes.iter().all(|byte| *byte == 7));
        assert_eq!(receivers.open_count(), 0);
    }

    #[tokio::test]
    async fn a_dropped_sender_reports_an_error_and_connection_loss_fails_the_rest() {
        let (out, mut queue) = outbound::channel(usize::MAX);
        let senders = StreamSenders::new(Opener::Client);
        let receivers = StreamReceivers::<u64>::new(MAX_IMAGE_BYTES);
        let sender = senders.open(&out, open(None)).unwrap();
        assert_eq!(sender.channel(), 1, "clients open odd channels");
        drop(sender);
        let open_frame = queue.recv().await.unwrap();
        receivers.accept(open_frame.channel, 1, None).unwrap();
        let close = queue.recv().await.unwrap();
        assert_eq!(close.kind, FrameKind::Close);
        let (key, result) = receivers.on_close(close.channel, &close.payload).unwrap();
        assert_eq!(key, 1);
        assert_eq!(result, Err("the sender gave up".to_string()));

        let other = senders.open(&out, open(None)).unwrap();
        assert_eq!(other.channel(), 3);
        senders.fail_all("connection lost");
        assert_eq!(other.wait_for_ack().await.unwrap_err(), "connection lost");
        receivers.accept(5, 2, None).unwrap();
        receivers.clear();
        assert_eq!(receivers.open_count(), 0);
    }

    #[tokio::test]
    async fn a_refusal_stops_the_sender() {
        let (out, mut queue) = outbound::channel(usize::MAX);
        let senders = StreamSenders::new(Opener::Client);
        let mut sender = senders.open(&out, open(None)).unwrap();
        let channel = sender.channel();
        senders.on_close(channel, &close_frame(channel, Some("too big")).payload);
        // Enough frames to run out of credit and hear the refusal.
        let payload = vec![0u8; MAX_STREAM_FRAME_BYTES * (INITIAL_CREDIT as usize + 1)];
        assert_eq!(sender.send_all(&payload).await.unwrap_err(), "too big");
        drop(sender);
        let mut kinds = Vec::new();
        while let Some(frame) = queue.try_recv() {
            kinds.push(frame.kind);
        }
        assert_eq!(kinds[0], FrameKind::Open);
        assert_eq!(
            *kinds.last().unwrap(),
            FrameKind::Close,
            "the drop tells the peer"
        );
    }

    #[test]
    fn a_receiver_holds_the_sender_to_its_limits() {
        let receivers = StreamReceivers::<&str>::new(10);
        assert!(receivers.accept(1, "big", Some(11)).is_err());
        receivers.accept(1, "declared", Some(4)).unwrap();
        assert!(
            receivers.accept(1, "again", None).is_err(),
            "channel in use"
        );
        assert_eq!(receivers.on_data(1, b"12345").unwrap_err().0, "declared");
        assert!(receivers.on_data(1, b"late").unwrap().is_none());

        receivers.accept(3, "short", Some(4)).unwrap();
        assert!(receivers.on_data(3, b"12").unwrap().is_none());
        let (key, result) = receivers.on_close(3, &[]).unwrap();
        assert_eq!(key, "short");
        assert!(result.unwrap_err().contains("2 of the 4"));

        receivers.accept(5, "unbounded", None).unwrap();
        assert!(receivers.on_data(5, &[0; 10]).unwrap().is_none());
        assert_eq!(receivers.on_data(5, b"1").unwrap_err().0, "unbounded");

        receivers.accept(7, "abandoned", None).unwrap();
        receivers.abandon(&"abandoned");
        assert_eq!(receivers.open_count(), 0);
        assert!(receivers.on_close(7, &[]).is_none());
    }
}
