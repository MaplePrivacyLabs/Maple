//! The client's connections to its saved hosts.
//!
//! One connector task per saved host dials the host's connections in
//! order, hands the UI a connected [`RemoteHostBackend`], forwards the
//! host's events, and reconnects with jittered exponential backoff when
//! the connection ends. Pairing dials with a code, saves the host, and
//! starts its connector with the connection already open. Everything the
//! UI needs arrives as [`HostManagerEvent`]s on one channel.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use maple_agent::host::{HostBackend, HostEvent, HostId};
use rand::Rng as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::client::{ClientConfig, RemoteHostBackend};
use crate::dial::{ConnectTarget, connect_direct};
use crate::hosts::{HostConnection, HostsStore, SavedHost};
use crate::keys::StaticKey;
use crate::pairing::PairingCode;
use crate::wire::{ClientHello, version_label};

/// Reconnect backoff: full jitter between half and all of an exponential
/// delay from `BACKOFF_FLOOR` to `BACKOFF_CAP`.
const BACKOFF_FLOOR: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostStatus {
    Connecting,
    Online,
    Offline { reason: String },
}

/// What a host announced about its build at the hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostVersion {
    /// The host's package version.
    pub version: String,
    /// The git revision it was built from, when its build knew it.
    pub build: Option<String>,
}

impl HostVersion {
    /// `0.1.0 (63bcff5c)`, or `0.1.0` without a build.
    pub fn label(&self) -> String {
        version_label(&self.version, self.build.as_deref())
    }
}

/// What the manager tells the UI.
#[derive(Clone)]
pub enum HostManagerEvent {
    /// A host's connection state changed. `backend` is present exactly
    /// when the status is `Online`; the version it announced is answered
    /// by [`HostManager::host_version`] and saved on the host's record.
    Status {
        host: HostId,
        name: String,
        status: HostStatus,
        backend: Option<Arc<RemoteHostBackend>>,
    },
    /// The host pushed an event.
    Event { host: HostId, event: HostEvent },
    /// The saved host list changed (paired, renamed, removed).
    HostsChanged(Vec<SavedHost>),
}

impl std::fmt::Debug for HostManagerEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status { host, status, .. } => {
                write!(f, "Status({host}, {status:?})")
            }
            Self::Event { host, .. } => write!(f, "Event({host})"),
            Self::HostsChanged(hosts) => write!(f, "HostsChanged({})", hosts.len()),
        }
    }
}

pub struct HostManager {
    device: StaticKey,
    /// The hello every connection sends; the device is this client.
    hello: ClientHello,
    store: Arc<HostsStore>,
    config: ClientConfig,
    events: mpsc::UnboundedSender<HostManagerEvent>,
    connectors: Mutex<HashMap<String, CancellationToken>>,
    /// Hosts with a live connection right now, for callers that did not
    /// watch the event stream (the settings screen).
    online: Mutex<HashSet<String>>,
    /// What each host with a live connection announced at its hello.
    versions: Mutex<HashMap<String, HostVersion>>,
    shutdown: CancellationToken,
}

