//! Framing inside one encrypted message.
//!
//! Every message after the handshake is one frame:
//! `[channel: u16 BE][kind: u8][payload]`. Channel 0 is the control
//! channel and carries one JSON-RPC message per `Data` frame. Channels 1
//! and up are binary streams opened by the side that sends the data:
//! clients open odd channels, hosts even ones. See [`crate::streams`].

use bytes::{BufMut, Bytes, BytesMut};

/// The control channel: JSON-RPC 2.0.
pub const CONTROL_CHANNEL: u16 = 0;

/// Largest control frame either side accepts. Anything larger belongs on
/// a stream or must be paged.
pub const MAX_CONTROL_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Largest data frame on a binary stream.
pub const MAX_STREAM_FRAME_BYTES: usize = 256 * 1024;

/// Bytes in the `[channel][kind]` header before the payload.
pub const HEADER_BYTES: usize = 3;

/// Largest payload a frame on `channel` may carry.
pub fn max_payload_bytes(channel: u16) -> usize {
    if channel == CONTROL_CHANNEL {
        MAX_CONTROL_FRAME_BYTES
    } else {
        MAX_STREAM_FRAME_BYTES
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// Opens a stream. Payload: a JSON [`crate::streams::StreamOpen`].
    Open = 0,
    /// Bytes on a stream, or one JSON-RPC message on channel 0.
    Data = 1,
    /// Ends a stream. From the sender: the bytes are complete, or a JSON
    /// [`crate::streams::StreamClose`] names why it stopped. From the
    /// receiver: it took the bytes, or a `StreamClose` names why it
    /// refused them.
    Close = 2,
    /// The receiver grants the sender more `Data` frames on a stream.
    /// Payload: a u32 BE count.
    Credit = 3,
}

impl FrameKind {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Open),
            1 => Some(Self::Data),
            2 => Some(Self::Close),
            3 => Some(Self::Credit),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub channel: u16,
    pub kind: FrameKind,
    pub payload: Bytes,
}

impl Frame {
    pub fn control(payload: impl Into<Bytes>) -> Self {
        Self {
            channel: CONTROL_CHANNEL,
            kind: FrameKind::Data,
            payload: payload.into(),
        }
    }

    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(HEADER_BYTES + self.payload.len());
        out.put_u16(self.channel);
        out.put_u8(self.kind as u8);
        out.extend_from_slice(&self.payload);
        out.freeze()
    }

    /// Decode one frame. Rejects a short header, an unknown kind, and a
    /// payload over the limit for its channel.
    pub fn decode(bytes: Bytes) -> Result<Self, String> {
        if bytes.len() < HEADER_BYTES {
            return Err("frame shorter than its header".to_string());
        }
        let channel = u16::from_be_bytes([bytes[0], bytes[1]]);
        let kind = FrameKind::from_byte(bytes[2])
            .ok_or_else(|| format!("unknown frame kind {}", bytes[2]))?;
        let payload = bytes.slice(HEADER_BYTES..);
        let limit = max_payload_bytes(channel);
        if payload.len() > limit {
            return Err(format!(
                "frame of {} bytes on channel {channel} exceeds the {limit} byte limit",
                payload.len()
            ));
        }
        Ok(Self {
            channel,
            kind,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frame = Frame {
            channel: 7,
            kind: FrameKind::Credit,
            payload: Bytes::from_static(&[0, 0, 0, 16]),
        };
        let decoded = Frame::decode(frame.encode()).unwrap();
        assert_eq!(decoded, frame);
        let control = Frame::control(r#"{"jsonrpc":"2.0"}"#);
        assert_eq!(control.channel, CONTROL_CHANNEL);
        assert_eq!(Frame::decode(control.encode()).unwrap(), control);
    }

    #[test]
    fn malformed_frames_are_refused() {
        assert!(Frame::decode(Bytes::from_static(&[0, 0])).is_err());
        assert!(Frame::decode(Bytes::from_static(&[0, 0, 9])).is_err());
        let oversized = Frame {
            channel: 3,
            kind: FrameKind::Data,
            payload: Bytes::from(vec![0u8; MAX_STREAM_FRAME_BYTES + 1]),
        };
        assert!(Frame::decode(oversized.encode()).is_err());
    }
}
