//! Noise inside a WebSocket: the encrypted carrier.
//!
//! Two handshakes. Pairing runs `XXpsk3` with the one-time code as the
//! pre-shared key: both sides send their static keys, and the code is what
//! authenticates the exchange. Every later connection runs `IK`: the client
//! knows the host's static key, sends its own encrypted in the first
//! message, and the host accepts it only if that key is paired. After
//! either handshake both sides hold the other's static key to pin.
//!
//! A relay in between sees only the handshake's ciphertext and the
//! transport messages. The first byte of the first message names the
//! handshake and is also the Noise prologue, so a relay cannot swap one
//! for the other.
//!
//! In the pairing pattern the client sends the last handshake message, so
//! it could not tell a wrong code from success until the host dropped it.
//! The host therefore sends one empty transport message once its side
//! completes and it has spent the code and recorded the device; the
//! client must decrypt it before it trusts the session.
//!
//! Noise transport messages hold at most 65535 bytes, so a frame is cut
//! into pieces; each piece carries one continuation byte before the
//! frame bytes. Every WebSocket binary message is exactly one Noise message.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::carrier::{Carrier, FrameSink, FrameStream};
use crate::frame::{Frame, HEADER_BYTES, MAX_CONTROL_FRAME_BYTES};

/// Pairing: statics exchanged, authenticated by the pre-shared code.
pub const PAIRING_PATTERN: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
/// Every later connection: the host's static is known and pinned.
pub const SESSION_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

const MODE_PAIR: u8 = 1;
const MODE_SESSION: u8 = 2;
const PROLOGUE_PAIR: &[u8] = b"maple-remote-v1/pair";
const PROLOGUE_SESSION: &[u8] = b"maple-remote-v1/session";

/// Largest plaintext one Noise message carries: 65535 minus the 16-byte
/// tag, minus the continuation byte.
const PIECE_BYTES: usize = 65535 - 16 - 1;
const MORE: u8 = 1;
const LAST: u8 = 0;

/// Largest frame a peer may reassemble from pieces: the biggest control
/// frame plus its header. Stream frames are smaller still.
const MAX_REASSEMBLED_BYTES: usize = HEADER_BYTES + MAX_CONTROL_FRAME_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeMode {
    Pair,
    Session,
}

impl HandshakeMode {
    fn byte(self) -> u8 {
        match self {
            Self::Pair => MODE_PAIR,
            Self::Session => MODE_SESSION,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            MODE_PAIR => Some(Self::Pair),
            MODE_SESSION => Some(Self::Session),
            _ => None,
        }
    }

    fn pattern(self) -> &'static str {
        match self {
            Self::Pair => PAIRING_PATTERN,
            Self::Session => SESSION_PATTERN,
        }
    }

    fn prologue(self) -> &'static [u8] {
        match self {
            Self::Pair => PROLOGUE_PAIR,
            Self::Session => PROLOGUE_SESSION,
        }
    }
}

/// What the initiator brings to a handshake.
pub enum Initiate {
    /// Pair with the code the host published.
    Pair { psk: [u8; 32] },
    /// Connect to a host whose static key is pinned.
    Session { host_static: [u8; 32] },
}

/// What the responder needs to answer a handshake.
pub struct Respond<'a> {
    /// The pre-shared key for a pairing attempt, when one is pending.
    pub pairing_psk: Option<[u8; 32]>,
    /// Whether a device's static key is paired, for a session handshake.
    pub is_paired: &'a (dyn Fn(&[u8; 32]) -> bool + Send + Sync),
    /// Called with the device's static key once a pairing handshake
    /// completes and before the client is told its code was right. It
    /// spends the code and records the device; an error refuses the
    /// pairing, so two clients racing on one code cannot both succeed.
    pub confirm_pairing: &'a (dyn Fn(&[u8; 32]) -> Result<(), String> + Send + Sync),
}

/// Why the host side of a handshake failed.
#[derive(Debug)]
pub struct HandshakeRefused {
    /// The handshake the client asked for, once its first byte was read.
    /// `None` when the failure came before that.
    pub mode: Option<HandshakeMode>,
    pub message: String,
}

impl std::fmt::Display for HandshakeRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The outcome of a handshake: the encrypted carrier and the peer's static
/// key.
pub struct Established {
    pub carrier: Carrier,
    pub remote_static: [u8; 32],
    pub mode: HandshakeMode,
}