impl HostManager {
    pub fn new(
        device: StaticKey,
        hello: ClientHello,
        store: Arc<HostsStore>,
        config: ClientConfig,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<HostManagerEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                device,
                hello,
                store,
                config,
                events,
                connectors: Mutex::new(HashMap::new()),
                online: Mutex::new(HashSet::new()),
                versions: Mutex::new(HashMap::new()),
                shutdown: CancellationToken::new(),
            }),
            receiver,
        )
    }

    pub fn store(&self) -> &Arc<HostsStore> {
        &self.store
    }

    /// Whether `id` has a live connection right now.
    pub fn is_online(&self, id: &str) -> bool {
        self.online
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(id)
    }

    /// What `id` announced at the hello of its live connection, or `None`
    /// while it is offline; the saved host keeps the last seen version.
    pub fn host_version(&self, id: &str) -> Option<HostVersion> {
        self.versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    fn set_online(&self, id: &str, online: bool) {
        let mut set = self
            .online
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if online {
            set.insert(id.to_string());
        } else {
            set.remove(id);
        }
    }

    /// Remember what a connection's hello announced, or forget it once the
    /// connection is gone. The saved record keeps the last seen version
    /// so an offline host still shows what it ran.
    fn set_version(&self, id: &str, version: Option<HostVersion>) {
        let mut versions = self
            .versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match version {
            Some(version) => {
                versions.insert(id.to_string(), version);
            }
            None => {
                versions.remove(id);
            }
        }
    }

    /// Whether `token` still belongs to the connector registered for `id`.
    /// A connector that was replaced or removed keeps running until it
    /// notices its cancellation; nothing it says after that may reach the
    /// UI or the online set, or it would overwrite its successor's state.
    fn is_current(&self, id: &str, token: &CancellationToken) -> bool {
        self.connectors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .is_some_and(|current| current == token)
    }

    /// Start a connector for every saved host. Must run inside a Tokio
    /// runtime.
    pub fn start(self: &Arc<Self>) {
        let hosts = self.store.list().unwrap_or_else(|error| {
            log::warn!("cannot read saved hosts: {error}");
            Vec::new()
        });
        // The UI learns the full list first, so it can tell a saved host
        // that is still connecting from one it does not know at all.
        self.emit(HostManagerEvent::HostsChanged(hosts.clone()));
        for host in hosts {
            self.spawn_connector(host, None);
        }
    }

    /// Stop every connector.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    fn emit(&self, event: HostManagerEvent) {
        let _ = self.events.send(event);
    }

    /// Pair with the host at `address` using `code`, save it, and start its
    /// connector on the connection the pairing opened.
    pub async fn pair(
        self: &Arc<Self>,
        address: &str,
        code: PairingCode,
        name: Option<String>,
    ) -> Result<SavedHost, String> {
        let address = address.trim().to_string();
        if address.is_empty() {
            return Err("Enter the host's address".to_string());
        }
        let dialed = connect_direct(&address, &self.device, ConnectTarget::Pair(code)).await?;
        let backend =
            RemoteHostBackend::connect(dialed.carrier, self.hello.clone(), self.config.clone())
                .await?;
        let announced = backend.host_hello().host.clone();
        let (last_seen_version, last_seen_build) = backend.host_version();
        let host = self.store.upsert(SavedHost {
            id: dialed.host_key.clone(),
            name: name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(announced.name),
            connections: vec![HostConnection::Direct { address }],
            paired_at_ms: 0,
            last_seen_version: Some(last_seen_version),
            last_seen_build,
        })?;
        self.emit(HostManagerEvent::HostsChanged(self.store.list()?));
        self.spawn_connector(host.clone(), Some(backend));
        Ok(host)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        self.store.rename(id, name)?;
        self.emit(HostManagerEvent::HostsChanged(self.store.list()?));
        Ok(())
    }

    /// Forget a host: its connector stops and its record goes.
    pub fn remove(&self, id: &str) -> Result<(), String> {
        if let Some(token) = self
            .connectors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            token.cancel();
        }
        self.set_online(id, false);
        self.set_version(id, None);
        let name = self
            .store
            .get(id)?
            .map(|host| host.name)
            .unwrap_or_default();
        self.store.remove(id)?;
        self.emit(HostManagerEvent::Status {
            host: HostId::new(id),
            name,
            status: HostStatus::Offline {
                reason: "removed".to_string(),
            },
            backend: None,
        });
        self.emit(HostManagerEvent::HostsChanged(self.store.list()?));
        Ok(())
    }

    fn spawn_connector(self: &Arc<Self>, host: SavedHost, initial: Option<Arc<RemoteHostBackend>>) {
        let token = self.shutdown.child_token();
        if let Some(previous) = self
            .connectors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host.id.clone(), token.clone())
        {
            previous.cancel();
        }
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager.run_connector(host, initial, token).await;
        });
    }

    async fn run_connector(
        &self,
        host: SavedHost,
        mut initial: Option<Arc<RemoteHostBackend>>,
        cancel: CancellationToken,
    ) {
        let id = HostId::new(host.id.clone());
        // Status and the online set belong to the current connector only.
        let status = |name: &str, status: HostStatus, backend: Option<Arc<RemoteHostBackend>>| {
            if self.is_current(&host.id, &cancel) {
                self.emit(HostManagerEvent::Status {
                    host: id.clone(),
                    name: name.to_string(),
                    status,
                    backend,
                });
            }
        };
        let online = |online: Option<HostVersion>| {
            if self.is_current(&host.id, &cancel) {
                self.set_online(&host.id, online.is_some());
                self.set_version(&host.id, online);
            }
        };
        let mut attempt: u32 = 0;
        while !cancel.is_cancelled() {
            // Connections may have been added since; read them fresh.
            let saved = self
                .store
                .get(&host.id)
                .ok()
                .flatten()
                .unwrap_or_else(|| host.clone());
            let name = saved.name.clone();
            let backend = match initial.take() {
                Some(backend) => Ok(backend),
                None => {
                    status(&name, HostStatus::Connecting, None);
                    self.dial(&saved, &cancel).await
                }
            };
            match backend {
                Ok(backend) => {
                    attempt = 0;
                    let (version, build) = backend.host_version();
                    // Every successful hello refreshes the record, so an
                    // offline host shows the build it ran most recently.
                    if let Err(error) =
                        self.store
                            .record_last_seen(&host.id, &version, build.as_deref())
                    {
                        log::warn!("cannot record the host's version: {error}");
                    }
                    online(Some(HostVersion { version, build }));
                    status(&name, HostStatus::Online, Some(Arc::clone(&backend)));
                    let reason = self.forward_events(&id, &backend, &cancel).await;
                    online(None);
                    backend.close().await;
                    status(&name, HostStatus::Offline { reason }, None);
                    if cancel.is_cancelled() {
                        return;
                    }
                }
                Err(reason) => status(&name, HostStatus::Offline { reason }, None),
            }
            let delay = backoff(attempt);
            attempt = attempt.saturating_add(1);
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = cancel.cancelled() => return,
            }
        }
    }

    /// Try each connection in order; the first that completes a handshake
    /// wins. Cancelling ends the attempt at once rather than after the
    /// dial's own timeout.
    async fn dial(
        &self,
        host: &SavedHost,
        cancel: &CancellationToken,
    ) -> Result<Arc<RemoteHostBackend>, String> {
        if host.connections.is_empty() {
            return Err("no address saved for this host".to_string());
        }
        let mut last_error = String::new();
        for connection in &host.connections {
            let HostConnection::Direct { address } = connection;
            let attempt = async {
                let dialed = connect_direct(
                    address,
                    &self.device,
                    ConnectTarget::Host {
                        host_key: host.id.clone(),
                    },
                )
                .await?;
                RemoteHostBackend::connect(dialed.carrier, self.hello.clone(), self.config.clone())
                    .await
            };
            let outcome = tokio::select! {
                outcome = attempt => outcome,
                _ = cancel.cancelled() => return Err("cancelled".to_string()),
            };
            match outcome {
                Ok(backend) => return Ok(backend),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    /// Forward the host's events until the connection ends or the
    /// connector is cancelled. Returns why it stopped.
    async fn forward_events(
        &self,
        id: &HostId,
        backend: &Arc<RemoteHostBackend>,
        cancel: &CancellationToken,
    ) -> String {
        let mut events = backend.subscribe();
        let mut closed = backend.closed();
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.emit(HostManagerEvent::Event { host: id.clone(), event }),
                    None => return "connection ended".to_string(),
                },
                changed = closed.changed() => {
                    if changed.is_err() {
                        return "connection ended".to_string();
                    }
                    if let Some(reason) = closed.borrow().clone() {
                        return reason;
                    }
                }
                _ = cancel.cancelled() => return "stopped".to_string(),
            }
        }
    }
}

