//! This machine as a host, for the desktop app and `maple-agent serve`.
//!
//! Both roles share one data root: the host key and the lock that keeps two
//! servers off one root are this machine's; the paired devices and the
//! pending pairing code belong to the account that is hosting, so a device
//! paired into one account never reaches another account's runtime. The
//! desktop app starts hosting when "Allow remote connections" is on and
//! shows the state here in Settings; the command runs it in the foreground.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "desktop")]
use std::sync::Mutex;

use maple_agent::host::LocalHostBackend;
use maple_remote::devices::PairedDevice;
use maple_remote::keys::StaticKey;
use maple_remote::listen::{HostStores, serve_listener};
use maple_remote::pairing::{PairingCode, PairingLimiter, PendingPairing};
use maple_remote::server::{HostIdentity, HostServer, HostServerConfig};
use maple_remote::wire::HostInfo;
use tokio_util::sync::CancellationToken;

use super::{account_remote_dir, device_store, pending_pairing_store, remote_dir};
#[cfg(feature = "desktop")]
use crate::backend::AgentBackend;

/// What a running host records for `serve pair` to describe.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeState {
    pub listen: String,
    pub name: String,
    pub host_id: String,
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("serve.json")
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.join("serve.lock")
}

fn open_lock(dir: &Path) -> Result<std::fs::File, String> {
    let path = lock_path(dir);
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))
}

/// Take the hosting lock for `dir`. One server per data root: two would
/// race the runtime's own account state and the pairing file. The lock is
/// held for as long as the returned file is open.
fn take_lock(dir: &Path) -> Result<std::fs::File, String> {
    let lock = open_lock(dir)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(
            "another Maple host already runs on this machine (`maple-agent serve` or another window)"
                .to_string(),
        ),
        Err(std::fs::TryLockError::Error(error)) => Err(format!(
            "cannot lock {}: {error}",
            lock_path(dir).display()
        )),
    }
}

/// Whether a host holds the lock for `dir` right now. The probe takes the
/// lock and lets it go again, so it answers the same on every platform
/// and is never fooled by a crashed host's leftovers or a reused pid.
pub fn lock_is_held(dir: &Path) -> bool {
    let Ok(probe) = open_lock(dir) else {
        return false;
    };
    match probe.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(error)) => {
            log::debug!("cannot probe the hosting lock: {error}");
            false
        }
    }
}