fn builder<'a>(
    mode: HandshakeMode,
    local_private: &'a [u8; 32],
) -> Result<snow::Builder<'a>, String> {
    let params = mode
        .pattern()
        .parse()
        .map_err(|error| format!("noise pattern: {error}"))?;
    snow::Builder::new(params)
        .prologue(mode.prologue())
        .map_err(|error| format!("noise prologue: {error}"))?
        .local_private_key(local_private)
        .map_err(|error| format!("noise local key: {error}"))
}

fn remote_static(state: &snow::HandshakeState) -> Result<[u8; 32], String> {
    state
        .get_remote_static()
        .and_then(|key| key.try_into().ok())
        .ok_or_else(|| "the peer sent no static key".to_string())
}

/// Run the client side of a handshake over `socket`.
pub async fn initiate<S>(
    mut socket: WebSocketStream<S>,
    local_private: &[u8; 32],
    initiate: Initiate,
) -> Result<Established, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mode, mut state) = match initiate {
        Initiate::Pair { psk } => (
            HandshakeMode::Pair,
            builder(HandshakeMode::Pair, local_private)?
                .psk(3, &psk)
                .map_err(|error| format!("noise psk: {error}"))?
                .build_initiator()
                .map_err(|error| format!("noise: {error}"))?,
        ),
        Initiate::Session { host_static } => (
            HandshakeMode::Session,
            builder(HandshakeMode::Session, local_private)?
                .remote_public_key(&host_static)
                .map_err(|error| format!("noise remote key: {error}"))?
                .build_initiator()
                .map_err(|error| format!("noise: {error}"))?,
        ),
    };
    let mut buffer = vec![0u8; 1024];
    let mut first = true;
    while !state.is_handshake_finished() {
        if state.is_my_turn() {
            let written = state
                .write_message(&[], &mut buffer)
                .map_err(|error| format!("noise handshake: {error}"))?;
            let mut message = Vec::with_capacity(written + 1);
            if first {
                message.push(mode.byte());
                first = false;
            }
            message.extend_from_slice(&buffer[..written]);
            socket
                .send(Message::Binary(message.into()))
                .await
                .map_err(|error| format!("cannot send the handshake: {error}"))?;
        } else {
            let message = next_binary(&mut socket)
                .await?
                .ok_or_else(|| "the host closed during the handshake".to_string())?;
            state.read_message(&message, &mut buffer).map_err(|_| {
                "the host refused the handshake: wrong pairing code, or this device is not paired"
                    .to_string()
            })?;
        }
    }
    let remote = remote_static(&state)?;
    let mut transport = state
        .into_transport_mode()
        .map_err(|error| format!("noise transport: {error}"))?;
    if mode == HandshakeMode::Pair {
        let confirmation = next_binary(&mut socket)
            .await?
            .ok_or_else(|| "the host refused the pairing code".to_string())?;
        let mut out = vec![0u8; confirmation.len()];
        let read = transport
            .read_message(&confirmation, &mut out)
            .map_err(|_| "the host refused the pairing code".to_string())?;
        if read != 0 {
            return Err("unexpected data before the pairing completed".to_string());
        }
    }
    Ok(Established {
        carrier: carrier(socket, transport),
        remote_static: remote,
        mode,
    })
}

/// Run the host side of a handshake over `socket`.
pub async fn respond<S>(
    mut socket: WebSocketStream<S>,
    local_private: &[u8; 32],
    respond: Respond<'_>,
) -> Result<Established, HandshakeRefused>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let before_mode = |message: String| HandshakeRefused {
        mode: None,
        message,
    };
    let first = next_binary(&mut socket)
        .await
        .map_err(before_mode)?
        .ok_or_else(|| before_mode("the client closed before the handshake".to_string()))?;
    let (&mode_byte, first_message) = first
        .split_first()
        .ok_or_else(|| before_mode("empty handshake message".to_string()))?;
    let mode = HandshakeMode::from_byte(mode_byte)
        .ok_or_else(|| before_mode(format!("unknown handshake mode {mode_byte}")))?;
    respond_as(socket, local_private, respond, mode, first_message)
        .await
        .map_err(|message| HandshakeRefused {
            mode: Some(mode),
            message,
        })
}

