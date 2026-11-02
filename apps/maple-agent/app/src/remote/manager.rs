//! Per-account networking owned by the local Maple daemon. Enrollment is local
//! control only; cloud metadata never substitutes for a persisted peer grant.
use super::{
    network, pairing,
    store::{Peer, Store},
    wire,
};
use crate::{
    backend::LocalBackend,
    host::{protocol, receipts::Receipts},
};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};
use maple_agent::{agent::AgentServiceEvent, maple_api::MapleApiSession};
use maple_sdk::remote::*;
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc, Mutex, RwLock, RwLockWriteGuard, TryLockError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const HOST_GRANT_RECOVERY_LIMIT: usize = 8;
const HOST_GRANT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const HOST_GRANT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const PAIRING_CONFIRMATION_REQUIRED: &str = "Pair this computer again before updating it remotely. Its saved pairing confirmation is unavailable.";

fn encode_install_reply(reply: super::update::InstallReply) -> Result<serde_json::Value, String> {
    serde_json::to_value(reply).map_err(|_| "Could not display remote update receipt".into())
}

fn remote_refresh_failure(error: &str) -> String {
    use crate::remote_updates::RefreshFailure;
    [
        RefreshFailure::Busy,
        RefreshFailure::Disabled,
        RefreshFailure::Unavailable,
        RefreshFailure::CheckFailed,
        RefreshFailure::TimedOut,
    ]
    .into_iter()
    .map(RefreshFailure::message)
    .find(|message| *message == error)
    .unwrap_or(RefreshFailure::CheckFailed.message())
    .to_owned()
}

fn confirmed_host_grant_reply(
    window: [u8; 16],
    confirmed: Result<[u8; 16], wire::Refusal>,
) -> Result<bool, String> {
    match confirmed {
        Ok(grant) if grant == window => Ok(true),
        Err(refusal)
            if refusal.terminal && refusal.message == "No current grant matches this pairing" =>
        {
            Ok(false)
        }
        Err(refusal) => Err(refusal.into_error()),
        Ok(_) => Err("Invalid grant acknowledgement".into()),
    }
}

/// Attempt history is only a bounded candidate list, never authority. A
/// successful authenticated ConfirmGrant response supplies the missing proof.
async fn recover_confirmed_host_grant<P, F>(
    attempts: &[[u8; 16]],
    probe_timeout: Duration,
    total_timeout: Duration,
    mut probe: P,
) -> Result<[u8; 16], String>
where
    P: FnMut([u8; 16]) -> F,
    F: std::future::Future<Output = Result<bool, String>>,
{
    tokio::time::timeout(total_timeout, async {
        for window in attempts.iter().rev().take(HOST_GRANT_RECOVERY_LIMIT) {
            match tokio::time::timeout(probe_timeout, probe(*window)).await {
                Ok(Ok(true)) => return Ok(*window),
                Ok(Ok(false)) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(PAIRING_CONFIRMATION_REQUIRED.into()),
            }
        }
        Err(PAIRING_CONFIRMATION_REQUIRED.into())
    })
    .await
    .map_err(|_| PAIRING_CONFIRMATION_REQUIRED.to_owned())?
}

async fn retain_observed_peer_info<F>(
    mut info: Option<super::update::PeerInfo>,
    known: bool,
    deadline: tokio::time::Instant,
    recovery: F,
) -> Option<super::update::PeerInfo>
where
    F: std::future::Future<Output = Result<[u8; 16], String>>,
{
    if let Some(info) = &mut info {
        let confirmed =
            known || matches!(tokio::time::timeout_at(deadline, recovery).await, Ok(Ok(_)));
        if !confirmed && info.update.blocked.is_none() {
            info.update.blocked = Some(super::update::BlockedReason::PairingConfirmationRequired);
        }
    }
    info
}

async fn query_peer_information<T, F>(
    connect: impl std::future::Future<Output = Result<T, String>>,
    probe: impl FnOnce(T) -> F,
    limit: Duration,
) -> Result<Option<super::update::PeerInfo>, String>
where
    F: std::future::Future<Output = Result<Option<super::update::PeerInfo>, String>>,
{
    let deadline = tokio::time::Instant::now() + limit;
    let connection = tokio::time::timeout_at(deadline, connect)
        .await
        .map_err(|_| "The remote computer did not answer its connection check. Check that it is online, then refresh.".to_owned())??;
    // Once connected, absence of the optional capability remains a safe
    // fallback. Do not misclassify failure to connect as an old peer.
    tokio::time::timeout_at(deadline, probe(connection))
        .await
        .unwrap_or(Ok(None))
}

pub struct Manager {
    node: AsyncMutex<Option<Arc<Node>>>,
    initializing: AsyncMutex<()>,
    backend: Arc<LocalBackend>,
    events: broadcast::Sender<Arc<AgentServiceEvent>>,
    receipts: Arc<Receipts>,
    resident: AtomicBool,
    generation: AtomicU64,
    resuming: AtomicBool,
    last_error: Mutex<Option<String>>,
}

pub struct Node {
    pub(super) backend: Arc<LocalBackend>,
    pub(super) events: broadcast::Sender<Arc<AgentServiceEvent>>,
    pub(super) receipts: Arc<Receipts>,
    // Shared permission/publication guards keep controls responsive during disk
    // publication; exclusive settings edits still serialize revoke with commit.
    pub(super) store: RwLock<Store>,
    pub(super) session: Arc<MapleApiSession>,
    pub(super) endpoint: Endpoint,
    addresses: Mutex<std::collections::HashMap<EndpointId, (EndpointAddr, RemoteDevice)>>,
    pub(super) account: String,
    pub(super) project: String,
    pub(super) deployment: String,
    window: AsyncMutex<Option<pairing::Window>>,
    pairing_generation: AtomicU64,
    registration: AsyncMutex<()>,
    pub(super) stopped: CancellationToken,
    pub(super) revoked: broadcast::Sender<EndpointId>,
    // Remote connections never occupy the local IPC/control capacity.
    pub(super) admissions: Arc<Semaphore>,
    pub(super) controllers: Arc<Semaphore>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub ready: bool,
    pub error: Option<String>,
    pub peers: std::collections::BTreeMap<String, Peer>,
}

#[derive(Serialize, Deserialize)]
pub struct PairingWindow {
    pub code: String,
    pub window: Uuid,
    pub expires_in: u64,
}

impl Manager {
    pub fn new(
        backend: Arc<LocalBackend>,
        events: broadcast::Sender<Arc<AgentServiceEvent>>,
        receipts: Arc<Receipts>,
    ) -> Arc<Self> {
        Arc::new(Self {
            node: AsyncMutex::new(None),
            initializing: AsyncMutex::new(()),
            backend,
            events,
            receipts,
            resident: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            resuming: AtomicBool::new(false),
            last_error: Mutex::new(None),
        })
    }
    pub(crate) async fn host_activity(&self) -> (usize, usize) {
        let node = self.node.lock().await;
        match node.as_ref() {
            Some(node) => (
                8usize.saturating_sub(node.controllers.available_permits()),
                4usize.saturating_sub(node.admissions.available_permits()),
            ),
            None => (0, 0),
        }
    }

