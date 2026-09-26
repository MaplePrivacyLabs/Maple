//! The client role on the network: dialing a host over plain WebSocket
//! with Noise inside.
//!
//! [`connect_direct`] dials a host by address, either to pair with a code
//! or to reconnect with the host's pinned static key. The relay is another
//! connector later; everything above the carrier is shared.

use crate::carrier::Carrier;
use crate::keys::{StaticKey, decode_key, encode_key};
use crate::net::{HANDSHAKE_TIMEOUT, websocket_config};
use crate::noise::{self, Initiate};
use crate::pairing::PairingCode;

/// What to dial for.
pub enum ConnectTarget {
    /// First contact: pair with the code the host published.
    Pair(PairingCode),
    /// A host already paired, whose static key is pinned.
    Host { host_key: String },
}

/// A carrier to the host at `address` plus the host's static key, which
/// the client pins after pairing and verifies afterwards.
pub struct Dialed {
    pub carrier: Carrier,
    pub host_key: String,
}

/// Dial `address` (`host:port`) over plain WebSocket and run the Noise
/// handshake as `device`.
pub async fn connect_direct(
    address: &str,
    device: &StaticKey,
    target: ConnectTarget,
) -> Result<Dialed, String> {
    let url = format!("ws://{address}/");
    let (socket, _) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::connect_async_with_config(&url, Some(websocket_config()), true),
    )
    .await
    .map_err(|_| format!("connecting to {address} timed out"))?
    .map_err(|error| format!("cannot connect to {address}: {error}"))?;
    let initiate = match &target {
        ConnectTarget::Pair(code) => Initiate::Pair { psk: code.psk() },
        ConnectTarget::Host { host_key } => Initiate::Session {
            host_static: decode_key(host_key)?,
        },
    };
    let established = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        noise::initiate(socket, device.private(), initiate),
    )
    .await
    .map_err(|_| "the host did not finish the handshake in time".to_string())??;
    let host_key = encode_key(&established.remote_static);
    if let ConnectTarget::Host { host_key: pinned } = &target
        && pinned != &host_key
    {
        return Err("the host's key does not match the pinned key".to_string());
    }
    Ok(Dialed {
        carrier: established.carrier,
        host_key,
    })
}