/// The rest of the host side once the handshake's mode is known.
async fn respond_as<S>(
    mut socket: WebSocketStream<S>,
    local_private: &[u8; 32],
    respond: Respond<'_>,
    mode: HandshakeMode,
    first_message: &[u8],
) -> Result<Established, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut state = match mode {
        HandshakeMode::Pair => {
            let psk = respond
                .pairing_psk
                .ok_or_else(|| "no pairing code is pending".to_string())?;
            builder(mode, local_private)?
                .psk(3, &psk)
                .map_err(|error| format!("noise psk: {error}"))?
                .build_responder()
                .map_err(|error| format!("noise: {error}"))?
        }
        HandshakeMode::Session => builder(mode, local_private)?
            .build_responder()
            .map_err(|error| format!("noise: {error}"))?,
    };
    let mut buffer = vec![0u8; 1024];
    state
        .read_message(first_message, &mut buffer)
        .map_err(|error| format!("handshake refused: {error}"))?;
    if mode == HandshakeMode::Session {
        // IK carries the client's static in its first message; refuse an
        // unpaired device before answering anything.
        let key = remote_static(&state)?;
        if !(respond.is_paired)(&key) {
            return Err("this device is not paired with the host".to_string());
        }
    }
    while !state.is_handshake_finished() {
        if state.is_my_turn() {
            let written = state
                .write_message(&[], &mut buffer)
                .map_err(|error| format!("noise handshake: {error}"))?;
            socket
                .send(Message::Binary(buffer[..written].to_vec().into()))
                .await
                .map_err(|error| format!("cannot send the handshake: {error}"))?;
        } else {
            let message = next_binary(&mut socket)
                .await?
                .ok_or_else(|| "the client closed during the handshake".to_string())?;
            state
                .read_message(&message, &mut buffer)
                .map_err(|error| format!("handshake refused: {error}"))?;
        }
    }
    let remote = remote_static(&state)?;
    let mut transport = state
        .into_transport_mode()
        .map_err(|error| format!("noise transport: {error}"))?;
    if mode == HandshakeMode::Pair {
        // Spend the code and record the device, then tell the client its
        // code was right; see the module docs.
        (respond.confirm_pairing)(&remote)?;
        let mut out = vec![0u8; 16];
        let written = transport
            .write_message(&[], &mut out)
            .map_err(|error| format!("noise confirm: {error}"))?;
        socket
            .send(Message::Binary(out[..written].to_vec().into()))
            .await
            .map_err(|error| format!("cannot confirm the pairing: {error}"))?;
    }
    Ok(Established {
        carrier: carrier(socket, transport),
        remote_static: remote,
        mode,
    })
}

async fn next_binary<S>(socket: &mut WebSocketStream<S>) -> Result<Option<Vec<u8>>, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match socket.next().await {
            Some(Ok(Message::Binary(bytes))) => return Ok(Some(bytes.to_vec())),
            Some(Ok(Message::Close(_))) | None => return Ok(None),
            Some(Ok(_)) => continue,
            Some(Err(error)) => return Err(format!("websocket: {error}")),
        }
    }
}

type Transport = Arc<Mutex<snow::TransportState>>;

fn carrier<S>(socket: WebSocketStream<S>, transport: snow::TransportState) -> Carrier
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let transport = Arc::new(Mutex::new(transport));
    let (sink, stream) = socket.split();
    Carrier {
        sink: Box::new(NoiseSink {
            sink: Some(sink),
            transport: Arc::clone(&transport),
        }),
        stream: Box::new(NoiseStream {
            stream,
            transport,
            reassembly: Reassembly::default(),
        }),
    }
}

struct NoiseSink<S> {
    sink: Option<futures_util::stream::SplitSink<WebSocketStream<S>, Message>>,
    transport: Transport,
}

