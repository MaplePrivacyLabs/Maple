//! The wire between a Maple client and a host.
//!
//! Layers, bottom up:
//!
//! - [`carrier`]: a bidirectional stream of [`frame::Frame`]s. The
//!   in-process pair is for tests; [`noise`] delivers the same frames over
//!   a WebSocket with Noise inside, [`listen`] accepts them for a host,
//!   [`dial`] opens them for a client, and [`net`] holds what both share.
//! - [`keys`], [`pairing`], [`devices`]: the static key of a host or a
//!   device, the one-time pairing code, and the host's paired device list.
//! - [`hosts`], [`manager`]: the client's saved hosts, and the connectors
//!   that keep them connected and forward their events.
//! - [`frame`]: `[channel][kind][payload]`. Channel 0 is control and carries
//!   JSON-RPC 2.0 ([`rpc`]). Other channels are binary streams with
//!   credit-based flow control ([`streams`]); the host keeps the images a
//!   client streams ahead of `run.send` in [`uploads`]. Every frame a side
//!   sends goes through one byte-bounded queue ([`outbound`]); overflow
//!   closes the connection rather than blocking the host.
//! - [`wire`]: the methods, grouped by domain, and the handshake.
//! - [`server`]: [`server::HostServer`] publishes any
//!   [`maple_agent::host::HostBackend`] to connections.
//! - [`client`]: [`client::RemoteHostBackend`] implements `HostBackend` over
//!   a connection, so the UI drives a remote host exactly like the local one.
//!
//! Compatibility rules for everything in [`wire`]: schemas are append-only;
//! new fields are optional with a serde default; unknown fields are
//! ignored; a field that stops being sent stays accepted. Every shim is
//! tagged `COMPAT(name): added in vX.Y, remove after YYYY-MM-DD`. Real
//! evolution goes through the feature bags in the handshake; the protocol
//! version is a tripwire that is bumped only for a change no feature flag
//! can express.

pub mod carrier;
pub mod client;
pub mod devices;
pub mod dial;
pub mod frame;
pub mod hosts;
pub mod keys;
pub mod listen;
pub mod manager;
pub mod net;
pub mod noise;
pub mod outbound;
pub mod pairing;
pub mod rpc;
pub mod server;
pub mod streams;
pub mod uploads;
pub mod wire;

pub use client::RemoteHostBackend;
pub use server::{HostServer, HostServerConfig};
pub use wire::{ClientHello, HostInfo, PROTOCOL_VERSION};

/// Milliseconds since the Unix epoch, for the timestamps in the stores.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