/// Full-jitter exponential backoff.
fn backoff(attempt: u32) -> Duration {
    let exponential = BACKOFF_FLOOR
        .checked_mul(2u32.saturating_pow(attempt.min(10)))
        .unwrap_or(BACKOFF_CAP)
        .min(BACKOFF_CAP);
    let millis = exponential.as_millis() as u64;
    let jittered = rand::thread_rng().gen_range((millis / 2).max(1)..=millis);
    Duration::from_millis(jittered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(
        dir: &std::path::Path,
    ) -> (Arc<HostManager>, mpsc::UnboundedReceiver<HostManagerEvent>) {
        let device = StaticKey::generate().unwrap();
        let hello = ClientHello {
            protocol: crate::wire::PROTOCOL_VERSION,
            app_version: "0.1.0".to_string(),
            build: None,
            pcr_environment: "Development".to_string(),
            features: crate::wire::features(),
            device: crate::wire::DeviceInfo {
                public_key: device.public_id(),
                name: "test".to_string(),
                user_id: None,
            },
        };
        let store = Arc::new(HostsStore::new(dir.join("hosts.json")));
        HostManager::new(device, hello, store, ClientConfig::default())
    }

    #[tokio::test]
    async fn removing_a_host_drops_its_dial_and_silences_its_connector() {
        let dir = std::env::temp_dir().join(format!("maple-manager-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // A listener that accepts and then never answers the WebSocket
        // handshake, so a dial hangs until it is cancelled.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 4096];
            // The WebSocket request arrives; then nothing more until EOF.
            let mut total = 0;
            loop {
                match tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => total += read,
                }
            }
            let _ = accepted_tx.send(total);
        });

        let (manager, mut events) = manager(&dir);
        let host = manager
            .store()
            .upsert(SavedHost {
                id: StaticKey::generate().unwrap().public_id(),
                name: "slow".to_string(),
                connections: vec![HostConnection::Direct { address }],
                paired_at_ms: 0,
                last_seen_version: None,
                last_seen_build: None,
            })
            .unwrap();
        manager.start();
        // The connector announces it is connecting, then hangs in the dial.
        let started = std::time::Instant::now();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let HostManagerEvent::Status {
                status: HostStatus::Connecting,
                ..
            } = event
            {
                break;
            }
        }
        manager.remove(&host.id).unwrap();
        // The peer sees EOF as soon as the dial is dropped, long before the
        // handshake timeout. How much of the WebSocket request was written
        // first depends on scheduling and does not matter.
        let _read = tokio::time::timeout(Duration::from_secs(5), accepted_rx)
            .await
            .expect("the dial was dropped at once")
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        // After the removal notice the old connector says nothing more.
        let mut statuses = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(300), events.recv()).await
        {
            if let HostManagerEvent::Status { status, .. } = event {
                statuses.push(status);
            }
        }
        assert_eq!(
            statuses,
            vec![HostStatus::Offline {
                reason: "removed".to_string()
            }]
        );
        assert!(!manager.is_online(&host.id));
        manager.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn backoff_grows_to_the_cap_and_stays_jittered() {
        for attempt in 0..12 {
            let delay = backoff(attempt);
            assert!(delay >= BACKOFF_FLOOR / 2, "attempt {attempt}: {delay:?}");
            assert!(delay <= BACKOFF_CAP, "attempt {attempt}: {delay:?}");
        }
        assert!(backoff(0) <= BACKOFF_FLOOR);
        assert!(backoff(10) >= BACKOFF_CAP / 2);
    }
}