#[async_trait]
impl<S> FrameSink for NoiseSink<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&mut self, frame: Frame) -> Result<(), String> {
        let sink = self
            .sink
            .as_mut()
            .ok_or_else(|| "carrier closed".to_string())?;
        let encoded = frame.encode();
        let pieces: Vec<&[u8]> = if encoded.is_empty() {
            vec![&[][..]]
        } else {
            encoded.chunks(PIECE_BYTES).collect()
        };
        let count = pieces.len();
        for (index, piece) in pieces.into_iter().enumerate() {
            let mut plaintext = Vec::with_capacity(piece.len() + 1);
            plaintext.push(if index + 1 == count { LAST } else { MORE });
            plaintext.extend_from_slice(piece);
            let ciphertext = {
                let mut transport = self
                    .transport
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut out = vec![0u8; plaintext.len() + 16];
                let written = transport
                    .write_message(&plaintext, &mut out)
                    .map_err(|error| format!("noise encrypt: {error}"))?;
                out.truncate(written);
                out
            };
            sink.send(Message::Binary(ciphertext.into()))
                .await
                .map_err(|error| format!("websocket send: {error}"))?;
        }
        Ok(())
    }

    async fn close(&mut self) {
        if let Some(mut sink) = self.sink.take() {
            let _ = sink.send(Message::Close(None)).await;
            let _ = sink.close().await;
        }
    }
}

/// The frame being put back together from its pieces.
#[derive(Default)]
struct Reassembly {
    partial: Vec<u8>,
}

impl Reassembly {
    /// Take one decrypted piece. `Ok(Some(_))` is a whole frame's bytes,
    /// `Ok(None)` means more pieces follow (an empty piece is ignored), and
    /// `Err` means the peer is sending a frame larger than any it may send.
    fn push(&mut self, plaintext: &[u8]) -> Result<Option<Bytes>, String> {
        let Some((&flag, piece)) = plaintext.split_first() else {
            return Ok(None);
        };
        if self.partial.len() + piece.len() > MAX_REASSEMBLED_BYTES {
            self.partial = Vec::new();
            return Err(format!(
                "frame grew past the {MAX_REASSEMBLED_BYTES} byte limit while reassembling"
            ));
        }
        self.partial.extend_from_slice(piece);
        if flag == MORE {
            return Ok(None);
        }
        Ok(Some(Bytes::from(std::mem::take(&mut self.partial))))
    }
}

struct NoiseStream<S> {
    stream: futures_util::stream::SplitStream<WebSocketStream<S>>,
    transport: Transport,
    reassembly: Reassembly,
}

#[async_trait]
impl<S> FrameStream for NoiseStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn recv(&mut self) -> Option<Frame> {
        loop {
            let ciphertext = match self.stream.next().await {
                Some(Ok(Message::Binary(bytes))) => bytes,
                Some(Ok(Message::Close(_))) | None => return None,
                Some(Ok(_)) => continue,
                Some(Err(error)) => {
                    log::debug!("websocket receive: {error}");
                    return None;
                }
            };
            let plaintext = {
                let mut transport = self
                    .transport
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut out = vec![0u8; ciphertext.len()];
                match transport.read_message(&ciphertext, &mut out) {
                    Ok(read) => {
                        out.truncate(read);
                        out
                    }
                    Err(error) => {
                        log::warn!("noise decrypt failed; closing: {error}");
                        return None;
                    }
                }
            };
            let bytes = match self.reassembly.push(&plaintext) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(error) => {
                    log::warn!("bad frame; closing: {error}");
                    return None;
                }
            };
            match Frame::decode(bytes) {
                Ok(frame) => return Some(frame),
                Err(error) => {
                    log::warn!("bad frame; closing: {error}");
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembly_rejects_a_frame_past_the_control_limit() {
        let mut reassembly = Reassembly::default();
        assert_eq!(reassembly.push(&[]).unwrap(), None);
        let mut more = vec![MORE];
        more.extend_from_slice(&[1, 2]);
        assert_eq!(reassembly.push(&more).unwrap(), None);
        let mut last = vec![LAST];
        last.extend_from_slice(&[3]);
        assert_eq!(
            reassembly.push(&last).unwrap(),
            Some(Bytes::from_static(&[1, 2, 3]))
        );

        let mut piece = vec![MORE];
        piece.extend(std::iter::repeat_n(0u8, PIECE_BYTES));
        let mut total = 0;
        let error = loop {
            match reassembly.push(&piece) {
                Ok(None) => total += PIECE_BYTES,
                Ok(Some(_)) => panic!("MORE pieces never complete a frame"),
                Err(error) => break error,
            }
        };
        assert!(total <= MAX_REASSEMBLED_BYTES);
        assert!(total + PIECE_BYTES > MAX_REASSEMBLED_BYTES);
        assert!(error.contains("limit"), "{error}");
        // The partial frame is dropped with the error.
        assert_eq!(
            reassembly.push(&last).unwrap(),
            Some(Bytes::from_static(&[3]))
        );
    }
}