    pub fn stays_running(&self) -> bool {
        self.resident.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn lifecycle_revision_for_test(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub async fn shutdown(&self) {
        let node = {
            let mut current = self.node.lock().await;
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.resident.store(false, Ordering::Release);
            current.take()
        };
        if let Some(node) = node {
            node.stopped.cancel();
            node.endpoint.close().await;
        }
    }

    fn database(&self) -> Result<Arc<maple_agent::store::Store>, String> {
        self.backend
            .store()
            .cloned()
            .ok_or_else(|| "Local storage is unavailable".to_owned())
    }

    fn store(&self, account: &str) -> Result<Store, String> {
        Store::open(
            self.database()?,
            self.backend.remote_api_url(),
            &crate::backend::local::configured_client_id().to_string(),
            crate::profile::PROFILE,
            account,
        )
    }

    fn store_exists(&self, account: &str) -> Result<bool, String> {
        let database = self.database()?;
        Store::exists(
            &database,
            self.backend.remote_api_url(),
            &crate::backend::local::configured_client_id().to_string(),
            crate::profile::PROFILE,
            account,
        )
    }

    pub async fn restore(self: &Arc<Self>) {
        let current = self.node.lock().await;
        let enabled = self.backend.saved_user_id().is_some_and(|account| {
            self.store_exists(&account).is_ok_and(|exists| exists)
                && self.store(&account).is_ok_and(|s| s.enabled())
        });
        if !enabled {
            return;
        }
        self.resident.store(true, Ordering::Release);
        drop(current);
        // Invalid configuration is actionable, not a network outage to retry.
        if let Err(error) = configuration(self.backend.remote_api_url()) {
            *self.last_error.lock().unwrap() = Some(error);
            return;
        }
        if self.resuming.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut delay = Duration::from_secs(1);
            loop {
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                if !manager.stays_running() {
                    manager.resuming.store(false, Ordering::Release);
                    return;
                }
                match manager.active().await {
                    Ok(_) => {
                        *manager.last_error.lock().unwrap() = None;
                        manager.resuming.store(false, Ordering::Release);
                        return;
                    }
                    Err(error) if manager.stays_running() => {
                        *manager.last_error.lock().unwrap() = Some(error)
                    }
                    Err(_) => {}
                }
                drop(manager);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        });
    }

    pub async fn active(self: &Arc<Self>) -> Result<Arc<Node>, String> {
        let account = self
            .backend
            .saved_user_id()
            .ok_or("Sign in before using remote access")?;
        let _initializing = self.initializing.lock().await;
        let (generation, current) = {
            let current = self.node.lock().await;
            (self.generation.load(Ordering::SeqCst), current.clone())
        };
        if let Some(node) = current {
            if node.account == account && node.valid().await {
                return Ok(node);
            }
            node.stopped.cancel();
            node.endpoint.close().await;
            let mut current = self.node.lock().await;
            if current.as_ref().is_some_and(|n| Arc::ptr_eq(n, &node)) {
                current.take();
            }
        }
        let session = tokio::time::timeout(
            Duration::from_secs(15),
            self.backend.remote_session(&account),
        )
        .await
        .map_err(|_| "Could not validate remote account; try again when connected")??;
        let store = self.store(&account)?;
        let key = store.key();
        let project = crate::backend::local::configured_client_id().to_string();
        let deployment = if self.backend.remote_api_url().starts_with("http://") {
            "local"
        } else if crate::profile::PCR_ENVIRONMENT == "development" {
            "dev"
        } else {
            "prod"
        }
        .to_string();
        let (relays, relay_only, loopback_only) = configuration(self.backend.remote_api_url())?;
        // Register possession before asking an authenticated Maple relay to
        // accept this key. Addresses are refreshed after binding.
        register(
            &session,
            &key,
            (&account, &project, &deployment),
            RemoteAddresses::default(),
            store.name().to_owned(),
            store.enabled(),
            None,
        )
        .await?;
        if self.generation.load(Ordering::SeqCst) != generation
            || !session.remote_session_active().await
            || self.backend.saved_user_id().as_deref() != Some(&account)
        {
            return Err("The signed-in account changed".into());
        }
        let endpoint = network::bind(key, relays, relay_only, loopback_only).await?;
        if self.generation.load(Ordering::SeqCst) != generation
            || !session.remote_session_active().await
            || self.backend.saved_user_id().as_deref() != Some(&account)
        {
            endpoint.close().await;
            return Err("The signed-in account changed".into());
        }
        // Settings mutations advance generation under node's installation lock.
        // Any older initialization is discarded, even after this disk read.
        let store = self.store(&account)?;
        let (revoked, _) = broadcast::channel(64);
        let node = Arc::new(Node {
            backend: self.backend.clone(),
            events: self.events.clone(),
            receipts: self.receipts.clone(),
            store: RwLock::new(store),
            session,
            endpoint,
            addresses: Mutex::new(std::collections::HashMap::new()),
            account,
            project,
            deployment,
            window: AsyncMutex::new(None),
            pairing_generation: AtomicU64::new(0),
            registration: AsyncMutex::new(()),
            stopped: CancellationToken::new(),
            revoked,
            admissions: Arc::new(Semaphore::new(4)),
            controllers: Arc::new(Semaphore::new(8)),
        });
        {
            let mut current = self.node.lock().await;
            if self.generation.load(Ordering::SeqCst) != generation {
                drop(current);
                node.endpoint.close().await;
                return Err("Remote access was stopped while connecting".into());
            }
            self.resident
                .store(node.store.read().unwrap().enabled(), Ordering::Release);
            *current = Some(node.clone());
        }
        super::control::listen(node.clone());
        let weak = Arc::downgrade(&node);
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(node) = weak.upgrade() else {
                    break;
                };
                if !node.valid().await {
                    node.stopped.cancel();
                    node.endpoint.close().await;
                    if let Some(manager) = manager.upgrade() {
                        let current = manager.node.lock().await;
                        if current.as_ref().is_some_and(|n| Arc::ptr_eq(n, &node)) {
                            manager.resident.store(false, Ordering::Release);
                        }
                    }
                    break;
                }
                // Discovery is only a reachability hint. Existing paired
                // control survives an enrollment-service outage.
                if let Err(error) = node.publish().await {
                    log::debug!("Remote discovery refresh unavailable: {error}");
                }
                let stopped = node.stopped.clone();
                drop(node);
                tokio::select! { _ = stopped.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(60)) => {} }
            }
        });
        Ok(node)
    }

    pub async fn dispatch(
        self: &Arc<Self>,
        request: protocol::Request,
    ) -> Result<serde_json::Value, String> {
        let encode = |value| {
            serde_json::to_value(value).map_err(|_| "Could not encode remote settings".to_string())
        };
        if matches!(
            request.method.as_str(),
            "remote_peer_info" | "remote_refresh_update"
        ) {
            let refresh = request.method == "remote_refresh_update";
            let (account, peer): (String, String) = serde_json::from_value(request.params)
                .map_err(|_| "Invalid remote update request")?;
            let _account_guard = self.backend.account_gate.read().await;
            self.backend.authorize_account(&account)?;
            let limit = Duration::from_secs(if refresh { 45 } else { 20 });
            let deadline = tokio::time::Instant::now() + limit;
            let mut recovery = None;
            let mut info = query_peer_information(
                self.connect(&peer),
                |link| {
                    recovery = Some((link.node.clone(), link.peer));
                    let account = &account;
                    async move {
                        if link.node.account != *account {
                            return Err("The signed-in account changed".into());
                        }
                        let observed = super::streams::peer_info(&link.connection).await;
                        let info = if refresh {
                            let observed = observed.ok_or("Could not read this computer’s update capabilities")?;
                            if !observed.supports_remote_refresh() {
                                return Err("This Maple version cannot check for updates remotely. Check for Updates directly on that computer.".into());
                            }
                            let response = super::streams::call(&link.connection, wire::Command {
                                id: 3,
                                method: wire::Method::RemoteUpdateRefresh,
                                send_id: None,
                                params: serde_json::json!({}),
                            }).await.map_err(|_| "The remote update check could not be completed")?;
                            let protocol::Message::Response { id: 3, result } = response else {
                                return Err("The remote update check returned an invalid response".into());
                            };
                            let value = result.map_err(|error| remote_refresh_failure(&error))?;
                            let checked: super::update::PeerInfo = serde_json::from_value(value)
                                .map_err(|_| "The remote update check returned an invalid response")?;
                            checked.validate()?;
                            Some(checked)
                        } else {
                            observed
                        };
                        Ok(info)
                    }
                },
                limit,
            )
            .await?;
            // Retain the independently read status before attempting metadata
            // recovery. Slow confirmation must never erase a known version or
            // turn a completed signed-feed check into an unavailable response.
            if let Some((node, peer)) = recovery.filter(|_| info.is_some()) {
                let known = node.store.read().ok().is_some_and(|store| {
                    store
                        .peers()
                        .get(&peer.to_string())
                        .is_some_and(|saved| saved.can_connect && saved.host_grant.is_some())
                });
                info =
                    retain_observed_peer_info(info, known, deadline, node.ensure_host_grant(peer))
                        .await;
            }
            self.backend.authorize_account(&account)?;
            if refresh && info.is_none() {
                return Err("The remote update check did not finish. Its previous status is not a fresh update check.".into());
            }
            return serde_json::to_value(info)
                .map_err(|_| "Could not display remote update status".into());
        }
        if request.method == "remote_install_update" {
            use super::update::{InstallRefusal as Refusal, InstallReply};
            let refused = |reason| Ok::<_, String>(InstallReply::Refused { reason });
            let (account, peer, expected_grant, install): (
                String,
                String,
                [u8; 16],
                super::update::InstallRequest,
            ) = serde_json::from_value(request.params)
                .map_err(|_| "Invalid remote update request")?;
            if install.validate().is_err() {
                return encode_install_reply(InstallReply::Refused {
                    reason: Refusal::Unsupported,
                });
            }
            let _account_guard = self.backend.account_gate.read().await;
            if self.backend.authorize_account(&account).is_err() {
                return encode_install_reply(InstallReply::Refused {
                    reason: Refusal::AccountChanged,
                });
            }
            let submitted = AtomicBool::new(false);
            let outcome = tokio::time::timeout(Duration::from_secs(15), async {
                let link = match self.connect(&peer).await {
                    Ok(link) => link,
                    Err(error) if wire::is_protocol_incompatibility_refusal(&error) => return refused(Refusal::Unsupported),
                    Err(_) => return refused(Refusal::Unavailable),
                };
                if self.backend.authorize_account(&account).is_err() || link.node.account != account {
                    return refused(Refusal::AccountChanged);
                }
                let paired = match link.node.store.read() {
                    Ok(store) => store.peers().get(&link.peer.to_string()).is_some_and(|value| value.can_connect && value.host_grant == Some(expected_grant)),
                    Err(_) => return refused(Refusal::Unavailable),
                };
                if !paired { return refused(Refusal::AuthorizationChanged); }
                let Some(info) = super::streams::peer_info(&link.connection).await else {
                    return refused(Refusal::Unavailable);
                };
                if !info.supports_remote_updates() { return refused(Refusal::Unsupported); }
                if info.update.offered.as_ref() != Some(&install.target) {
                    return refused(Refusal::UpdateChanged);
                }
                if !info.install_requestable() {
                    return refused(if info.update.blocked == Some(super::update::BlockedReason::Busy) { Refusal::Busy } else { Refusal::NotReady });
                }
                if self.backend.authorize_account(&account).is_err() {
                    return refused(Refusal::AccountChanged);
                }
                if link.node.ensure_valid().await.is_err() { return refused(Refusal::AuthorizationChanged); }
                let paired = match link.node.store.read() {
                    Ok(store) => store.peers().get(&link.peer.to_string()).is_some_and(|value| value.can_connect && value.host_grant == Some(expected_grant)),
                    Err(_) => return refused(Refusal::Unavailable),
                };
                if !paired {
                    return refused(Refusal::AuthorizationChanged);
                }
                // From this point a lost/invalid reply can follow admission.
                // Never turn that ambiguity into a definite refusal or retry.
                let params = match serde_json::to_value(super::update::InstallCommand {
                    expected_grant, request: install.clone(),
                }) {
                    Ok(params) => params,
                    Err(_) => return refused(Refusal::Unsupported),
                };
                submitted.store(true, Ordering::Relaxed);
                let message = super::streams::call(&link.connection, wire::Command {
                    id: 2,
                    method: wire::Method::RemoteUpdateInstall,
                    send_id: None,
                    params,
                }).await.map_err(|_| "Remote update outcome is uncertain. Refresh its status; do not submit another update.")?;
                let protocol::Message::Response { result, .. } = message else {
                    return Err("Remote update outcome is uncertain. Refresh its status; do not submit another update.".into());
                };
                let value = match result {
                    Ok(value) => value,
                    // The execution host replies Err only when admission failed.
                    // Do not copy its arbitrary private error into the client UI.
                    // An older host refuses an offer it has not downloaded.
                    Err(_) if info.needs_local_download() => return refused(Refusal::DownloadRequired),
                    Err(_) => return refused(Refusal::Unavailable),
                };
                let accepted: super::update::InstallAccepted = serde_json::from_value(value)
                    .map_err(|_| "Remote update outcome is uncertain. Refresh its status; do not submit another update.")?;
                if accepted.operation_id != install.operation_id || accepted.target != install.target {
                    return Err("Remote update outcome is uncertain. Refresh its status; do not submit another update.".into());
                }
                Ok::<_, String>(InstallReply::Accepted { receipt: accepted })
            }).await;
            let response = match outcome {
                Ok(response) => response?,
                Err(_) if !submitted.load(Ordering::Relaxed) => InstallReply::Refused { reason: Refusal::Unavailable },
                Err(_) => return Err("Remote update outcome is uncertain. Refresh its status; do not submit another update.".into()),
            };
            if self.backend.authorize_account(&account).is_err() {
                return match response {
                    InstallReply::Refused { .. } => encode_install_reply(InstallReply::Refused { reason: Refusal::AccountChanged }),
                    InstallReply::Accepted { .. } => Err("Remote update outcome is uncertain. Refresh its status; do not submit another update.".into()),
                };
            }
            return encode_install_reply(response);
        }
        if request.method == "remote_pairing_window" {
            let current = self.node.lock().await.clone();
            let window = match current {
                Some(node) if node.valid().await => node.current_pairing().await,
                _ => None,
            };
            return serde_json::to_value(window)
                .map_err(|_| "Could not display pairing window".into());
        }
        if request.method == "remote_status" {
            let account = self
                .backend
                .saved_user_id()
                .ok_or("Sign in before using remote access")?;
            if !self.store_exists(&account)? {
                return encode(Status {
                    name: super::store::default_name().into(),
                    enabled: false,
                    endpoint: None,
                    ready: false,
                    error: None,
                    peers: Default::default(),
                });
            }
            let current = self.node.lock().await;
            let store = self.store(&account)?;
            return encode(Status {
                name: store.name().into(),
                enabled: store.enabled(),
                endpoint: Some(store.key().public().to_string()),
                ready: current
                    .as_ref()
                    .is_some_and(|n| n.account == account && !n.stopped.is_cancelled()),
                error: self.last_error.lock().unwrap().clone(),
                peers: store.peers().clone(),
            });
        }
        if request.method == "remote_set_name" {
            let (name,): (String,) =
                serde_json::from_value(request.params).map_err(|_| "Invalid computer name")?;
            super::validate_name(&name)?;
            let account = self
                .backend
                .saved_user_id()
                .ok_or("Sign in before naming this computer")?;
            // Like enable/revoke, serialize local writes with node installation.
            // Naming an offline/disabled computer must not start networking.
            let current = self.node.lock().await;
            self.generation.fetch_add(1, Ordering::SeqCst);
            let node = current.as_ref().filter(|n| n.account == account).cloned();
            if let Some(node) = &node {
                let mut store = write_policy(&node.store).await?;
                self.backend.authorize_account(&account)?;
                store.set_name(&name)?;
            } else {
                self.store(&account)?.set_name(&name)?;
            }
            drop(current);
            if let Some(node) = node {
                tokio::spawn(async move {
                    let _ = node.publish().await;
                });
            }
            return Ok(serde_json::Value::Null);
        }
        if matches!(
            request.method.as_str(),
            "remote_revoke" | "remote_cancel_pairing" | "remote_enable"
        ) {
            let account = self
                .backend
                .saved_user_id()
                .ok_or("No local account is selected")?;
            let enable = if request.method == "remote_enable" {
                let (enabled,): (bool,) = serde_json::from_value(request.params.clone())
                    .map_err(|_| "Invalid remote setting")?;
                Some(enabled)
            } else {
                None
            };
            // This local lock orders settings writes against node install.
            // No credential validation or network operation holds it.
            let current = self.node.lock().await;
            self.generation.fetch_add(1, Ordering::SeqCst);
            let node = current.as_ref().filter(|n| n.account == account).cloned();
            if (request.method != "remote_enable" || enable == Some(false))
                && let Some(node) = &node
            {
                let mut window = node.window.lock().await;
                node.pairing_generation.fetch_add(1, Ordering::SeqCst);
                *window = None;
            }
            if request.method != "remote_cancel_pairing" {
                let mut disk;
                let mut memory;
                let store = if let Some(node) = &node {
                    memory = write_policy(&node.store).await?;
                    &mut *memory
                } else {
                    disk = self.store(&account)?;
                    &mut disk
                };
                self.backend.authorize_account(&account)?;
                if let Some(enabled) = enable {
                    store.set_enabled(enabled)?;
                    self.resident.store(enabled, Ordering::Release);
                    *self.last_error.lock().unwrap() = None;
                    if !enabled && let Some(node) = &node {
                        let _ = node.revoked.send(node.endpoint.id());
                    }
                } else {
                    let (peer,): (String,) =
                        serde_json::from_value(request.params).map_err(|_| "Invalid device")?;
                    let peer = peer.parse().map_err(|_| "Invalid device")?;
                    store.revoke(peer)?;
                    if let Some(node) = &node {
                        let _ = node.revoked.send(peer);
                    }
                }
            }
            drop(current);
            if enable == Some(true) {
                self.restore().await;
            }
            // Publish best effort without making local disable/cancel wait on a service.
            if let Some(node) = node {
                tokio::spawn(async move {
                    let _ = node.publish().await;
                });
            }
            return Ok(serde_json::Value::Null);
        }
        let node = self.active().await?;
        match request.method.as_str() {
            "remote_begin_pairing" => serde_json::to_value(node.begin_pairing().await?)
                .map_err(|_| "Could not display pairing window".into()),
            "remote_discover" => {
                node.publish().await?;
                let list = node.session.list_remote_devices().await?;
                validate_devices(&list.devices)?;
                node.ensure_valid().await?;
                node.remember_peer_names(&list.devices).await;
                serde_json::to_value(
                    list.devices
                        .into_iter()
                        .filter(|d| {
                            d.channel == crate::profile::PROFILE
                                && d.endpoint != node.endpoint.id().to_string()
                        })
                        .collect::<Vec<_>>(),
                )
                .map_err(|_| "Invalid device list".into())
            }
            "remote_pair" => {
                let (peer, mut code): (String, String) = serde_json::from_value(request.params)
                    .map_err(|_| "Invalid pairing request")?;
                let parsed = pairing::Code::parse(&code);
                zeroize::Zeroize::zeroize(&mut code);
                node.pair(
                    peer.parse().map_err(|_| "Invalid remote identity")?,
                    parsed?,
                )
                .await?;
                Ok(serde_json::Value::Null)
            }
            "remote_reconcile" => {
                let (peer,): (String,) =
                    serde_json::from_value(request.params).map_err(|_| "Invalid device")?;
                node.reconcile(peer.parse().map_err(|_| "Invalid device")?)
                    .await?;
                Ok(serde_json::Value::Null)
            }
            _ => Err("Unknown local remote setting".into()),
        }
    }
}

