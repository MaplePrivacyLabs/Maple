//! The host role on the network: listening over plain WebSocket with
//! Noise inside.
//!
//! [`serve_listener`] accepts TCP connections on behalf of one
//! [`HostServer`], runs the handshake, registers a newly paired device,
//! and hands each established carrier to the server. Pairing failures are
//! counted per source address here; see [`PairingLimiter`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::devices::DeviceStore;
use crate::keys::{StaticKey, encode_key};
use crate::net::{HANDSHAKE_TIMEOUT, websocket_config};
use crate::noise::{self, HandshakeMode, Respond};
use crate::pairing::{PairingCode, PairingLimiter, PendingPairingStore};
use crate::server::HostServer;

/// The name a device carries until its first hello names it.
const UNNAMED_DEVICE: &str = "new device";

/// What a listening host needs besides the server: its key and its
/// device and pairing records.
pub struct HostStores {
    pub key: StaticKey,
    pub devices: Arc<DeviceStore>,
    pub pending_pairing: Arc<PendingPairingStore>,
    pub limiter: PairingLimiter,
}

/// Accept connections until `shutdown` fires.
pub async fn serve_listener(
    listener: TcpListener,
    server: Arc<HostServer>,
    stores: Arc<HostStores>,
    shutdown: CancellationToken,
) -> Result<(), String> {
    log::info!(
        "listening on {} as host {}",
        listener
            .local_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_default(),
        stores.key.public_id()
    );
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    log::warn!("accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = shutdown.cancelled() => return Ok(()),
        };
        let server = Arc::clone(&server);
        let stores = Arc::clone(&stores);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, peer, server, stores, shutdown).await {
                log::info!("connection from {peer} ended: {error}");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    server: Arc<HostServer>,
    stores: Arc<HostStores>,
    shutdown: CancellationToken,
) -> Result<(), String> {
    let _ = stream.set_nodelay(true);
    // One deadline covers both handshakes.
    let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
    let socket = tokio::time::timeout_at(
        deadline,
        tokio_tungstenite::accept_async_with_config(stream, Some(websocket_config())),
    )
    .await
    .map_err(|_| "handshake timed out".to_string())?
    .map_err(|error| format!("websocket accept: {error}"))?;
    let pending_code = match stores.pending_pairing.current() {
        Some(pending) if stores.limiter.allows(peer.ip()) => Some(pending.code()?),
        Some(_) => {
            log::warn!("pairing attempts from {peer} are rate limited");
            None
        }
        None => None,
    };
    let devices = Arc::clone(&stores.devices);
    let is_paired = move |key: &[u8; 32]| devices.is_paired(&encode_key(key));
    let confirm_pairing = |key: &[u8; 32]| -> Result<(), String> {
        let code = pending_code
            .as_ref()
            .ok_or_else(|| "no pairing code is pending".to_string())?;
        stores.pending_pairing.consume_if(code)?;
        stores.devices.insert(&encode_key(key), UNNAMED_DEVICE)?;
        Ok(())
    };
    let established = tokio::time::timeout_at(
        deadline,
        noise::respond(
            socket,
            stores.key.private(),
            Respond {
                pairing_psk: pending_code.as_ref().map(PairingCode::psk),
                is_paired: &is_paired,
                confirm_pairing: &confirm_pairing,
            },
        ),
    )
    .await
    .map_err(|_| "handshake timed out".to_string())?;
    let established = match established {
        Ok(established) => established,
        Err(refused) => {
            // Only a failed pairing counts against the address: a wrong
            // code and a probe of the pairing pattern look the same. A
            // session handshake a revoked device keeps retrying must not
            // lock its address out of pairing again.
            if refused.mode == Some(HandshakeMode::Pair) {
                stores.limiter.record_failure(peer.ip());
            }
            return Err(refused.message);
        }
    };
    let device_key = encode_key(&established.remote_static);
    if established.mode == HandshakeMode::Pair {
        log::info!("paired device {device_key} from {peer}");
    }
    let connection = shutdown.child_token();
    // Revocation is an edit to the device file; a revoked device's live
    // connection ends at the next check.
    let revocation_watch = {
        let devices = Arc::clone(&stores.devices);
        let key = device_key.clone();
        let connection = connection.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if !devices.is_paired(&key) {
                    log::info!("device {key} was revoked; disconnecting");
                    connection.cancel();
                    return;
                }
            }
        })
    };
    let result = server
        .serve_with_peer(established.carrier, Some(device_key), connection)
        .await;
    revocation_watch.abort();
    result
}