/// The running host's state, or `None` when no host holds the lock (a
/// crash leaves the state file behind; the lock it does not).
pub fn read_state(dir: &Path) -> Option<ServeState> {
    if !lock_is_held(dir) {
        return None;
    }
    let bytes = std::fs::read(state_path(dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Publish a fresh pairing code for a host of `user_id` to accept.
pub fn publish_pairing_code(user_id: &str) -> Result<(PairingCode, PendingPairing), String> {
    let dir = account_remote_dir(user_id)?;
    let code = PairingCode::generate();
    let pending = pending_pairing_store(&dir).publish(&code)?;
    Ok((code, pending))
}

/// The code a host of `user_id` accepts right now, if one is pending and
/// not yet expired or consumed.
#[cfg(feature = "desktop")]
pub fn pending_pairing_code(user_id: &str) -> Result<Option<PendingPairing>, String> {
    Ok(pending_pairing_store(&account_remote_dir(user_id)?).current())
}

/// The devices paired into `user_id` on this machine.
pub fn list_devices(user_id: &str) -> Result<Vec<PairedDevice>, String> {
    device_store(&account_remote_dir(user_id)?).list()
}

/// Forget a device paired into `user_id`, by public key or by name.
pub fn revoke_device(user_id: &str, device: &str) -> Result<PairedDevice, String> {
    device_store(&account_remote_dir(user_id)?).revoke(device)
}

/// A running host: its listener task and the lock on the data root.
/// Dropping it stops nothing; call [`Hosting::stop`].
pub struct Hosting {
    pub listen: SocketAddr,
    pub host_id: String,
    pub name: String,
    shutdown: CancellationToken,
    listener: tokio::task::JoinHandle<Result<(), String>>,
    dir: PathBuf,
    lock: std::fs::File,
}

impl Hosting {
    /// Bind `listen` and serve the local host of `user_id` until
    /// [`Self::stop`]. Only devices paired into `user_id` are admitted.
    /// Fails when another host holds the data root or the address cannot
    /// be bound. Runs on the backend runtime, whose context spawns the
    /// listener task.
    pub async fn start(
        host: Arc<LocalHostBackend>,
        user_id: &str,
        listen: &str,
        name: String,
    ) -> Result<Self, String> {
        let dir = remote_dir()?;
        let lock = take_lock(&dir)?;
        let key = StaticKey::load_or_create(&dir.join("host_key.json"))?;
        let account_dir = account_remote_dir(user_id)?;
        let devices = Arc::new(device_store(&account_dir));
        let pending = Arc::new(pending_pairing_store(&account_dir));
        let hook_devices = Arc::clone(&devices);
        let config = HostServerConfig {
            on_client_hello: Some(Arc::new(move |hello| {
                if let Err(error) = hook_devices.touch(
                    &hello.device.public_key,
                    &hello.device.name,
                    hello.device.user_id.as_deref(),
                ) {
                    log::warn!("cannot record the device: {error}");
                }
            })),
            ..Default::default()
        };
        let identity = HostIdentity {
            app_version: crate::env::APP_VERSION.to_string(),
            build: crate::env::build_hash().map(str::to_string),
            pcr_environment: format!(
                "{:?}",
                maple_agent::open_secret_config::configured_pcr0_environment()?
            ),
        };
        let host_id = key.public_id();
        let info = HostInfo {
            id: host_id.clone(),
            name: name.clone(),
            user_id: Some(user_id.to_string()),
        };
        // The saved harness reaches the runtime when it starts
        // (`LocalHostBackend::start_runtime`); nothing to apply here.
        let server = HostServer::new(host, info, identity, config);
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|error| format!("cannot listen on {listen}: {error}"))?;
        let local = listener.local_addr().map_err(|error| error.to_string())?;
        maple_agent::private_file::write_private_json(
            &state_path(&dir),
            &ServeState {
                listen: local.to_string(),
                name: name.clone(),
                host_id: host_id.clone(),
            },
        )
        .map_err(|error| format!("cannot write the serve state: {error}"))?;
        let shutdown = CancellationToken::new();
        let stores = Arc::new(HostStores {
            key,
            devices,
            pending_pairing: pending,
            limiter: PairingLimiter::default(),
        });
        let listener = tokio::spawn(serve_listener(listener, server, stores, shutdown.clone()));
        Ok(Self {
            listen: local,
            host_id,
            name,
            shutdown,
            listener,
            dir,
            lock,
        })
    }

    /// Stop serving: end the listener and its connections, then release
    /// the lock once the port is free, so a host started next can bind.
    /// Resolves when both are gone; callers on the UI thread spawn it on
    /// the backend runtime.
    pub async fn stop(self) {
        self.shutdown.cancel();
        match self.listener.await {
            Ok(Ok(())) => log::info!("host {} stopped listening on {}", self.name, self.listen),
            Ok(Err(error)) => log::warn!("the host listener ended with an error: {error}"),
            Err(error) => log::warn!("the host listener task failed: {error}"),
        }
        let _ = std::fs::remove_file(state_path(&self.dir));
        // Last, after the listener let the port go.
        drop(self.lock);
    }
}

/// Where the desktop app's hosting stands.
#[cfg(feature = "desktop")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostingStatus {
    Off,
    /// A start is in flight on the backend runtime.
    Starting,
    Listening {
        listen: String,
        host_id: String,
        name: String,
    },
    Failed(String),
}

#[cfg(feature = "desktop")]
enum HostingState {
    Off,
    /// A start is in flight; the generation tells it whether it still
    /// owns the outcome when it finishes.
    Starting(u64),
    Listening(Hosting),
    Failed(String),
}

/// The desktop app's host role: starts and stops hosting for the signed-in
/// account and answers Settings. Starting and stopping run on the backend
/// runtime; the state answers at once.
#[cfg(feature = "desktop")]
pub struct HostingController {
    backend: Arc<AgentBackend>,
    host: Arc<LocalHostBackend>,
    user_id: String,
    state: Mutex<HostingState>,
    generation: std::sync::atomic::AtomicU64,
}

#[cfg(feature = "desktop")]
impl HostingController {
    pub fn new(backend: Arc<AgentBackend>, host: Arc<LocalHostBackend>, user_id: String) -> Self {
        Self {
            backend,
            host,
            user_id,
            state: Mutex::new(HostingState::Off),
            generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, HostingState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn status(&self) -> HostingStatus {
        match &*self.lock_state() {
            HostingState::Off => HostingStatus::Off,
            HostingState::Starting(_) => HostingStatus::Starting,
            HostingState::Listening(hosting) => HostingStatus::Listening {
                listen: hosting.listen.to_string(),
                host_id: hosting.host_id.clone(),
                name: hosting.name.clone(),
            },
            HostingState::Failed(error) => HostingStatus::Failed(error.clone()),
        }
    }

    /// Start hosting on `listen`. The state reads `Starting` at once; the
    /// returned future does the work and resolves to the outcome, so run
    /// it on the backend runtime. Already listening or starting resolves
    /// to the current status without a second start.
    pub fn start(
        self: &Arc<Self>,
        listen: &str,
    ) -> impl std::future::Future<Output = HostingStatus> + Send + 'static {
        let this = Arc::clone(self);
        let listen = listen.to_string();
        let generation = {
            let mut state = self.lock_state();
            match &*state {
                HostingState::Off | HostingState::Failed(_) => {
                    let generation = self
                        .generation
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    *state = HostingState::Starting(generation);
                    Some(generation)
                }
                HostingState::Starting(_) | HostingState::Listening(_) => None,
            }
        };
        async move {
            let Some(generation) = generation else {
                return this.status();
            };
            let result = Hosting::start(
                Arc::clone(&this.host),
                &this.user_id,
                &listen,
                crate::env::hostname(),
            )
            .await;
            let stale = {
                let mut state = this.lock_state();
                if matches!(*state, HostingState::Starting(current) if current == generation) {
                    *state = match result {
                        Ok(hosting) => {
                            log::info!(
                                "hosting as {} ({}) on {}",
                                hosting.name,
                                hosting.host_id,
                                hosting.listen
                            );
                            HostingState::Listening(hosting)
                        }
                        Err(error) => {
                            log::warn!("hosting did not start: {error}");
                            HostingState::Failed(error)
                        }
                    };
                    None
                } else {
                    // A stop arrived while the start ran: the stop wins.
                    result.ok()
                }
            };
            if let Some(hosting) = stale {
                hosting.stop().await;
            }
            this.status()
        }
    }

    /// Stop hosting. The state reads `Off` at once; the listener and the
    /// lock go on the backend runtime.
    pub fn stop(&self) {
        let previous = std::mem::replace(&mut *self.lock_state(), HostingState::Off);
        if let HostingState::Listening(hosting) = previous {
            self.backend.spawn(hosting.stop());
        }
    }
}

#[cfg(feature = "desktop")]
impl Drop for HostingController {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_probe_sees_a_held_lock_and_a_free_one() {
        let dir = std::env::temp_dir().join(format!("maple-hosting-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!lock_is_held(&dir), "no host runs on a fresh directory");
        assert!(read_state(&dir).is_none());
        let held = take_lock(&dir).unwrap();
        assert!(lock_is_held(&dir));
        assert!(take_lock(&dir).is_err(), "a second host is refused");
        maple_agent::private_file::write_private_json(
            &state_path(&dir),
            &ServeState {
                listen: "127.0.0.1:7130".to_string(),
                name: "box".to_string(),
                host_id: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(read_state(&dir).unwrap().name, "box");
        drop(held);
        assert!(!lock_is_held(&dir), "the probe leaves the lock free");
        assert!(
            read_state(&dir).is_none(),
            "a state file without its lock is a crashed host's leftover"
        );
        assert!(take_lock(&dir).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
