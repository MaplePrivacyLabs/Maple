//! Host-owned remote update admission. The desktop owns the existing signed
//! installer; a remote request can only claim its exact, freshly published offer.
//! Receipt I/O is bounded and off the async executor. Restart never requeues work.

use crate::remote::update::{
    BlockedReason, InstallAccepted, InstallRequest, OperationState, ReleaseIdentity,
    RemoteInstallAuthority, RemoteUpdateOperation, RemoteUpdateStatus, RunningApplication,
    UpdateState,
};
use maple_agent::store::{Store, StoreState, lifecycle::StoreMode};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::watch;
use uuid::Uuid;

const LEASE_TIME: Duration = Duration::from_secs(20);
const MAX_RECEIPTS: usize = 512;
const MAX_JOURNAL: u64 = 1024 * 1024;
const ACK_LIFETIME: Duration = Duration::from_secs(60);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(40);
/// The one desktop completion error a receipt records verbatim. Nothing was
/// stopped or installed, so another request may simply be tried.
pub(crate) const DOWNLOAD_FAILED: &str =
    "This computer could not download or verify the update. Nothing was installed.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RefreshFailure {
    Busy,
    Disabled,
    Unavailable,
    CheckFailed,
    TimedOut,
}
impl RefreshFailure {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Busy => {
                "The remote updater is busy. Wait for its current operation, then check again."
            }
            Self::Disabled => "Updates are unavailable for this remote installation.",
            Self::Unavailable => "Open Maple on the remote computer before checking for updates.",
            Self::CheckFailed => {
                "The remote computer could not check its signed update feed. Its last reported status is not a fresh check."
            }
            Self::TimedOut => {
                "The remote update check timed out. Its last reported status is not a fresh check."
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DesktopRefreshCommand {
    pub id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RefreshCompleteRequest {
    pub expected_account: String,
    pub lease: String,
    pub id: String,
    pub result: Result<RemoteUpdateStatus, RefreshFailure>,
}

#[derive(Clone)]
struct Refresh {
    id: String,
    account: String,
    lease: String,
    expires: Instant,
    claimed: bool,
    completion: watch::Sender<Option<Result<RemoteUpdateStatus, RefreshFailure>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DesktopLease {
    pub id: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttachRequest {
    pub expected_account: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublishRequest {
    pub expected_account: String,
    pub lease: String,
    pub status: RemoteUpdateStatus,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TakeRequest {
    pub expected_account: String,
    pub lease: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompleteRequest {
    pub expected_account: String,
    pub lease: String,
    pub operation_id: String,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DesktopCommand {
    pub operation_id: String,
    pub target: ReleaseIdentity,
}

#[derive(Clone)]
struct Lease {
    id: String,
    account: String,
    expires: Instant,
    status: RemoteUpdateStatus,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Receipt {
    sequence: u64,
    requested_ms: u64,
    account_binding: String,
    peer_binding: String,
    grant_binding: String,
    store_binding: String,
    lease: String,
    operation: RemoteUpdateOperation,
}
#[derive(Clone, Default)]
struct State {
    loaded: bool,
    receipts: BTreeMap<String, Receipt>,
    lease: Option<Lease>,
    authorities: BTreeMap<String, RemoteInstallAuthority>,
    // Ephemeral: a restart never recreates or retries a remote check.
    refresh: Option<Refresh>,
}

pub(crate) struct Broker {
    store: Option<Arc<Store>>,
    directory: Option<PathBuf>,
    state: Arc<Mutex<State>>,
    running: RunningApplication,
}
impl Broker {
    pub(crate) fn new(store: Option<Arc<Store>>) -> Self {
        let directory = store
            .as_ref()
            .and_then(|s| s.paths().root().parent())
            .map(|p| p.join("updates").join("remote-installs"));
        Self {
            store,
            directory,
            state: Arc::new(Mutex::new(State::default())),
            running: RunningApplication::current(),
        }
    }

    async fn access<T: Send + 'static>(
        &self,
        effect: impl FnOnce(&mut State, Option<&Store>) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let state = self.state.clone();
        let directory = self.directory.clone();
        let store = self.store.clone();
        let running = self.running.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = state
                .lock()
                .map_err(|_| "Remote update coordinator is unavailable")?;
            if !state.loaded {
                let directory = directory.as_deref().ok_or("Local storage is unavailable")?;
                let mut receipts = read_journal(directory)?;
                if reconcile_startup(&mut receipts, &running) {
                    // Persist before publishing the reconciled outcome. A later
                    // local upgrade must not turn an observed success back into
                    // an interrupted operation. A failed write serves no work.
                    write_journal(directory, &receipts)?;
                }
                state.receipts = receipts;
                state.loaded = true;
            }
            let before = state.clone();
            expire_requests(&mut state);
            let result = effect(&mut state, store.as_deref());
            if result.is_err() {
                *state = before;
                return result;
            }
            if before.receipts != state.receipts
                && let Err(error) = write_journal(
                    directory.as_deref().ok_or("Local storage is unavailable")?,
                    &state.receipts,
                )
            {
                *state = before;
                return Err(error);
            }
            result
        })
        .await
        .map_err(|_| "Remote update coordinator stopped".to_string())?
    }

    pub(crate) async fn attach(&self, account: String) -> Result<DesktopLease, String> {
        self.access(move |state, _| {
            if state
                .lease
                .as_ref()
                .is_some_and(|l| l.expires > Instant::now())
            {
                return Err("Another desktop owns this computer's updater".into());
            }
            let id = Uuid::new_v4().to_string();
            state.lease = Some(Lease {
                id: id.clone(),
                account,
                expires: Instant::now() + LEASE_TIME,
                status: unavailable(),
            });
            Ok(DesktopLease { id })
        })
        .await
    }

    pub(crate) async fn publish(&self, request: PublishRequest) -> Result<(), String> {
        self.access(move |state, _| {
            if let Some(offered) = &request.status.offered {
                offered.validate()?;
            }
            let lease = lease(state, &request.expected_account, &request.lease)?;
            lease.status = request.status;
            lease.status.operation = None; // The host journal owns operation truth.
            Ok(())
        })
        .await
    }

    pub(crate) async fn request_refresh(
        &self,
        account: String,
        peer: String,
    ) -> Result<RemoteUpdateStatus, String> {
        let (completion, mut response) = watch::channel(None);
        let expected_account = account.clone();
        let (id, owner) = self
            .access(move |state, _| {
                let owner = state
                    .lease
                    .as_ref()
                    .filter(|lease| {
                        lease.account == expected_account && lease.expires > Instant::now()
                    })
                    .ok_or_else(|| RefreshFailure::Unavailable.message().to_owned())?;
                if state.refresh.is_some()
                    || state.receipts.values().any(|receipt| {
                        receipt.lease == owner.id
                            && matches!(
                                receipt.operation.state,
                                OperationState::PendingAck
                                    | OperationState::Queued
                                    | OperationState::Installing
                            )
                    })
                {
                    return Err(RefreshFailure::Busy.message().into());
                }
                let id = Uuid::new_v4().to_string();
                let owner = owner.id.clone();
                state.refresh = Some(Refresh {
                    id: id.clone(),
                    account: expected_account,
                    lease: owner.clone(),
                    expires: Instant::now() + REFRESH_TIMEOUT,
                    claimed: false,
                    completion,
                });
                Ok((id, owner))
            })
            .await?;
        let result = tokio::time::timeout(REFRESH_TIMEOUT, async {
            loop {
                let completed = response.borrow().clone();
                if let Some(result) = completed {
                    return result;
                }
                if response.changed().await.is_err() {
                    return Err(RefreshFailure::Unavailable);
                }
            }
        })
        .await
        .unwrap_or(Err(RefreshFailure::TimedOut));
        // Consume only this generation under the same desktop/account owner.
        // A heartbeat or a replacement desktop cannot manufacture completion.
        self.access(move |state, store| {
            let owned = state.lease.as_ref().is_some_and(|lease| {
                lease.id == owner && lease.account == account && lease.expires > Instant::now()
            });
            if !owned {
                return Ok(Err(RefreshFailure::Unavailable));
            }
            if !state
                .refresh
                .as_ref()
                .is_some_and(|refresh| refresh.id == id && refresh.lease == owner)
            {
                return Ok(Err(result.err().unwrap_or(RefreshFailure::Unavailable)));
            }
            state.refresh = None;
            Ok(result.map(|mut status| {
                apply_store_policy(&mut status, store);
                let account = scope_binding("account", &account);
                let peer = scope_binding("peer", &peer);
                status.operation = state
                    .receipts
                    .values()
                    .filter(|receipt| {
                        receipt.account_binding == account && receipt.peer_binding == peer
                    })
                    .max_by_key(|receipt| receipt.sequence)
                    .map(|receipt| receipt.operation.clone());
                status
            }))
        })
        .await?
        .map_err(|failure| failure.message().to_owned())
    }

    pub(crate) async fn take_refresh(
        &self,
        request: TakeRequest,
    ) -> Result<Option<DesktopRefreshCommand>, String> {
        self.access(move |state, _| {
            lease(state, &request.expected_account, &request.lease)?;
            let Some(refresh) = state.refresh.as_mut().filter(|refresh| {
                refresh.account == request.expected_account
                    && refresh.lease == request.lease
                    && !refresh.claimed
            }) else {
                return Ok(None);
            };
            refresh.claimed = true;
            Ok(Some(DesktopRefreshCommand {
                id: refresh.id.clone(),
            }))
        })
        .await
    }

    pub(crate) async fn complete_refresh(
        &self,
        request: RefreshCompleteRequest,
    ) -> Result<(), String> {
        self.access(move |state, _| {
            lease(state, &request.expected_account, &request.lease)?;
            let refresh = state
                .refresh
                .as_ref()
                .filter(|refresh| {
                    refresh.id == request.id
                        && refresh.account == request.expected_account
                        && refresh.lease == request.lease
                        && refresh.claimed
                })
                .ok_or("Remote update check expired or changed owner")?;
            if refresh.completion.borrow().is_some() {
                return Err("Remote update check already completed".into());
            }
            let mut result = request.result;
            if let Ok(status) = &mut result {
                if let Some(offered) = &status.offered {
                    offered.validate()?;
                }
                status.operation = None;
                state.lease.as_mut().expect("checked lease").status = status.clone();
            }
            refresh.completion.send_replace(Some(result));
            Ok(())
        })
        .await
    }

    pub(crate) async fn take(
        &self,
        request: TakeRequest,
    ) -> Result<Option<DesktopCommand>, String> {
        self.access(move |state, store| {
            lease(state, &request.expected_account, &request.lease)?;
            let account = scope_binding("account", &request.expected_account);
            // Password mode blocks installation, not version/status visibility
            // or the desktop lease heartbeat (including an unlocked store).
            if plain_store(store).is_err() {
                return Ok(None);
            }
            let receipt = state.receipts.values_mut().find(|r| {
                r.account_binding == account
                    && r.lease == request.lease
                    && r.operation.state == OperationState::Queued
            });
            let Some(receipt) = receipt else {
                return Ok(None);
            };
            receipt.operation.state = OperationState::Installing;
            Ok(Some(DesktopCommand {
                operation_id: receipt.operation.operation_id.clone(),
                target: receipt.operation.target.clone(),
            }))
        })
        .await
    }

    pub(crate) async fn complete(&self, request: CompleteRequest) -> Result<(), String> {
        self.access(move |state, _| {
            lease(state, &request.expected_account, &request.lease)?;
            let receipt = state
                .receipts
                .get_mut(&request.operation_id)
                .ok_or("Remote update request is unavailable")?;
            if receipt.account_binding != scope_binding("account", &request.expected_account) || receipt.lease != request.lease {
                return Err("Remote update ownership changed".into());
            }
            if matches!(receipt.operation.state, OperationState::Succeeded | OperationState::Failed) {
                return Ok(());
            }
            if receipt.operation.state != OperationState::Installing {
                return Err("The remote update was not claimed by this desktop".into());
            }
            receipt.operation.state = OperationState::Failed;
            // Successful replacement ends the old host; an alive host has no
            // authority to claim that the new process restarted successfully.
            // Keep caller errors in the desktop's live UI. Durable receipts
            // retain controlled diagnostics only, never an account/path-bearing
            // error copied from a frontend or installation tool.
            receipt.operation.message = Some(
                if request.error.as_deref() == Some(DOWNLOAD_FAILED) {
                    DOWNLOAD_FAILED
                } else {
                    "The update did not complete. Open Maple on this computer to review it before trying again."
                }
                .into(),
            );
            state.authorities.remove(&request.operation_id);
            Ok(())
        })
        .await
    }

    pub(crate) async fn status(
        &self,
        account: &str,
        peer: &str,
    ) -> Result<RemoteUpdateStatus, String> {
        let account = scope_binding("account", account);
        let peer = scope_binding("peer", peer);
        self.access(move |state, store| {
            let mut status = state
                .lease
                .as_ref()
                .filter(|l| {
                    l.expires > Instant::now() && scope_binding("account", &l.account) == account
                })
                .map(|l| l.status.clone())
                .unwrap_or_else(unavailable);
            apply_store_policy(&mut status, store);
            status.operation = state
                .receipts
                .values()
                .filter(|r| r.account_binding == account && r.peer_binding == peer)
                .max_by_key(|r| r.sequence)
                .map(|r| r.operation.clone());
            Ok(status)
        })
        .await
    }

    pub(crate) async fn request_install(
        &self,
        authority: RemoteInstallAuthority,
        request: InstallRequest,
    ) -> Result<InstallAccepted, String> {
        request.validate()?;
        authority.ensure_current().await?;
        let account = authority.account().to_owned();
        let account_binding = scope_binding("account", &account);
        let peer_binding = scope_binding("peer", authority.peer());
        let grant_binding = authority.grant_binding();
        self.access(move |state, store| {
            let id = canonical_id(&request.operation_id)?;
            let store_binding = scope_binding("store", &plain_store(store)?);
            if let Some(previous) = state.receipts.get(&id) {
                if previous.account_binding != account_binding
                    || previous.peer_binding != peer_binding
                    || previous.grant_binding != grant_binding
                    || previous.store_binding != store_binding
                    || previous.operation.target != request.target
                {
                    return Err(
                        "Remote update operation ID was already used for a different request"
                            .into(),
                    );
                }
                return Ok(InstallAccepted {
                    operation_id: id,
                    target: request.target,
                });
            }
            if state.receipts.len() >= MAX_RECEIPTS {
                return Err(
                    "Remote update receipt limit reached; update this computer manually".into(),
                );
            }
            if state.refresh.is_some() {
                return Err("A remote update check is in progress".into());
            }
            let lease = state
                .lease
                .as_ref()
                .filter(|l| l.expires > Instant::now() && l.account == account)
                .ok_or("Open Maple on the remote computer before updating it")?;
            if !admits_request(&lease.status, &request.target) {
                return Err("That exact remote update is no longer ready".into());
            }
            if state.receipts.values().any(|r| {
                r.lease == lease.id
                    && matches!(
                        r.operation.state,
                        OperationState::PendingAck
                            | OperationState::Queued
                            | OperationState::Installing
                    )
            }) {
                return Err("A remote update is already in progress".into());
            }
            let operation = RemoteUpdateOperation {
                operation_id: id.clone(),
                target: request.target.clone(),
                state: OperationState::PendingAck,
                message: None,
            };
            let sequence = state
                .receipts
                .values()
                .map(|r| r.sequence)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or("Remote update receipt limit reached")?;
            state.receipts.insert(
                id.clone(),
                Receipt {
                    sequence,
                    requested_ms: now_ms(),
                    account_binding,
                    peer_binding,
                    grant_binding,
                    store_binding,
                    lease: lease.id.clone(),
                    operation,
                },
            );
            state.authorities.insert(id.clone(), authority);
            Ok(InstallAccepted {
                operation_id: id,
                target: request.target,
            })
        })
        .await
    }

    pub(crate) async fn acknowledge(
        &self,
        authority: &RemoteInstallAuthority,
        operation_id: &str,
    ) -> Result<(), String> {
        authority.ensure_current().await?;
        let account = scope_binding("account", authority.account());
        let peer = scope_binding("peer", authority.peer());
        let grant_binding = authority.grant_binding();
        let id = operation_id.to_owned();
        self.access(move |state, store| {
            let store_binding = scope_binding("store", &plain_store(store)?);
            let receipt = state
                .receipts
                .get_mut(&id)
                .ok_or("Remote update request is unavailable")?;
            if receipt.account_binding != account
                || receipt.peer_binding != peer
                || receipt.grant_binding != grant_binding
                || receipt.store_binding != store_binding
            {
                return Err("Remote update ownership changed".into());
            }
            if receipt.operation.state == OperationState::PendingAck {
                receipt.operation.state = OperationState::Queued;
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn cancel_pending(
        &self,
        authority: &RemoteInstallAuthority,
        operation_id: &str,
    ) -> Result<(), String> {
        let account = scope_binding("account", authority.account());
        let peer = scope_binding("peer", authority.peer());
        let grant_binding = authority.grant_binding();
        let id = operation_id.to_owned();
        self.access(move |state, _| {
            if let Some(receipt) = state.receipts.get_mut(&id) {
                if receipt.account_binding != account
                    || receipt.peer_binding != peer
                    || receipt.grant_binding != grant_binding
                {
                    return Err("Remote update ownership changed".into());
                }
                if receipt.operation.state == OperationState::PendingAck {
                    receipt.operation.state = OperationState::Failed;
                    receipt.operation.message = Some(
                        "The controller did not acknowledge this request. No update was started."
                            .into(),
                    );
                    state.authorities.remove(&id);
                }
            }
            Ok(())
        })
        .await
    }

    /// Caller holds OwnerContext's operation and exclusive local admission.
    /// This orders the final password/account/grant decision with local edits.
    pub(crate) async fn authorize_stop(
        &self,
        operation: [u8; 16],
        account: Option<String>,
    ) -> Result<(), String> {
        let id = Uuid::from_bytes(operation).to_string();
        let account = account.map(|account| scope_binding("account", &account));
        let lookup = id.clone();
        let authority = self
            .access(move |state, store| {
                plain_store(store)?;
                state
                    .authorities
                    .get(&lookup)
                    .cloned()
                    .ok_or_else(|| "Remote update authorization expired".into())
            })
            .await?;
        authority.ensure_current().await?;
        self.access(move |state, store| {
            authority.with_current_grant(|| {
                let store_binding = scope_binding("store", &plain_store(store)?);
                let receipt = state
                    .receipts
                    .get(&id)
                    .ok_or("Remote update request is unavailable")?;
                if account.as_deref() != Some(receipt.account_binding.as_str())
                    || receipt.store_binding != store_binding
                    || receipt.operation.state != OperationState::Installing
                {
                    return Err("Remote update ownership changed".into());
                }
                let lease = state
                    .lease
                    .as_ref()
                    .filter(|l| {
                        l.expires > Instant::now()
                            && l.id == receipt.lease
                            && scope_binding("account", &l.account) == receipt.account_binding
                    })
                    .ok_or("The remote desktop updater is no longer available")?;
                if lease.status.offered.as_ref() != Some(&receipt.operation.target)
                    || !matches!(
                        lease.status.state,
                        UpdateState::Ready | UpdateState::Installing
                    )
                    || lease.status.blocked.is_some()
                {
                    return Err("That exact remote update is no longer ready".into());
                }
                // This is the install-admission linearization point, ordered
                // with incoming re-pair/revoke by the captured grant's mutex.
                state.authorities.remove(&id);
                Ok(())
            })
        })
        .await
    }
}

fn reconcile_startup(
    receipts: &mut BTreeMap<String, Receipt>,
    running: &RunningApplication,
) -> bool {
    let mut changed = false;
    for receipt in receipts.values_mut() {
        if matches!(
            receipt.operation.state,
            OperationState::Succeeded | OperationState::Failed
        ) {
            continue;
        }
        let target = &receipt.operation.target;
        let observed_target = receipt.operation.state == OperationState::Installing
            && running.version == target.version
            && running.source_sha == target.source_sha
            && running
                .build
                .as_ref()
                .is_none_or(|build| *build == target.build);
        if observed_target {
            receipt.operation.state = OperationState::Succeeded;
            // This reports the running target; it does not invent a packaging
            // build when the executable's runtime metadata has none, or claim
            // which install path caused that version to start.
            receipt.operation.message =
                Some("The requested version and source are running on this computer.".into());
        } else {
            receipt.operation.state = OperationState::Failed;
            receipt.operation.message = Some(
                "Previous update was interrupted; check the remote computer before trying again."
                    .into(),
            );
        }
        changed = true;
    }
    // Reconciliation is outcome reporting only. It never recreates a lease,
    // captured authority, acknowledgement or queued installation.
    changed
}

/// An Available offer is downloaded and verified by the execution desktop
/// before it stops anything; the typed stop still requires Ready or Installing.
fn admits_request(status: &RemoteUpdateStatus, target: &ReleaseIdentity) -> bool {
    matches!(status.state, UpdateState::Ready | UpdateState::Available)
        && status.blocked.is_none()
        && status.offered.as_ref() == Some(target)
}

fn lease<'a>(state: &'a mut State, account: &str, id: &str) -> Result<&'a mut Lease, String> {
    let lease = state
        .lease
        .as_mut()
        .filter(|l| l.id == id && l.account == account && l.expires > Instant::now())
        .ok_or("Desktop updater lease expired")?;
    lease.expires = Instant::now() + LEASE_TIME;
    Ok(lease)
}
fn canonical_id(id: &str) -> Result<String, String> {
    let parsed = Uuid::parse_str(id).map_err(|_| "Invalid remote update operation ID")?;
    if parsed.to_string() != id {
        return Err("Invalid remote update operation ID".into());
    }
    Ok(id.to_owned())
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
/// Receipt scope only: no credentials, plaintext identity or key material is
/// retained outside the store. These bindings are correlation metadata, not
/// authority; each effect still requires its live captured grant and lease.
fn scope_binding(domain: &str, value: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"maple-agent-remote-update-receipt-v1\0");
    hash.update(domain.as_bytes());
    hash.update([0]);
    hash.update(value.as_bytes());
    format!("{:x}", hash.finalize())
}
fn expire_requests(state: &mut State) {
    let now = Instant::now();
    if state.refresh.as_ref().is_some_and(|refresh| {
        refresh.expires <= now
            || !state.lease.as_ref().is_some_and(|lease| {
                lease.id == refresh.lease && lease.account == refresh.account && lease.expires > now
            })
    }) {
        state.refresh = None;
    }
    let milliseconds = now_ms();
    for (id, receipt) in &mut state.receipts {
        let active = state.lease.as_ref().is_some_and(|l| {
            l.id == receipt.lease
                && scope_binding("account", &l.account) == receipt.account_binding
                && l.expires > now
        });
        let pending_expired = matches!(
            receipt.operation.state,
            OperationState::PendingAck | OperationState::Queued
        ) && milliseconds.saturating_sub(receipt.requested_ms)
            > ACK_LIFETIME.as_millis() as u64;
        if matches!(
            receipt.operation.state,
            OperationState::PendingAck | OperationState::Queued | OperationState::Installing
        ) && (!active || pending_expired)
        {
            receipt.operation.state = OperationState::Failed;
            receipt.operation.message = Some("The update request expired or its desktop disconnected. Check the remote computer before trying again.".into());
            state.authorities.remove(id);
        }
    }
}
fn plain_store(store: Option<&Store>) -> Result<String, String> {
    let store = store.ok_or("Local storage is unavailable")?;
    let status = store.lock_status().map_err(|e| e.to_string())?;
    if status.mode != StoreMode::Plain {
        return Err("Remote updates are unavailable for password-protected local data, even while unlocked. Update this computer locally.".into());
    }
    if status.state != StoreState::Open {
        return Err("Remote local data is not ready".into());
    }
    status
        .store_id
        .ok_or_else(|| "Remote local data identity is unavailable".into())
}
fn apply_store_policy(status: &mut RemoteUpdateStatus, store: Option<&Store>) {
    match store.map(Store::lock_status) {
        Some(Ok(lock)) if lock.mode == StoreMode::Password => {
            status.blocked = Some(BlockedReason::PasswordProtected);
        }
        Some(Ok(lock)) if lock.state == StoreState::Open => {}
        Some(_) => status.blocked = Some(BlockedReason::Busy),
        None => status.blocked = Some(BlockedReason::DesktopUnavailable),
    }
}
fn unavailable() -> RemoteUpdateStatus {
    RemoteUpdateStatus {
        state: UpdateState::Unknown,
        offered: None,
        blocked: Some(BlockedReason::DesktopUnavailable),
        operation: None,
    }
}

fn private_directory(path: &Path) -> Result<(), String> {
    // The updates ancestor is part of this authority boundary. Never follow a
    // pre-existing link there or create a receipt child through a broad-mode
    // directory outside the profile. Both components are nonrecursive.
    let updates = path
        .parent()
        .ok_or("Invalid remote update receipt directory")?;
    private_directory_component(updates)?;
    private_directory_component(path)
}

fn private_directory_component(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Err(error) => return Err(error.to_string()),
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() {
        return Err("Unsafe remote update receipt directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err("Unsafe remote update receipt directory permissions".into());
        }
    }
    Ok(())
}
fn open_options() -> std::fs::OpenOptions {
    let options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = options;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        options
    };
    options
}
fn verify_file(file: &std::fs::File) -> Result<(), String> {
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_JOURNAL {
        return Err("Unsafe remote update receipts".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err("Unsafe remote update receipt permissions".into());
        }
    }
    Ok(())
}
fn read_journal(directory: &Path) -> Result<BTreeMap<String, Receipt>, String> {
    use std::io::Read;
    private_directory(directory)?;
    let file = match open_options()
        .read(true)
        .open(directory.join("receipts.json"))
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.to_string()),
    };
    verify_file(&file)?;
    let mut bytes = Vec::new();
    file.take(MAX_JOURNAL + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_JOURNAL as usize {
        return Err("Remote update receipt limit exceeded".into());
    }
    let receipts: BTreeMap<String, Receipt> =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid remote update receipts")?;
    if receipts.len() > MAX_RECEIPTS {
        return Err("Remote update receipt limit exceeded".into());
    }
    for (id, receipt) in &receipts {
        if canonical_id(id)? != receipt.operation.operation_id {
            return Err("Invalid remote update receipt identity".into());
        }
        for binding in [
            &receipt.account_binding,
            &receipt.peer_binding,
            &receipt.grant_binding,
            &receipt.store_binding,
        ] {
            if binding.len() != 64
                || !binding
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err("Invalid remote update receipt scope".into());
            }
        }
        receipt.operation.target.validate()?;
    }
    Ok(receipts)
}
fn write_journal(directory: &Path, receipts: &BTreeMap<String, Receipt>) -> Result<(), String> {
    use std::io::Write;
    private_directory(directory)?;
    let bytes = serde_json::to_vec(receipts).map_err(|_| "Cannot encode remote update receipts")?;
    if bytes.len() > MAX_JOURNAL as usize {
        return Err("Remote update receipt limit exceeded".into());
    }
    let temporary = directory.join(format!(".receipts-{}", Uuid::new_v4()));
    let result = (|| {
        let mut file = open_options()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        verify_file(&file)?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, directory.join("receipts.json")).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        std::fs::File::open(directory)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use maple_agent::store::{
        StorePaths,
        keys::{KdfParams, Password},
    };

    fn fixture() -> (tempfile::TempDir, Arc<Store>, Broker) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(StorePaths::new(directory.path())).unwrap());
        let broker = Broker::new(Some(store.clone()));
        (directory, store, broker)
    }

    async fn claim_refresh(broker: &Broker, lease: &DesktopLease) -> DesktopRefreshCommand {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(command) = broker
                    .take_refresh(TakeRequest {
                        expected_account: "alice".into(),
                        lease: lease.id.clone(),
                    })
                    .await
                    .unwrap()
                {
                    return command;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued refresh must become claimable")
    }

    #[tokio::test]
    async fn fresh_refresh_requires_one_claimed_generation_not_a_cached_publication() {
        let (_directory, _store, broker) = fixture();
        let broker = Arc::new(broker);
        let lease = desktop(&broker).await;
        let waiting = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .request_refresh("alice".into(), "paired-peer".into())
                    .await
            }
        });
        let command = claim_refresh(&broker, &lease).await;
        broker
            .publish(PublishRequest {
                expected_account: "alice".into(),
                lease: lease.id.clone(),
                status: RemoteUpdateStatus {
                    state: UpdateState::Current,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        assert!(
            !waiting.is_finished(),
            "a heartbeat is not a fresh-check completion"
        );
        assert!(
            broker
                .take_refresh(TakeRequest {
                    expected_account: "alice".into(),
                    lease: lease.id.clone(),
                })
                .await
                .unwrap()
                .is_none(),
            "a check is claimed only once"
        );
        assert!(
            broker
                .request_refresh("alice".into(), "paired-peer".into())
                .await
                .unwrap_err()
                .contains("busy")
        );
        assert!(
            broker
                .complete_refresh(RefreshCompleteRequest {
                    expected_account: "alice".into(),
                    lease: lease.id.clone(),
                    id: Uuid::new_v4().to_string(),
                    result: Ok(RemoteUpdateStatus::default()),
                })
                .await
                .is_err()
        );
        assert!(!waiting.is_finished());
        let fresh = RemoteUpdateStatus {
            state: UpdateState::Available,
            offered: Some(target()),
            ..Default::default()
        };
        broker
            .complete_refresh(RefreshCompleteRequest {
                expected_account: "alice".into(),
                lease: lease.id.clone(),
                id: command.id,
                result: Ok(fresh.clone()),
            })
            .await
            .unwrap();
        assert_eq!(waiting.await.unwrap().unwrap(), fresh);
        assert!(broker.state.lock().unwrap().receipts.is_empty());
    }

    #[tokio::test]
    async fn failed_or_disconnected_refresh_never_returns_the_old_offer() {
        for disconnect in [false, true] {
            let (_directory, _store, broker) = fixture();
            let broker = Arc::new(broker);
            let lease = desktop(&broker).await;
            let waiting = tokio::spawn({
                let broker = broker.clone();
                async move {
                    broker
                        .request_refresh("alice".into(), "paired-peer".into())
                        .await
                }
            });
            let command = claim_refresh(&broker, &lease).await;
            if disconnect {
                broker
                    .access(|state, _| {
                        state.lease.as_mut().unwrap().expires =
                            Instant::now() - Duration::from_secs(1);
                        Ok(())
                    })
                    .await
                    .unwrap();
                let _ = broker.status("alice", "paired-peer").await.unwrap();
            } else {
                broker
                    .complete_refresh(RefreshCompleteRequest {
                        expected_account: "alice".into(),
                        lease: lease.id.clone(),
                        id: command.id,
                        result: Err(RefreshFailure::CheckFailed),
                    })
                    .await
                    .unwrap();
            }
            let result = tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap()
                .unwrap();
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn refresh_rechecks_store_policy_after_the_desktop_check() {
        let (_directory, store, broker) = fixture();
        let broker = Arc::new(broker);
        let lease = desktop(&broker).await;
        let waiting = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .request_refresh("alice".into(), "paired-peer".into())
                    .await
            }
        });
        let command = claim_refresh(&broker, &lease).await;
        store.close().unwrap();
        broker
            .complete_refresh(RefreshCompleteRequest {
                expected_account: "alice".into(),
                lease: lease.id.clone(),
                id: command.id,
                result: Ok(RemoteUpdateStatus {
                    state: UpdateState::Ready,
                    offered: Some(target()),
                    ..Default::default()
                }),
            })
            .await
            .unwrap();
        let status = waiting.await.unwrap().unwrap();
        assert_eq!(status.blocked, Some(BlockedReason::Busy));
    }
    fn target() -> ReleaseIdentity {
        ReleaseIdentity {
            version: "0.1.0".into(),
            build: "600".into(),
            source_sha: "a".repeat(40),
            manifest_sha256: "b".repeat(64),
        }
    }
    fn running_target() -> RunningApplication {
        let target = target();
        RunningApplication {
            version: target.version,
            build: Some(target.build),
            source_sha: target.source_sha,
            platform: "fixture-platform".into(),
        }
    }
    fn reopen_as(store: Arc<Store>, running: RunningApplication) -> Broker {
        let mut broker = Broker::new(Some(store));
        broker.running = running;
        broker
    }
    async fn desktop(broker: &Broker) -> DesktopLease {
        let lease = broker.attach("alice".into()).await.unwrap();
        broker
            .publish(PublishRequest {
                expected_account: "alice".into(),
                lease: lease.id.clone(),
                status: RemoteUpdateStatus {
                    state: UpdateState::Ready,
                    offered: Some(target()),
                    blocked: None,
                    operation: None,
                },
            })
            .await
            .unwrap();
        lease
    }
    async fn receipt(broker: &Broker, lease: &DesktopLease, phase: OperationState) -> String {
        let id = Uuid::new_v4().to_string();
        let key = id.clone();
        let lease = lease.id.clone();
        broker
            .access(move |state, store| {
                let sequence = state
                    .receipts
                    .values()
                    .map(|r| r.sequence)
                    .max()
                    .unwrap_or(0)
                    + 1;
                state.receipts.insert(
                    key.clone(),
                    Receipt {
                        sequence,
                        requested_ms: now_ms(),
                        account_binding: scope_binding("account", "alice"),
                        peer_binding: scope_binding("peer", "paired-peer"),
                        grant_binding: "c".repeat(64),
                        store_binding: scope_binding(
                            "store",
                            &store.unwrap().lock_status().unwrap().store_id.unwrap(),
                        ),
                        lease,
                        operation: RemoteUpdateOperation {
                            operation_id: key,
                            target: target(),
                            state: phase,
                            message: None,
                        },
                    },
                );
                Ok(())
            })
            .await
            .unwrap();
        id
    }
    fn take_request(lease: &DesktopLease) -> TakeRequest {
        TakeRequest {
            expected_account: "alice".into(),
            lease: lease.id.clone(),
        }
    }

    #[tokio::test]
    async fn only_acknowledged_work_is_claimed_once_and_restart_never_replays_it() {
        let (_directory, store, broker) = fixture();
        let lease = desktop(&broker).await;
        let pending = receipt(&broker, &lease, OperationState::PendingAck).await;
        assert!(broker.take(take_request(&lease)).await.unwrap().is_none());
        broker
            .access(move |state, _| {
                state.receipts.get_mut(&pending).unwrap().operation.state = OperationState::Queued;
                Ok(())
            })
            .await
            .unwrap();
        let command = broker.take(take_request(&lease)).await.unwrap().unwrap();
        assert_eq!(command.target, target());
        assert!(broker.take(take_request(&lease)).await.unwrap().is_none());
        drop(broker);
        let reopened = Broker::new(Some(store));
        let next_lease = desktop(&reopened).await;
        assert!(
            reopened
                .take(take_request(&next_lease))
                .await
                .unwrap()
                .is_none()
        );
        let outcome = reopened
            .status("alice", "paired-peer")
            .await
            .unwrap()
            .operation
            .unwrap();
        assert_eq!(outcome.operation_id, command.operation_id);
        assert_eq!(outcome.state, OperationState::Failed);
        assert!(outcome.message.unwrap().contains("interrupted"));
        assert!(
            reopened
                .status("bob", "paired-peer")
                .await
                .unwrap()
                .operation
                .is_none()
        );
        assert!(
            reopened
                .status("alice", "another-peer")
                .await
                .unwrap()
                .operation
                .is_none()
        );
    }

    #[tokio::test]
    async fn observed_success_is_durable_and_survives_a_later_local_upgrade() {
        let (_directory, store, broker) = fixture();
        let lease = desktop(&broker).await;
        let id = receipt(&broker, &lease, OperationState::Queued).await;
        assert_eq!(
            broker
                .take(take_request(&lease))
                .await
                .unwrap()
                .unwrap()
                .operation_id,
            id
        );
        let journal = broker.directory.clone().unwrap();
        drop(broker);

        let reopened = reopen_as(store.clone(), running_target());
        // A read-only status request is enough to reconcile and durably save
        // startup success; no subsequent queue/publication mutation is needed.
        let outcome = reopened
            .status("alice", "paired-peer")
            .await
            .unwrap()
            .operation
            .unwrap();
        assert_eq!(outcome.state, OperationState::Succeeded);
        assert_eq!(
            read_journal(&journal).unwrap()[&id].operation.state,
            OperationState::Succeeded
        );
        assert!(reopened.state.lock().unwrap().authorities.is_empty());
        let new_lease = desktop(&reopened).await;
        assert!(
            reopened
                .take(take_request(&new_lease))
                .await
                .unwrap()
                .is_none()
        );
        drop(reopened);

        let mut later = running_target();
        later.version = "0.2.0".into();
        later.build = Some("700".into());
        later.source_sha = "d".repeat(40);
        let later_host = reopen_as(store, later);
        assert_eq!(
            later_host
                .status("alice", "paired-peer")
                .await
                .unwrap()
                .operation
                .unwrap()
                .state,
            OperationState::Succeeded
        );
        assert_eq!(
            read_journal(&journal).unwrap()[&id].operation.state,
            OperationState::Succeeded
        );
    }

    #[tokio::test]
    async fn startup_preserves_unknown_build_honestly_but_requires_matching_known_identity() {
        for mismatch in [None, Some("version"), Some("source"), Some("build")] {
            let (_directory, store, broker) = fixture();
            let lease = desktop(&broker).await;
            let id = receipt(&broker, &lease, OperationState::Installing).await;
            let journal = broker.directory.clone().unwrap();
            drop(broker);
            let mut running = running_target();
            match mismatch {
                None => running.build = None, // Actual executables currently omit packaging build.
                Some("version") => running.version = "0.0.9".into(),
                Some("source") => running.source_sha = "c".repeat(40),
                Some("build") => running.build = Some("601".into()),
                _ => unreachable!(),
            }
            let reopened = reopen_as(store.clone(), running);
            let expected = if mismatch.is_none() {
                OperationState::Succeeded
            } else {
                OperationState::Failed
            };
            assert_eq!(
                reopened
                    .status("alice", "paired-peer")
                    .await
                    .unwrap()
                    .operation
                    .unwrap()
                    .state,
                expected
            );
            assert_eq!(
                read_journal(&journal).unwrap()[&id].operation.state,
                expected
            );
            drop(reopened);
            // A later startup may not reinterpret an already terminal result.
            let later = reopen_as(store, running_target());
            assert_eq!(
                later
                    .status("alice", "paired-peer")
                    .await
                    .unwrap()
                    .operation
                    .unwrap()
                    .state,
                expected
            );
        }
    }

    #[tokio::test]
    async fn startup_fails_unclaimed_work_and_never_recreates_install_admission() {
        let (_directory, store, broker) = fixture();
        let lease = desktop(&broker).await;
        let pending = receipt(&broker, &lease, OperationState::PendingAck).await;
        let queued = receipt(&broker, &lease, OperationState::Queued).await;
        let installing = receipt(&broker, &lease, OperationState::Installing).await;
        let journal = broker.directory.clone().unwrap();
        drop(broker);
        let reopened = reopen_as(store, running_target());
        reopened.status("alice", "paired-peer").await.unwrap();
        let saved = read_journal(&journal).unwrap();
        assert_eq!(saved[&pending].operation.state, OperationState::Failed);
        assert_eq!(saved[&queued].operation.state, OperationState::Failed);
        assert_eq!(
            saved[&installing].operation.state,
            OperationState::Succeeded
        );
        let lease = desktop(&reopened).await;
        assert!(reopened.take(take_request(&lease)).await.unwrap().is_none());
        assert!(
            reopened
                .authorize_stop(
                    *Uuid::parse_str(&installing).unwrap().as_bytes(),
                    Some("alice".into())
                )
                .await
                .is_err()
        );
    }

    #[test]
    fn requests_admit_only_the_exact_unblocked_ready_or_available_offer() {
        let status = |state, blocked| RemoteUpdateStatus {
            state,
            offered: Some(target()),
            blocked,
            operation: None,
        };
        assert!(admits_request(&status(UpdateState::Ready, None), &target()));
        assert!(admits_request(
            &status(UpdateState::Available, None),
            &target()
        ));
        for state in [
            UpdateState::Downloading,
            UpdateState::Installing,
            UpdateState::Error,
            UpdateState::Current,
            UpdateState::Unknown,
        ] {
            assert!(
                !admits_request(&status(state, None), &target()),
                "{state:?}"
            );
        }
        for blocked in [
            BlockedReason::PasswordProtected,
            BlockedReason::UnsupportedInstallation,
            BlockedReason::UpdateNotReady,
            BlockedReason::Busy,
        ] {
            assert!(!admits_request(
                &status(UpdateState::Available, Some(blocked)),
                &target()
            ));
        }
        let mut changed = target();
        changed.manifest_sha256 = "c".repeat(64);
        assert!(!admits_request(
            &status(UpdateState::Available, None),
            &changed
        ));
    }

    #[tokio::test]
    async fn a_failed_remote_download_is_recorded_without_arbitrary_desktop_errors() {
        let (_directory, _store, broker) = fixture();
        let lease = desktop(&broker).await;
        for (error, expected) in [
            (Some(DOWNLOAD_FAILED.to_owned()), DOWNLOAD_FAILED),
            (
                Some("/Users/alice/private path".to_owned()),
                "The update did not complete. Open Maple on this computer to review it before trying again.",
            ),
        ] {
            let id = receipt(&broker, &lease, OperationState::Installing).await;
            broker
                .complete(CompleteRequest {
                    expected_account: "alice".into(),
                    lease: lease.id.clone(),
                    operation_id: id,
                    error,
                })
                .await
                .unwrap();
            let operation = broker
                .status("alice", "paired-peer")
                .await
                .unwrap()
                .operation
                .unwrap();
            assert_eq!(operation.state, OperationState::Failed);
            assert_eq!(operation.message.as_deref(), Some(expected));
        }
    }

    #[tokio::test]
    async fn repeated_completion_is_idempotent_and_cannot_overwrite_terminal_success() {
        let (_directory, _store, broker) = fixture();
        let lease = desktop(&broker).await;
        let id = receipt(&broker, &lease, OperationState::Installing).await;
        let completion = || CompleteRequest {
            expected_account: "alice".into(),
            lease: lease.id.clone(),
            operation_id: id.clone(),
            error: None,
        };
        broker.complete(completion()).await.unwrap();
        broker.complete(completion()).await.unwrap();
        assert_eq!(
            broker
                .status("alice", "paired-peer")
                .await
                .unwrap()
                .operation
                .unwrap()
                .state,
            OperationState::Failed
        );
        let key = id.clone();
        broker
            .access(move |state, _| {
                state.receipts.get_mut(&key).unwrap().operation.state = OperationState::Succeeded;
                Ok(())
            })
            .await
            .unwrap();
        broker.complete(completion()).await.unwrap();
        assert_eq!(
            broker
                .status("alice", "paired-peer")
                .await
                .unwrap()
                .operation
                .unwrap()
                .state,
            OperationState::Succeeded
        );
        let pending = receipt(&broker, &lease, OperationState::PendingAck).await;
        assert!(
            broker
                .complete(CompleteRequest {
                    operation_id: pending,
                    ..completion()
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn failed_durable_claim_never_reaches_the_desktop() {
        let (_directory, _store, broker) = fixture();
        let lease = desktop(&broker).await;
        let id = receipt(&broker, &lease, OperationState::Queued).await;
        let path = broker.directory.as_ref().unwrap().join("receipts.json");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap(); // Atomic journal replacement must fail.
        assert!(broker.take(take_request(&lease)).await.is_err());
        assert_eq!(
            broker.state.lock().unwrap().receipts[&id].operation.state,
            OperationState::Queued
        );
        std::fs::remove_dir(&path).unwrap();
        assert_eq!(
            broker
                .take(take_request(&lease))
                .await
                .unwrap()
                .unwrap()
                .operation_id,
            id
        );
    }

    #[tokio::test]
    async fn expired_or_unacknowledged_operations_cannot_block_a_live_desktop_forever() {
        let (_directory, _store, broker) = fixture();
        let lease = desktop(&broker).await;
        let id = receipt(&broker, &lease, OperationState::PendingAck).await;
        {
            let mut state = broker.state.lock().unwrap();
            state.receipts.get_mut(&id).unwrap().requested_ms =
                now_ms().saturating_sub(ACK_LIFETIME.as_millis() as u64 + 1);
        }
        assert!(broker.take(take_request(&lease)).await.unwrap().is_none());
        assert_eq!(
            broker
                .status("alice", "paired-peer")
                .await
                .unwrap()
                .operation
                .unwrap()
                .state,
            OperationState::Failed
        );
        broker.state.lock().unwrap().lease.as_mut().unwrap().expires =
            Instant::now() - Duration::from_secs(1);
        assert!(broker.take(take_request(&lease)).await.is_err());
        let replacement = desktop(&broker).await;
        assert_ne!(replacement.id, lease.id);
        assert!(
            broker
                .publish(PublishRequest {
                    expected_account: "alice".into(),
                    lease: lease.id,
                    status: unavailable()
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn password_mode_blocks_installation_even_unlocked_but_keeps_version_visibility() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path());
        let store = tokio::task::spawn_blocking(move || {
            use maple_agent::store::keys::{KeyFile, KeyFilePair, LockedKey};
            use std::io::Read;
            // Match the existing host lifecycle fixture: a real SQLCipher
            // database and a salt-bound wrapping slot. App tests do not expose
            // the production integrity-check child needed for export-swap.
            let key_bytes = [0x72; 32];
            let store = Arc::new(
                Store::open_keyed(paths, LockedKey::from_bytes(key_bytes).unwrap()).unwrap(),
            );
            let mut salt = [0; 16];
            std::fs::File::open(store.paths().database())
                .unwrap()
                .read_exact(&mut salt)
                .unwrap();
            let password = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
            let slot = KeyFile::create(
                &password,
                &[],
                &LockedKey::from_bytes(key_bytes).unwrap(),
                salt,
                KdfParams::floor(),
            )
            .unwrap();
            KeyFilePair::write(&store.paths().db(), &slot).unwrap();
            store.upsert_account("protected-sentinel", 1).unwrap();
            store.close().unwrap();
            store.initialize_recovered().unwrap();
            assert_eq!(store.state(), StoreState::Locked);
            store.unlock_store(&password, None).unwrap();
            assert!(store.account_id("protected-sentinel").unwrap().is_some());
            store
        })
        .await
        .unwrap();
        let broker = Arc::new(Broker::new(Some(store.clone())));
        let lease = desktop(&broker).await;
        let check = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .request_refresh("alice".into(), "paired-peer".into())
                    .await
            }
        });
        let command = claim_refresh(&broker, &lease).await;
        broker
            .complete_refresh(RefreshCompleteRequest {
                expected_account: "alice".into(),
                lease: lease.id.clone(),
                id: command.id,
                result: Ok(RemoteUpdateStatus {
                    state: UpdateState::Ready,
                    offered: Some(target()),
                    ..Default::default()
                }),
            })
            .await
            .unwrap();
        assert_eq!(
            check.await.unwrap().unwrap().blocked,
            Some(BlockedReason::PasswordProtected)
        );
        receipt(&broker, &lease, OperationState::Queued).await;
        assert_eq!(store.state(), StoreState::Open);
        assert!(
            plain_store(Some(&store))
                .unwrap_err()
                .contains("even while unlocked")
        );
        assert!(broker.take(take_request(&lease)).await.unwrap().is_none());
        let status = broker.status("alice", "paired-peer").await.unwrap();
        assert_eq!(status.blocked, Some(BlockedReason::PasswordProtected));
        assert_eq!(status.offered, Some(target()));
        let refused = broker
            .authorize_stop(*Uuid::new_v4().as_bytes(), Some("alice".into()))
            .await
            .unwrap_err();
        assert!(refused.contains("even while unlocked"));
        assert_eq!(store.state(), StoreState::Open);
        store.close().unwrap();
        assert!(plain_store(Some(&store)).is_err());
    }

    #[tokio::test]
    async fn status_uses_durable_sequence_and_receipts_survive_desktop_replacement() {
        let (_directory, _store, broker) = fixture();
        let lease = desktop(&broker).await;
        let first = receipt(&broker, &lease, OperationState::Failed).await;
        let second = receipt(&broker, &lease, OperationState::Failed).await;
        assert_ne!(first, second);
        assert_eq!(
            broker
                .status("alice", "paired-peer")
                .await
                .unwrap()
                .operation
                .unwrap()
                .operation_id,
            second
        );
        broker.state.lock().unwrap().lease = None;
        let status = broker.status("alice", "paired-peer").await.unwrap();
        assert_eq!(status.blocked, Some(BlockedReason::DesktopUnavailable));
        assert_eq!(status.operation.unwrap().operation_id, second);
    }

    #[tokio::test]
    async fn journal_retains_only_domain_separated_scope_bindings() {
        let (_directory, store, broker) = fixture();
        let lease = desktop(&broker).await;
        receipt(&broker, &lease, OperationState::Queued).await;
        let path = broker.directory.as_ref().unwrap().join("receipts.json");
        let bytes = std::fs::read(&path).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let store_id = store.lock_status().unwrap().store_id.unwrap();
        assert!(!text.contains("alice"));
        assert!(!text.contains("paired-peer"));
        assert!(!text.contains(&store_id));
        assert_ne!(
            scope_binding("account", "same-value"),
            scope_binding("peer", "same-value")
        );
        assert_ne!(
            scope_binding("account", "same-value"),
            scope_binding("store", "same-value")
        );
        assert!(
            broker
                .status("alice", "paired-peer")
                .await
                .unwrap()
                .operation
                .is_some()
        );
        assert!(
            broker
                .status("bob", "paired-peer")
                .await
                .unwrap()
                .operation
                .is_none()
        );
        assert!(
            broker
                .status("alice", "another-peer")
                .await
                .unwrap()
                .operation
                .is_none()
        );

        // This is an unreleased journal format: a raw or malformed persisted
        // scope cannot be interpreted as a matching account or replayed.
        let mut invalid: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let receipt = invalid
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap();
        receipt["account_binding"] = serde_json::json!("alice");
        std::fs::write(path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(
            Broker::new(Some(store))
                .status("alice", "paired-peer")
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn linked_or_public_journal_files_fail_closed() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (_directory, store, broker) = fixture();
        let lease = desktop(&broker).await;
        receipt(&broker, &lease, OperationState::Queued).await;
        let path = broker.directory.as_ref().unwrap().join("receipts.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            Broker::new(Some(store.clone()))
                .status("alice", "paired-peer")
                .await
                .is_err()
        );
        std::fs::remove_file(&path).unwrap();
        symlink("missing-target", &path).unwrap();
        assert!(
            Broker::new(Some(store))
                .status("alice", "paired-peer")
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn updates_ancestor_symlink_never_creates_receipts_outside_the_profile() {
        use std::os::unix::fs::symlink;
        let (_directory, store, broker) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let updates = broker.directory.as_ref().unwrap().parent().unwrap();
        symlink(outside.path(), updates).unwrap();
        assert!(broker.attach("alice".into()).await.is_err());
        assert!(
            Broker::new(Some(store))
                .status("alice", "paired-peer")
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn public_updates_ancestor_is_refused_without_creating_a_receipt_child() {
        use std::os::unix::fs::PermissionsExt;
        let (_directory, _store, broker) = fixture();
        let updates = broker.directory.as_ref().unwrap().parent().unwrap();
        std::fs::create_dir(updates).unwrap();
        std::fs::set_permissions(updates, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(broker.attach("alice".into()).await.is_err());
        assert!(!updates.join("remote-installs").exists());
        assert_eq!(std::fs::read_dir(updates).unwrap().count(), 0);
    }
}