/// Publication holds a shared policy guard while it makes the attachment
/// durable. Wait for exclusive edits without blocking an async worker or
/// detaching an edit from its caller's account/window/installation guards.
/// The caller must finish its short local edit before awaiting again.
async fn write_policy(store: &RwLock<Store>) -> Result<RwLockWriteGuard<'_, Store>, String> {
    tokio::time::timeout(super::streams::IO_TIMEOUT, async {
        loop {
            {
                match store.try_write() {
                    Ok(store) => return Ok(store),
                    Err(TryLockError::Poisoned(_)) => {
                        return Err("Remote grant unavailable".into());
                    }
                    Err(TryLockError::WouldBlock) => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "Remote device settings are busy; try again".to_string())?
}

impl Node {
    pub(super) async fn valid(&self) -> bool {
        !self.stopped.is_cancelled()
            && self.backend.saved_user_id().as_deref() == Some(&self.account)
            && self.session.remote_session_active().await
    }
    pub(super) async fn ensure_valid(&self) -> Result<(), String> {
        if self.valid().await {
            Ok(())
        } else {
            Err("The remote account is no longer active".into())
        }
    }
    pub(super) fn hello(&self, mode: wire::Mode) -> wire::Hello {
        wire::Hello {
            version: wire::VERSION,
            minimum_version: wire::VERSION,
            account: self.account.clone(),
            project: self.project.clone(),
            deployment: self.deployment.clone(),
            channel: crate::profile::PROFILE.into(),
            mode,
        }
    }
    fn context<'a>(
        &'a self,
        controller: &'a EndpointId,
        execution: &'a EndpointId,
        window: &'a [u8; 16],
    ) -> pairing::Context<'a> {
        pairing::Context {
            controller: controller.as_bytes(),
            execution: execution.as_bytes(),
            account: &self.account,
            project: &self.project,
            deployment: &self.deployment,
            channel: crate::profile::PROFILE,
            window,
        }
    }
    pub(super) async fn publish(&self) -> Result<(), String> {
        // A stale readiness refresh must not overwrite a newly registered W.
        // Local cancel/disable never wait on this network serialization gate.
        let _registration = self.registration.lock().await;
        self.ensure_valid().await?;
        let key = self.store.read().unwrap().key();
        let enabled = self.store.read().unwrap().enabled();
        let name = self.store.read().unwrap().name().to_owned();
        let window = self
            .window
            .lock()
            .await
            .as_ref()
            .filter(|w| w.is_open(&w.id))
            .map(|w| RemoteWindowInit {
                id: Uuid::from_bytes(w.id),
                remaining_seconds: w.remaining().as_secs().min(300) as u32,
            });
        let address = self.endpoint.addr();
        register(
            &self.session,
            &key,
            (&self.account, &self.project, &self.deployment),
            RemoteAddresses {
                relays: address.relay_urls().map(ToString::to_string).collect(),
                direct: address.ip_addrs().map(ToString::to_string).collect(),
            },
            name,
            enabled,
            window,
        )
        .await?;
        self.ensure_valid().await
    }
    async fn current_pairing(&self) -> Option<PairingWindow> {
        self.window
            .lock()
            .await
            .as_ref()
            .filter(|w| w.is_open(&w.id))
            .and_then(|w| {
                Some(PairingWindow {
                    code: w.code()?,
                    window: Uuid::from_bytes(w.id),
                    expires_in: w.remaining().as_secs(),
                })
            })
    }
    async fn begin_pairing(&self) -> Result<PairingWindow, String> {
        let _starting = self.registration.lock().await;
        if !self.store.read().unwrap().enabled() {
            return Err("Enable remote access on this computer first".into());
        }
        if let Some(window) = self.current_pairing().await {
            return Ok(window);
        }
        let generation = self.pairing_generation.load(Ordering::SeqCst);
        let next = pairing::Window::new();
        let key = self.store.read().unwrap().key();
        let name = self.store.read().unwrap().name().to_owned();
        let address = self.endpoint.addr();
        let registration = register(
            &self.session,
            &key,
            (&self.account, &self.project, &self.deployment),
            RemoteAddresses {
                relays: address.relay_urls().map(ToString::to_string).collect(),
                direct: address.ip_addrs().map(ToString::to_string).collect(),
            },
            name,
            true,
            Some(RemoteWindowInit {
                id: Uuid::from_bytes(next.id),
                remaining_seconds: 300,
            }),
        )
        .await?;
        self.ensure_valid().await?;
        let mut window = self.window.lock().await;
        if self.pairing_generation.load(Ordering::SeqCst) != generation
            || !self.store.read().unwrap().enabled()
        {
            return Err("Pairing was cancelled locally".into());
        }
        if registration.window.as_ref().map(|w| w.id) != Some(Uuid::from_bytes(next.id))
            || next.remaining().is_zero()
        {
            return Err("Pairing registration was not acknowledged. Start a new window.".into());
        }
        let result = PairingWindow {
            code: next.code().ok_or("Pairing window expired")?,
            window: Uuid::from_bytes(next.id),
            expires_in: next.remaining().as_secs(),
        };
        *window = Some(next);
        Ok(result)
    }
    pub(super) async fn destination(
        &self,
        peer: EndpointId,
    ) -> Result<(EndpointAddr, RemoteDevice), String> {
        let available =
            tokio::time::timeout(Duration::from_secs(3), self.session.list_remote_devices()).await;
        self.ensure_valid().await?;
        let list = match available {
            Ok(Ok(list)) => list,
            _ => {
                return self
                    .addresses
                    .lock()
                    .unwrap()
                    .get(&peer)
                    .cloned()
                    .ok_or_else(|| {
                        "Remote discovery is unavailable and no previous address is known".into()
                    });
            }
        };
        validate_devices(&list.devices)?;
        self.remember_peer_names(&list.devices).await;
        let device = list
            .devices
            .into_iter()
            .find(|d| d.channel == crate::profile::PROFILE && d.endpoint == peer.to_string())
            .ok_or("The other computer is not currently discoverable")?;
        let mut address = EndpointAddr::new(peer);
        if device.addresses.relays.len() > 4 || device.addresses.direct.len() > 16 {
            return Err("Invalid remote address list".into());
        }
        for relay in &device.addresses.relays {
            address = address.with_relay_url(relay.parse().map_err(|_| "Invalid remote relay")?);
        }
        for ip in &device.addresses.direct {
            address = address.with_ip_addr(ip.parse().map_err(|_| "Invalid remote address")?);
        }
        {
            let mut addresses = self.addresses.lock().unwrap();
            if addresses.len() < 64 || addresses.contains_key(&peer) {
                addresses.insert(peer, (address.clone(), device.clone()));
            }
        }
        Ok((address, device))
    }
    async fn remember_peer_names(&self, devices: &[RemoteDevice]) {
        let names = devices
            .iter()
            .filter(|d| d.channel == crate::profile::PROFILE)
            .map(|d| (d.endpoint.as_str(), d.name.as_str()));
        let result = match write_policy(&self.store).await {
            Ok(mut store) => store.refresh_peer_names(names),
            Err(error) => Err(error),
        };
        if result.is_err() {
            log::warn!("Could not save updated remote computer names");
        }
    }

    /// Pairing authority is already persisted. Resolve the controller's label
    /// separately, so optional discovery metadata cannot delay or fail pairing.
    pub(super) async fn refresh_peer_names(&self) {
        if let Ok(Ok(list)) =
            tokio::time::timeout(Duration::from_secs(3), self.session.list_remote_devices()).await
            && validate_devices(&list.devices).is_ok()
            && self.valid().await
        {
            self.remember_peer_names(&list.devices).await;
        }
    }
    async fn pair(&self, peer: EndpointId, code: pairing::Code) -> Result<(), String> {
        let generation = self.pairing_generation.load(Ordering::SeqCst);
        self.publish().await?;
        let (address, device) = self.destination(peer).await?;
        let window = device
            .window
            .ok_or("Open a pairing window on the other computer first")?
            .id;
        let admission = self
            .session
            .request_remote_admission(RemoteAdmissionRequest {
                controller: self.endpoint.id().to_string(),
                execution: peer.to_string(),
                window,
            })
            .await?;
        self.ensure_valid().await?;
        let controller = self.endpoint.id();
        let proof =
            pairing::Controller::new(code, &self.context(&controller, &peer, window.as_bytes()))?;
        let exchange = async {
            let connection = self
                .endpoint
                .connect(address, wire::ALPN)
                .await
                .map_err(|_| "Could not connect to the other computer")?;
            if connection.remote_id() != peer {
                return Err("The remote identity changed".into());
            }
            let (mut tx, mut rx) = connection
                .open_bi()
                .await
                .map_err(|_| "Pairing connection closed")?;
            wire::write(
                &mut tx,
                &self.hello(wire::Mode::Pair {
                    window: *window.as_bytes(),
                    admission: admission.handle,
                }),
                wire::MAX_PREFACE,
            )
            .await
            .map_err(|e| e.to_string())?;
            // Reserve immediately before the first code-derived byte. A failed
            // dial does not burn a window, but a partial proof can never retry.
            self.ensure_valid().await?;
            {
                let mut store = write_policy(&self.store).await?;
                if self.pairing_generation.load(Ordering::SeqCst) != generation {
                    return Err("Pairing was cancelled locally".into());
                }
                store.reserve_attempt(peer, *window.as_bytes())?;
            }
            wire::write(&mut tx, &proof.message(), wire::MAX_PREFACE)
                .await
                .map_err(|e| e.to_string())?;
            let answer: Result<(Vec<u8>, Vec<u8>), wire::Refusal> =
                wire::read(&mut rx, wire::MAX_PREFACE)
                    .await
                    .map_err(|e| e.to_string())?;
            let (message, host_tag) = answer.map_err(|e| e.message)?;
            let tag = proof.confirm(message, &host_tag)?;
            {
                let _account = self.backend.account_gate.read().await;
                self.ensure_valid().await?;
                let mut store = write_policy(&self.store).await?;
                if self.pairing_generation.load(Ordering::SeqCst) != generation {
                    return Err("Pairing was cancelled locally".into());
                }
                store.remember_host_proof(peer, *window.as_bytes(), device.name)?;
            }
            wire::write(&mut tx, &tag.as_slice(), wire::MAX_PREFACE)
                .await
                .map_err(|e| e.to_string())?;
            let grant: Result<[u8; 16], wire::Refusal> = wire::read(&mut rx, wire::MAX_PREFACE)
                .await
                .map_err(|e| e.to_string())?;
            let grant = grant.map_err(|e| e.message)?;
            if grant != *window.as_bytes() {
                return Err("Invalid pairing acknowledgement".into());
            }
            {
                let _account = self.backend.account_gate.read().await;
                self.ensure_valid().await?;
                let mut store = write_policy(&self.store).await?;
                if self.pairing_generation.load(Ordering::SeqCst) != generation {
                    return Err("Pairing was cancelled locally".into());
                }
                store.confirm_host(peer, grant)?;
            }
            tx.finish().map_err(|_| "Pairing connection closed")?;
            Ok::<_, String>(())
        };
        match tokio::time::timeout(Duration::from_secs(20), exchange).await {
            Ok(Ok(())) => Ok(()),
            result => {
                if self
                    .store
                    .read()
                    .unwrap()
                    .peers()
                    .get(&peer.to_string())
                    .is_some_and(|p| p.pending_confirmation == Some(*window.as_bytes()))
                {
                    return self.reconcile(peer).await;
                }
                result.map_err(|_| "Pairing timed out; open a new window before retrying")?
            }
        }
    }
    pub(super) async fn reconcile(&self, peer: EndpointId) -> Result<(), String> {
        let generation = self.pairing_generation.load(Ordering::SeqCst);
        let window = self
            .store
            .read()
            .unwrap()
            .peers()
            .get(&peer.to_string())
            .and_then(|p| p.pending_confirmation)
            .ok_or("No host confirmation is pending")?;
        let (address, _) = self.destination(peer).await?;
        let confirm = async {
            let connection = self
                .endpoint
                .connect(address, wire::ALPN)
                .await
                .map_err(|_| "Could not reconnect to confirm pairing")?;
            if connection.remote_id() != peer {
                return Err("Remote identity changed".to_string());
            }
            let (mut tx, mut rx) = connection
                .open_bi()
                .await
                .map_err(|_| "Remote connection closed")?;
            wire::write(
                &mut tx,
                &self.hello(wire::Mode::ConfirmGrant { window }),
                wire::MAX_PREFACE,
            )
            .await
            .map_err(|e| e.to_string())?;
            let confirmed: Result<[u8; 16], wire::Refusal> = wire::read(&mut rx, wire::MAX_PREFACE)
                .await
                .map_err(|e| e.to_string())?;
            if confirmed.map_err(|e| e.message)? != window {
                return Err("Invalid grant acknowledgement".into());
            }
            let _account = self.backend.account_gate.read().await;
            self.ensure_valid().await?;
            let mut store = write_policy(&self.store).await?;
            if self.pairing_generation.load(Ordering::SeqCst) != generation {
                return Err("Pairing was cancelled locally".into());
            }
            store.confirm_host(peer, window)
        };
        tokio::time::timeout(Duration::from_secs(15), confirm)
            .await
            .map_err(|_| "Pairing acknowledgement is still pending; reconnect to confirm it")?
    }
    /// Repair only confirmed outgoing metadata in a released record. The caller
    /// holds its local account gate and has already admitted a normal control
    /// connection to this exact endpoint. No pairing proof or install is replayed.
    async fn ensure_host_grant(&self, peer: EndpointId) -> Result<[u8; 16], String> {
        let generation = self.pairing_generation.load(Ordering::SeqCst);
        let expected = {
            let store = self.store.read().map_err(|_| "Remote grant unavailable")?;
            let saved = store
                .peers()
                .get(&peer.to_string())
                .filter(|saved| saved.can_connect)
                .ok_or(PAIRING_CONFIRMATION_REQUIRED)?;
            if let Some(grant) = saved.host_grant {
                return Ok(grant);
            }
            store.host_grant_recovery(peer)?
        };
        let window = recover_confirmed_host_grant(
            &expected.attempts,
            HOST_GRANT_PROBE_TIMEOUT,
            HOST_GRANT_RECOVERY_TIMEOUT,
            |window| self.confirm_saved_host_grant(peer, window),
        )
        .await?;
        self.ensure_valid().await?;
        let mut store = write_policy(&self.store).await?;
        self.backend.authorize_account(&self.account)?;
        if self.pairing_generation.load(Ordering::SeqCst) != generation {
            return Err(PAIRING_CONFIRMATION_REQUIRED.into());
        }
        // The durable edit checks directional authority and the attempt snapshot:
        // ordinary re-pairing does not necessarily change pairing_generation.
        store.recover_host_grant(peer, &expected, window)?;
        Ok(window)
    }

    async fn confirm_saved_host_grant(
        &self,
        peer: EndpointId,
        window: [u8; 16],
    ) -> Result<bool, String> {
        self.ensure_valid().await?;
        let (address, _) = self.destination(peer).await?;
        let connection = self
            .endpoint
            .connect(address, wire::ALPN)
            .await
            .map_err(|_| PAIRING_CONFIRMATION_REQUIRED)?;
        if connection.remote_id() != peer {
            return Err("Remote identity changed".into());
        }
        let (mut tx, mut rx) = connection
            .open_bi()
            .await
            .map_err(|_| PAIRING_CONFIRMATION_REQUIRED)?;
        wire::write(
            &mut tx,
            &self.hello(wire::Mode::ConfirmGrant { window }),
            wire::MAX_PREFACE,
        )
        .await
        .map_err(|error| error.to_string())?;
        let confirmed: Result<[u8; 16], wire::Refusal> = wire::read(&mut rx, wire::MAX_PREFACE)
            .await
            .map_err(|error| error.to_string())?;
        // Only the exact legacy negative acknowledgement permits another
        // candidate. Context, protocol and identity failures stop recovery.
        confirmed_host_grant_reply(window, confirmed)
    }

    pub(super) async fn accept_pairing(
        &self,
        peer: EndpointId,
        window: [u8; 16],
        admission: String,
        tx: &mut iroh::endpoint::SendStream,
        rx: &mut iroh::endpoint::RecvStream,
    ) -> Result<(), String> {
        if !self.store.read().unwrap().enabled() {
            return Err("Remote access is disabled".into());
        }
        if !self
            .window
            .lock()
            .await
            .as_ref()
            .is_some_and(|w| w.is_open(&window))
        {
            return Err("Pairing window is closed".into());
        }
        let admitted = tokio::time::timeout(
            Duration::from_secs(8),
            self.session.check_remote_admission(CheckRemoteAdmission {
                handle: admission,
                controller: peer.to_string(),
                execution: self.endpoint.id().to_string(),
                window: Uuid::from_bytes(window),
            }),
        )
        .await
        .map_err(|_| "Pairing admission check timed out; try again")??;
        self.ensure_valid().await?;
        if !admitted.valid {
            return Err("Pairing admission was rejected".into());
        }
        let code = self
            .window
            .lock()
            .await
            .as_mut()
            .ok_or("Pairing window is closed")?
            .take_attempt(&window)?;
        let message = wire::read(rx, wire::MAX_PREFACE)
            .await
            .map_err(|e| e.to_string())?;
        let execution = self.endpoint.id();
        let proof =
            pairing::Execution::new(code, &self.context(&peer, &execution, &window), message)?;
        wire::write(
            tx,
            &Ok::<_, wire::Refusal>((proof.message(), proof.confirmation().as_slice())),
            wire::MAX_PREFACE,
        )
        .await
        .map_err(|e| e.to_string())?;
        let tag: Vec<u8> = wire::read(rx, wire::MAX_PREFACE)
            .await
            .map_err(|e| e.to_string())?;
        proof.verify(&tag)?;
        let _account = self.backend.account_gate.read().await;
        self.ensure_valid().await?;
        let pending_window = self.window.lock().await;
        if !pending_window
            .as_ref()
            .is_some_and(|w| w.id == window && !w.remaining().is_zero())
        {
            return Err("Pairing window was closed or expired".into());
        }
        {
            let mut store = write_policy(&self.store).await?;
            if !pending_window
                .as_ref()
                .is_some_and(|w| w.id == window && !w.remaining().is_zero())
            {
                return Err("Pairing window was closed or expired".into());
            }
            if !store.enabled() {
                return Err("Remote access is disabled".into());
            }
            store.grant(
                peer,
                Peer {
                    name: "Paired controller".into(),
                    grant: window,
                    host_grant: None,
                    controls_this_computer: true,
                    can_connect: false,
                    pending_confirmation: None,
                },
            )?;
        }
        drop(pending_window);
        wire::write(tx, &Ok::<_, wire::Refusal>(window), wire::MAX_PREFACE)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

async fn register(
    session: &MapleApiSession,
    key: &SecretKey,
    (account, project, deployment): (&str, &str, &str),
    addresses: RemoteAddresses,
    name: String,
    enabled: bool,
    window: Option<RemoteWindowInit>,
) -> Result<RemoteDevice, String> {
    let registration = RemoteRegistration {
        account: account.parse().map_err(|_| "Invalid account")?,
        project: project.parse().map_err(|_| "Invalid project")?,
        deployment: deployment.into(),
        channel: crate::profile::PROFILE.into(),
        endpoint: key.public().to_string(),
        name,
        addresses,
        accepting_connections: enabled,
        window,
    };
    let signature = key
        .sign(
            &registration
                .proof_message()
                .map_err(|_| "Invalid registration")?,
        )
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    tokio::time::timeout(
        Duration::from_secs(10),
        session.register_remote_device(RegisterRemoteDevice {
            registration,
            signature,
        }),
    )
    .await
    .map_err(|_| "Remote registration timed out")?
}

fn configuration(api_url: &str) -> Result<(RelayMode, bool, bool), String> {
    let configured = crate::env::env_string("MAPLE_REMOTE_RELAYS").unwrap_or_default();
    let relay_only = crate::env::env_string("MAPLE_REMOTE_RELAY_ONLY").as_deref() == Some("1");
    let loopback_only = loopback_configuration(
        crate::profile::PROFILE,
        cfg!(debug_assertions),
        api_url,
        &configured,
        relay_only,
        crate::env::env_string("MAPLE_REMOTE_BIND_LOOPBACK").as_deref(),
    )?;
    let (relays, relay_only) =
        relay_configuration(crate::profile::PROFILE, &configured, relay_only)?;
    Ok((relays, relay_only, loopback_only))
}

/// Only an explicit Local debug qualification may restrict Iroh to the same
/// machine. This neither adds an address override nor alters packaged defaults.
fn loopback_configuration(
    profile: &str,
    debug: bool,
    api_url: &str,
    configured_relays: &str,
    relay_only: bool,
    requested: Option<&str>,
) -> Result<bool, String> {
    let Some(requested) = requested else {
        return Ok(false);
    };
    let invalid = || {
        "Loopback remote qualification requires MAPLE_REMOTE_BIND_LOOPBACK=1, an unpackaged debug build, a Local HTTP loopback API, MAPLE_REMOTE_RELAYS=none, and relay-only mode disabled.".to_owned()
    };
    let api = url::Url::parse(api_url).map_err(|_| invalid())?;
    let local_host = match api.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if requested != "1"
        || profile != "unpackaged"
        || !debug
        || api.scheme() != "http"
        || !local_host
        || !api.username().is_empty()
        || api.password().is_some()
        || api.query().is_some()
        || api.fragment().is_some()
        || configured_relays != "none"
        || relay_only
    {
        return Err(invalid());
    }
    Ok(true)
}

fn relay_configuration(
    profile: &str,
    configured: &str,
    relay_only: bool,
) -> Result<(RelayMode, bool), String> {
    // Each packaged profile uses its own Maple relay fleet. Explicit overrides
    // replace the fleet; neither errors nor outages select public relays.
    // This selects relays only; network::builder still disables public lookup.
    let configured = if configured.is_empty() {
        match profile {
            "dev" => "https://relay-dev.maple.ai/",
            "prod" => "https://relay-use1.maple.ai/,https://relay-usw2.maple.ai/",
            _ => configured,
        }
    } else {
        configured
    };
    if configured == "public-test" && profile == "unpackaged" {
        return Ok((RelayMode::Default, relay_only));
    }
    if configured == "none" && profile == "unpackaged" {
        return Ok((RelayMode::Disabled, false));
    }
    let relays: Vec<RelayUrl> = configured
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().map_err(|_| "Invalid remote relay configuration"))
        .collect::<Result<_, _>>()?;
    if relays.is_empty() {
        return Err("Maple remote relay service is not configured for this build".into());
    }
    Ok((
        RelayMode::Custom(RelayMap::from_iter(relays)),
        profile == "unpackaged" && relay_only,
    ))
}

fn validate_devices(devices: &[RemoteDevice]) -> Result<(), String> {
    if devices.len() > 64 {
        return Err("Invalid remote discovery response".into());
    }
    for device in devices {
        let key = device
            .endpoint
            .parse::<EndpointId>()
            .map_err(|_| "Invalid remote discovery identity")?;
        if key.to_string() != device.endpoint
            || device.name.len() > 128
            || device.name.chars().any(char::is_control)
            || device.addresses.direct.len() > 16
            || device.addresses.relays.len() > 4
        {
            return Err("Invalid remote discovery metadata".into());
        }
        for relay in &device.addresses.relays {
            let url = url::Url::parse(relay).map_err(|_| "Invalid remote relay address")?;
            if relay.len() > 512
                || url.scheme() != "https"
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err("Invalid remote relay address".into());
            }
        }
        for direct in &device.addresses.direct {
            direct
                .parse::<std::net::SocketAddr>()
                .map_err(|_| "Invalid remote direct address")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_slow_legacy_confirmation_cannot_erase_an_observed_ready_release() {
        use super::super::update::{
            BlockedReason, PeerInfo, ProtocolInfo, RemoteUpdateStatus, RunningApplication,
            UpdateState,
        };
        let observed = PeerInfo {
            protocol: ProtocolInfo::current(),
            running: RunningApplication::current(),
            update: RemoteUpdateStatus {
                state: UpdateState::Ready,
                ..Default::default()
            },
        };
        let returned = retain_observed_peer_info(
            Some(observed.clone()),
            false,
            tokio::time::Instant::now() + Duration::from_secs(2),
            std::future::pending::<Result<[u8; 16], String>>(),
        )
        .await
        .unwrap();
        assert_eq!(returned.running, observed.running);
        assert_eq!(returned.update.state, UpdateState::Ready);
        assert_eq!(
            returned.update.blocked,
            Some(BlockedReason::PairingConfirmationRequired)
        );

        let mut protected = observed;
        protected.update.blocked = Some(BlockedReason::PasswordProtected);
        let returned = retain_observed_peer_info(
            Some(protected.clone()),
            false,
            tokio::time::Instant::now(),
            std::future::ready(Err(PAIRING_CONFIRMATION_REQUIRED.into())),
        )
        .await;
        assert_eq!(returned, Some(protected));
    }

    #[test]
    fn remote_refresh_preserves_controlled_failure_categories_without_peer_text() {
        use crate::remote_updates::RefreshFailure;
        for failure in [
            RefreshFailure::Busy,
            RefreshFailure::Disabled,
            RefreshFailure::Unavailable,
            RefreshFailure::CheckFailed,
            RefreshFailure::TimedOut,
        ] {
            assert_eq!(remote_refresh_failure(failure.message()), failure.message());
        }
        assert_eq!(
            remote_refresh_failure("synthetic private remote path or account detail"),
            RefreshFailure::CheckFailed.message(),
        );
    }

    #[tokio::test]
    async fn legacy_grant_recovery_needs_an_exact_confirmation_and_has_a_bounded_tail() {
        let attempts: Vec<_> = (1..=12).map(|n| [n; 16]).collect();
        let mut probes = Vec::new();
        let recovered = recover_confirmed_host_grant(
            &attempts,
            Duration::from_secs(1),
            Duration::from_secs(2),
            |candidate| {
                probes.push(candidate);
                std::future::ready(confirmed_host_grant_reply(
                    candidate,
                    if candidate == [10; 16] {
                        Ok(candidate)
                    } else {
                        Err(wire::Refusal::terminal(
                            "No current grant matches this pairing",
                        ))
                    },
                ))
            },
        )
        .await
        .unwrap();
        assert_eq!(recovered, [10; 16]);
        assert_eq!(probes, [[12; 16], [11; 16], [10; 16]]);

        probes.clear();
        let missing = recover_confirmed_host_grant(
            &attempts,
            Duration::from_secs(1),
            Duration::from_secs(2),
            |candidate| {
                probes.push(candidate);
                std::future::ready(Ok(candidate == [1; 16]))
            },
        )
        .await;
        assert_eq!(missing.unwrap_err(), PAIRING_CONFIRMATION_REQUIRED);
        assert_eq!(probes.len(), HOST_GRANT_RECOVERY_LIMIT);
        assert_eq!(probes.last(), Some(&[5; 16]));
    }

    #[tokio::test]
    async fn malformed_or_context_refused_confirmation_cannot_fall_through_to_another_attempt() {
        for response in [
            Ok([9; 16]),
            Err(wire::Refusal::terminal(
                "Remote account or deployment mismatch",
            )),
            Err(wire::Refusal::terminal(
                wire::Error::Incompatible.to_string(),
            )),
        ] {
            let mut response = Some(response);
            let mut calls = 0;
            let result = recover_confirmed_host_grant(
                &[[1; 16], [2; 16]],
                Duration::from_secs(1),
                Duration::from_secs(2),
                |candidate| {
                    calls += 1;
                    std::future::ready(confirmed_host_grant_reply(
                        candidate,
                        response.take().unwrap(),
                    ))
                },
            )
            .await;
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_grant_recovery_times_out_without_trying_another_candidate() {
        let started = tokio::time::Instant::now();
        let mut calls = 0;
        let result = recover_confirmed_host_grant(
            &[[1; 16], [2; 16]],
            Duration::from_secs(3),
            Duration::from_secs(15),
            |_| {
                calls += 1;
                std::future::pending::<Result<bool, String>>()
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), PAIRING_CONFIRMATION_REQUIRED);
        assert_eq!(calls, 1);
        assert_eq!(started.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_grant_recovery_total_budget_bounds_a_slow_negative_history() {
        let started = tokio::time::Instant::now();
        let result = recover_confirmed_host_grant(
            &[[1; 16]; 12],
            Duration::from_secs(3),
            Duration::from_secs(5),
            |_| async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(false)
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), PAIRING_CONFIRMATION_REQUIRED);
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    fn policy_fixture() -> (tempfile::TempDir, Arc<RwLock<Store>>) {
        let root = tempfile::tempdir().unwrap();
        let database = Arc::new(
            maple_agent::store::Store::open(maple_agent::store::StorePaths::new(root.path()))
                .unwrap(),
        );
        database.upsert_account("account", 1).unwrap();
        let mut policy =
            Store::open(database, "local", "project", "unpackaged", "account").unwrap();
        policy.set_enabled(true).unwrap();
        (root, Arc::new(RwLock::new(policy)))
    }

    fn hold_publication_policy(
        policy: Arc<RwLock<Store>>,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _publication = policy.read().unwrap();
            entered.send(()).unwrap();
            resume.recv().unwrap();
        });
        waiting.recv().unwrap();
        (release, worker)
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_waiting_policy_edit_cannot_disable_access_later() {
        let (_root, policy) = policy_fixture();
        let (release, publication) = hold_publication_policy(policy.clone());
        let editing = policy.clone();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let edit = tokio::spawn(async move {
            started.send(()).unwrap();
            write_policy(&editing).await?.set_enabled(false)
        });
        waiting.await.unwrap();
        tokio::task::yield_now().await;
        assert!(!edit.is_finished());
        assert!(policy.read().unwrap().enabled());
        edit.abort();
        assert!(edit.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        publication.join().unwrap();
        tokio::time::advance(super::super::streams::IO_TIMEOUT).await;
        assert!(policy.read().unwrap().enabled());
    }

    #[tokio::test(start_paused = true)]
    async fn publication_cannot_make_a_policy_edit_wait_indefinitely() {
        let (_root, policy) = policy_fixture();
        let (release, publication) = hold_publication_policy(policy.clone());
        let started = tokio::time::Instant::now();
        let result = write_policy(&policy).await;
        assert!(
            matches!(result, Err(error) if error == "Remote device settings are busy; try again")
        );
        assert_eq!(started.elapsed(), super::super::streams::IO_TIMEOUT);
        release.send(()).unwrap();
        publication.join().unwrap();
        assert!(policy.read().unwrap().enabled());
    }

    #[tokio::test]
    async fn peer_info_connection_timeout_is_unreachable_not_legacy() {
        let result = query_peer_information(
            std::future::pending::<Result<(), String>>(),
            |()| async { Ok(None) },
            Duration::from_millis(20),
        )
        .await;
        assert!(result.unwrap_err().contains("connection check"));
    }

    #[tokio::test]
    async fn peer_info_probe_timeout_remains_optional_after_connection() {
        let result = query_peer_information(
            async { Ok(()) },
            |()| std::future::pending::<Result<Option<super::super::update::PeerInfo>, String>>(),
            Duration::from_millis(20),
        )
        .await;
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn peer_info_account_errors_are_not_optional_capability_fallbacks() {
        let result = query_peer_information(
            async { Ok(()) },
            |()| async { Err("The signed-in account changed".into()) },
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.unwrap_err(), "The signed-in account changed");
    }

    #[test]
    fn loopback_qualification_requires_every_local_debug_boundary() {
        for api in [
            "http://localhost:3000",
            "http://127.0.0.1:3000",
            "http://127.4.3.2:3000",
            "http://[::1]:3000",
        ] {
            assert_eq!(
                loopback_configuration("unpackaged", true, api, "none", false, Some("1")),
                Ok(true),
                "{api}"
            );
        }
        let api = "http://127.0.0.1:3000";
        for profile in ["dev", "prod", "unknown"] {
            assert!(loopback_configuration(profile, true, api, "none", false, Some("1")).is_err());
        }
        assert!(
            loopback_configuration("unpackaged", false, api, "none", false, Some("1")).is_err()
        );
        assert!(loopback_configuration("unpackaged", true, api, "none", true, Some("1")).is_err());
        for relays in ["", "public-test", "https://relay.example/"] {
            assert!(
                loopback_configuration("unpackaged", true, api, relays, false, Some("1")).is_err()
            );
        }
        for flag in ["0", "true", "127.0.0.1", ""] {
            assert!(
                loopback_configuration("unpackaged", true, api, "none", false, Some(flag)).is_err()
            );
        }
        for remote in [
            "https://localhost:3000",
            "http://localhost.example:3000",
            "http://192.168.1.2:3000",
            "http://0.0.0.0:3000",
            "http://[::]:3000",
            "http://api.example:3000",
            "http://user@localhost:3000",
            "http://localhost:3000?forward=elsewhere",
            "http://localhost:3000#fragment",
            "not a URL",
        ] {
            assert!(
                loopback_configuration("unpackaged", true, remote, "none", false, Some("1"))
                    .is_err(),
                "{remote}"
            );
        }
        // An absent validation flag leaves existing networking selection alone.
        assert_eq!(
            loopback_configuration("prod", false, "https://api.example", "", false, None),
            Ok(false)
        );
    }

    #[test]
    fn packaged_profiles_default_to_their_maple_relay_fleet_with_normal_path_selection() {
        let fleets: &[(&str, &[&str])] = &[
            ("dev", &["https://relay-dev.maple.ai/"]),
            (
                "prod",
                &[
                    "https://relay-use1.maple.ai/",
                    "https://relay-usw2.maple.ai/",
                ],
            ),
        ];
        for (profile, urls) in fleets {
            for forced in [false, true] {
                let (relays, relay_only) = relay_configuration(profile, "", forced).unwrap();
                let RelayMode::Custom(map) = relays else {
                    panic!("packaged profile did not select its Maple relay fleet");
                };
                assert_eq!(map.len(), urls.len());
                for url in *urls {
                    assert!(map.contains(&url.parse().unwrap()));
                }
                assert!(!relay_only);
            }
        }
    }

    #[test]
    fn explicit_relays_override_packaged_defaults_and_development_modes() {
        let urls = ["https://relay-one.example/", "https://relay-two.example/"];
        for profile in ["dev", "prod", "unpackaged"] {
            for forced in [false, true] {
                let (relays, relay_only) =
                    relay_configuration(profile, &urls.join(","), forced).unwrap();
                let RelayMode::Custom(map) = relays else {
                    panic!("explicit relay configuration was not retained");
                };
                assert_eq!(map.len(), urls.len());
                for url in urls {
                    assert!(map.contains(&url.parse().unwrap()));
                }
                assert_eq!(relay_only, profile == "unpackaged" && forced);
            }
        }
    }

    #[test]
    fn invalid_overrides_never_fall_back_to_default_relays() {
        for profile in ["dev", "prod", "unpackaged"] {
            for configured in [",,,", "not a URL", "https://relay.example/,not a URL"] {
                assert!(relay_configuration(profile, configured, false).is_err());
            }
        }
    }

    #[test]
    fn development_shortcuts_are_not_available_in_packaged_profiles() {
        for profile in ["dev", "prod"] {
            for configured in ["public-test", "none"] {
                assert!(relay_configuration(profile, configured, true).is_err());
            }
        }
    }

    #[test]
    fn unpackaged_networking_still_requires_explicit_configuration() {
        assert!(relay_configuration("unpackaged", "", false).is_err());
        assert!(relay_configuration("unknown", "", false).is_err());
        for forced in [false, true] {
            let (relays, relay_only) =
                relay_configuration("unpackaged", "public-test", forced).unwrap();
            assert!(matches!(relays, RelayMode::Default));
            assert_eq!(relay_only, forced);
            let (relays, relay_only) = relay_configuration("unpackaged", "none", forced).unwrap();
            assert!(matches!(relays, RelayMode::Disabled));
            assert!(!relay_only);
        }
    }
}
