//! Agent-only update orchestration. All requests and filesystem operations run
//! on the backend runtime; views receive precomputed, truthful snapshots.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use maple_agent_updater::{
    CheckOutcome, Config, PreparedUpdate, Revalidation, Target, Updater, VerifiedRelease,
};
use tokio::sync::{Mutex, OnceCell, watch};
use tokio_util::sync::CancellationToken;

gpui::actions!(maple_updates, [AboutAgent, CheckForUpdates]);

pub struct UpdateRequested(pub UpdateAction);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Disabled,
    Idle,
    Checking,
    Current,
    Available,
    Downloading,
    Ready,
    Installing,
    RestartRequired,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateAction {
    Check,
    Download,
    ManualDownload,
    Restart,
    Relaunch,
    Later,
    AutomaticDownloads(bool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub state: State,
    pub label: String,
    pub version: Option<String>,
    /// Exact verified platform document, never reconstructed from a version.
    pub remote_candidate: Option<crate::remote::update::ReleaseIdentity>,
    pub primary: Option<UpdateAction>,
    pub primary_label: &'static str,
    pub manual_download_url: Option<String>,
    pub show_banner: bool,
}

impl Snapshot {
    fn new(state: State, label: impl Into<String>) -> Self {
        Self {
            state,
            label: label.into(),
            version: None,
            remote_candidate: None,
            primary: None,
            primary_label: "",
            manual_download_url: None,
            show_banner: false,
        }
    }
    pub fn initial() -> Self {
        match disabled_reason(
            crate::profile::PROFILE,
            crate::profile::UPDATE_PUBLIC_KEYS,
            crate::env::env_flag("MAPLE_DISABLE_UPDATE_CHECK"),
        ) {
            Some(reason) => Self::new(State::Disabled, reason),
            None => Self::new(State::Idle, "Not checked yet"),
        }
    }
    pub fn busy(&self) -> bool {
        matches!(
            self.state,
            State::Checking | State::Downloading | State::Installing
        )
    }
}

fn disabled_reason(profile: &str, keys: &[&str], hard_off: bool) -> Option<&'static str> {
    if hard_off {
        Some("Update checks disabled")
    } else if profile != "prod" {
        Some("Updates are available in packaged Prod builds")
    } else if keys.is_empty() {
        Some("Updates not configured")
    } else {
        None
    }
}

struct Service {
    updater: Arc<Updater>,
    installed_path: PathBuf,
    manual_only: AtomicBool,
}
impl Service {
    fn manual_only(&self) -> bool {
        self.manual_only.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug)]
struct InstallationFailure {
    message: String,
    manual: bool,
    recover_host: bool,
    // Only a known pre-stop refusal may retain the exact verified staging.
    retry_ready: bool,
}
impl From<maple_agent_updater::installer::InstallError> for InstallationFailure {
    fn from(error: maple_agent_updater::installer::InstallError) -> Self {
        Self {
            manual: error.requires_manual_installation(),
            message: error.to_string(),
            recover_host: false,
            retry_ready: false,
        }
    }
}
impl From<String> for InstallationFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            manual: false,
            recover_host: false,
            retry_ready: false,
        }
    }
}
impl From<&str> for InstallationFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
#[derive(Default)]
struct ReleaseState {
    release: Option<VerifiedRelease>,
    prepared: Option<PreparedUpdate>,
    installation: Option<maple_agent_updater::installer::InstallPreparation>,
    install_lock: Option<maple_agent_updater::installer::InstallLock>,
    deferred: Option<String>,
}
struct Inner {
    service: OnceCell<Service>,
    operation: Mutex<()>,
    releases: Mutex<ReleaseState>,
    snapshots: watch::Sender<Snapshot>,
    automatic: AtomicBool,
    started: AtomicBool,
    cancel: CancellationToken,
    installed: OnceLock<CommittedInstallation>,
}
struct CommittedInstallation {
    path: PathBuf,
    // The GPUI helper waits for this PID to exit. Keep old-client admission
    // fenced until OS exit closes the files, then the new process can start.
    _host_fence: Option<std::mem::ManuallyDrop<crate::host::HostUpdateFence>>,
}

/// One controller per desktop process. The operation mutex covers each complete
/// check/download/install; repeated clicks cannot launch competing transactions.
#[derive(Clone)]
pub struct Controller(Arc<Inner>);

impl Controller {
    pub fn new(automatic_downloads: bool) -> Self {
        let (snapshots, _) = watch::channel(Snapshot::initial());
        Self(Arc::new(Inner {
            service: OnceCell::new(),
            operation: Mutex::new(()),
            releases: Mutex::new(ReleaseState::default()),
            snapshots,
            automatic: AtomicBool::new(automatic_downloads),
            started: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            installed: OnceLock::new(),
        }))
    }
    pub fn snapshot(&self) -> Snapshot {
        self.0.snapshots.borrow().clone()
    }
    pub(crate) fn remote_status(&self) -> crate::remote::update::RemoteUpdateStatus {
        use crate::remote::update::{BlockedReason, RemoteUpdateStatus, UpdateState};
        let snapshot = self.snapshot();
        let state = match snapshot.state {
            State::Disabled => UpdateState::Disabled,
            State::Idle => UpdateState::Idle,
            State::Checking => UpdateState::Checking,
            State::Current => UpdateState::Current,
            State::Available => UpdateState::Available,
            State::Downloading => UpdateState::Downloading,
            State::Ready => UpdateState::Ready,
            State::Installing => UpdateState::Installing,
            State::RestartRequired => UpdateState::RestartRequired,
            State::Error => UpdateState::Error,
        };
        let blocked = if snapshot.primary == Some(UpdateAction::ManualDownload) {
            Some(BlockedReason::UnsupportedInstallation)
        } else {
            match snapshot.state {
                State::Ready | State::Installing | State::Current | State::Idle => None,
                State::Disabled => Some(BlockedReason::UpdatesDisabled),
                State::Checking | State::Downloading => Some(BlockedReason::Busy),
                _ => Some(BlockedReason::UpdateNotReady),
            }
        };
        RemoteUpdateStatus {
            state,
            offered: snapshot.remote_candidate,
            blocked,
            operation: None,
        }
    }
    #[cfg(test)]
    pub(crate) fn startup_policy_for_test(&self) -> (bool, bool) {
        (
            self.0.started.load(Ordering::Acquire),
            self.0.automatic.load(Ordering::Acquire),
        )
    }
    #[cfg(test)]
    pub(crate) fn set_test_state(&self, state: State) {
        self.publish(Snapshot::new(state, "Test update state"));
    }
    #[cfg(test)]
    pub(crate) fn set_test_installed(&self, path: PathBuf) {
        self.installed(path, "0.0.2", None);
    }
    #[cfg(test)]
    pub(crate) fn set_test_installed_with_fence(
        &self,
        path: PathBuf,
        fence: crate::host::HostUpdateFence,
    ) {
        self.installed(path, "0.0.2", Some(fence));
    }
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.0.snapshots.subscribe()
    }
    pub fn shutdown(&self) {
        self.0.cancel.cancel();
    }
    /// Once replacement commits, this process can only finish its restart.
    /// GPUI's restart API cannot report helper-spawn failure, so retain an
    /// accurate retryable state even when the old window remains open.
    pub fn restart_path(&self) -> Option<PathBuf> {
        self.0
            .installed
            .get()
            .map(|installed| installed.path.clone())
    }
    fn installed(
        &self,
        path: PathBuf,
        version: &str,
        host_fence: Option<crate::host::HostUpdateFence>,
    ) {
        // Exactly one committed update can occur in this process. Deliberately
        // leave its two lock descriptors open until OS exit, even if GPUI drops
        // its final controller Arc during teardown before the PID disappears.
        // Pending old-client startup must remain fenced during that interval.
        if let Err(rejected) = self.0.installed.set(CommittedInstallation {
            path,
            _host_fence: host_fence.map(std::mem::ManuallyDrop::new),
        }) {
            if let Some(fence) = rejected._host_fence {
                drop(std::mem::ManuallyDrop::into_inner(fence));
            }
            return;
        }
        self.0.cancel.cancel();
        let mut snapshot = Snapshot::new(
            State::RestartRequired,
            format!("v{version} is installed. Restart Maple to finish, or quit and reopen it."),
        );
        snapshot.version = Some(version.to_owned());
        snapshot.primary = Some(UpdateAction::Relaunch);
        snapshot.primary_label = "Restart Maple";
        snapshot.show_banner = true;
        self.publish(snapshot);
    }
    /// Start best-effort cleanup before GPUI drops the windows and their backend
    /// owners. GPUI polls its quit future without a Tokio reactor, so both the
    /// lock timeout and blocking filesystem work must run on the backend runtime.
    pub fn cleanup_on_exit(
        &self,
        backend: Arc<crate::backend::AgentBackend>,
    ) -> tokio::task::JoinHandle<()> {
        let controller = self.clone();
        let cleanup_backend = backend.clone();
        backend.spawn(async move {
            // A Handle does not keep an owned runtime alive. Retain its owner
            // even if GPUI's 200 ms shutdown budget stops awaiting this task.
            let _backend = cleanup_backend;
            controller.cleanup_staging_on_exit().await;
        })
    }
    /// Keep the authenticated downloaded payload for restoration, but remove
    /// only the derived uncommitted same-volume staging bundle.
    async fn cleanup_staging_on_exit(&self) {
        let Ok(_operation) =
            tokio::time::timeout(Duration::from_secs(2), self.0.operation.lock()).await
        else {
            return;
        };
        let mut releases = self.0.releases.lock().await;
        let staging = releases.installation.take();
        let lock = releases.install_lock.take();
        drop(releases);
        if let Some(staging) = staging {
            discard_preparation_with_lock(staging, lock).await;
        }
    }
    fn publish(&self, snapshot: Snapshot) {
        if self.restart_path().is_some() && snapshot.state != State::RestartRequired {
            return;
        }
        self.0.snapshots.send_if_modified(|current| {
            if *current == snapshot {
                false
            } else {
                *current = snapshot;
                true
            }
        });
    }
    fn begin_check(&self) {
        let previous = self.snapshot();
        let mut checking = Snapshot::new(State::Checking, "Checking for updates…");
        checking.version = previous.version;
        checking.remote_candidate = previous.remote_candidate;
        checking.manual_download_url = previous.manual_download_url;
        self.publish(checking);
    }
    fn recheck_failed(&self, previous: Snapshot) {
        if previous.state == State::Ready {
            let mut ready = previous;
            ready.label = "The downloaded update is ready. Could not check for updates; restarting will verify it again before installation.".into();
            self.publish(ready);
        } else {
            self.fail("Could not check for updates. Your current app is still available.");
        }
    }
    fn fail(&self, message: impl Into<String>) {
        if self.restart_path().is_some() {
            return;
        }
        let previous = self.snapshot();
        let mut snapshot = Snapshot::new(State::Error, message);
        snapshot.version = previous.version;
        snapshot.remote_candidate = previous.remote_candidate;
        snapshot.manual_download_url = previous.manual_download_url;
        snapshot.show_banner = snapshot.version.is_some();
        snapshot.primary = Some(UpdateAction::Check);
        snapshot.primary_label = "Retry";
        self.publish(snapshot);
    }
    async fn service(&self) -> Result<&Service, String> {
        self.0
            .service
            .get_or_try_init(|| async {
                if self.snapshot().state == State::Disabled {
                    return Err(self.snapshot().label);
                }
                tokio::task::spawn_blocking(build_service)
                    .await
                    .map_err(|_| "Could not initialize updates".to_string())?
            })
            .await
    }
    /// Owned by the desktop lifetime. Cancellation interrupts polling and HTTP
    /// work; a blocking atomic replacement, once started, finishes deliberately.
    pub async fn run(self) {
        if self.0.started.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.snapshot().state == State::Disabled || self.0.cancel.is_cancelled() {
            return;
        }
        let startup = async {
            if let Ok(service) = self.service().await {
                cleanup_installed_update(service).await;
                match service.updater.restore_prepared().await {
                    Ok(Some(prepared)) => {
                        let mut releases = self.0.releases.lock().await;
                        releases.release = Some(prepared.release.clone());
                        releases.prepared = Some(prepared);
                    }
                    Ok(None) => {}
                    Err(_) => self.fail("Saved update could not be verified; checking again"),
                }
            }
            self.check().await;
        };
        tokio::select! { _ = self.0.cancel.cancelled() => return, _ = startup => {} }
        loop {
            if self.restart_path().is_some() {
                return;
            }
            let delay = Duration::from_secs(3300 + rand::random::<u64>() % 601);
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {},
            }
            tokio::select! { _ = self.0.cancel.cancelled() => return, _ = self.check() => {} }
        }
    }
    pub async fn check(&self) {
        if self.0.cancel.is_cancelled() || self.restart_path().is_some() {
            return;
        }
        let Ok(_operation) = self.0.operation.try_lock() else {
            return;
        };
        self.check_inner(true).await;
    }
    /// A remote check must actually acquire the updater and read its signed
    /// feed. A busy operation or failed recheck cannot turn cached Ready into
    /// a successful fresh response. Downloads resume separately after the reply.
    pub(crate) async fn refresh_remote(
        &self,
    ) -> Result<crate::remote::update::RemoteUpdateStatus, crate::remote_updates::RefreshFailure>
    {
        self.refresh_remote_with(|| self.check_inner(false)).await
    }

    async fn refresh_remote_with<C, F>(
        &self,
        check: C,
    ) -> Result<crate::remote::update::RemoteUpdateStatus, crate::remote_updates::RefreshFailure>
    where
        C: FnOnce() -> F,
        F: std::future::Future<Output = bool>,
    {
        use crate::remote_updates::RefreshFailure;
        if self.0.cancel.is_cancelled() || self.restart_path().is_some() {
            return Err(RefreshFailure::Unavailable);
        }
        if self.snapshot().state == State::Disabled {
            return Err(RefreshFailure::Disabled);
        }
        let Ok(_operation) = self.0.operation.try_lock() else {
            return Err(RefreshFailure::Busy);
        };
        // The broker bounds the remote caller's wait. Keep this operation
        // owned until it finishes: a caller timeout must not abandon a blocking
        // package validation and let a second updater operation overlap it.
        tokio::select! {
            _ = self.0.cancel.cancelled() => Err(RefreshFailure::Unavailable),
            checked = check() => match checked {
                true => Ok(self.remote_status()),
                false => Err(RefreshFailure::CheckFailed),
            },
        }
    }

    async fn check_inner(&self, automatic_download: bool) -> bool {
        if self.snapshot().state == State::Disabled {
            return false;
        }
        let previous = self.snapshot();
        self.begin_check();
        let service = match self.service().await {
            Ok(service) => service,
            Err(error) => {
                self.fail(error);
                return false;
            }
        };
        match service.updater.check().await {
            Ok(CheckOutcome::Current | CheckOutcome::Withdrawn) => {
                let mut releases = self.0.releases.lock().await;
                releases.release = None;
                releases.prepared = None;
                let staging = releases.installation.take();
                let lock = releases.install_lock.take();
                drop(releases);
                if let Some(staging) = staging {
                    discard_preparation_with_lock(staging, lock).await;
                }
                let _ = service.updater.clear_prepared().await;
                let mut snapshot = Snapshot::new(State::Current, "Up to date");
                snapshot.primary = Some(UpdateAction::Check);
                snapshot.primary_label = "Check for Updates";
                self.publish(snapshot);
                true
            }
            Ok(CheckOutcome::Available(release)) => {
                self.accept_release_with_policy(service, *release, automatic_download)
                    .await;
                true
            }
            Err(_) => {
                self.recheck_failed(previous);
                false
            }
        }
    }
    async fn accept_release(&self, service: &Service, release: VerifiedRelease) {
        self.accept_release_with_policy(service, release, true)
            .await;
    }
    async fn accept_release_with_policy(
        &self,
        service: &Service,
        release: VerifiedRelease,
        automatic_download: bool,
    ) {
        let (prepared, previous_staging, previous_lock) = {
            let mut releases = self.0.releases.lock().await;
            let prepared = releases
                .prepared
                .as_ref()
                .filter(|p| p.release.envelope == release.envelope)
                .cloned();
            let (previous_staging, previous_lock) = if prepared.is_none() {
                releases.prepared = None;
                (releases.installation.take(), releases.install_lock.take())
            } else {
                (None, None)
            };
            releases.release = Some(release.clone());
            (prepared, previous_staging, previous_lock)
        };
        if let Some(staging) = previous_staging {
            discard_preparation_with_lock(staging, previous_lock).await;
        }
        if let Some(prepared) = prepared.filter(|_| !service.manual_only()) {
            match self.stage_prepared(prepared).await {
                Ok(()) => self.release_snapshot(service, &release, State::Ready).await,
                Err(error) => self.installation_failed(service, &release, error).await,
            }
        } else if automatic_download
            && self.0.automatic.load(Ordering::Relaxed)
            && !service.manual_only()
        {
            self.download_inner(service, release).await;
        } else {
            self.release_snapshot(service, &release, State::Available)
                .await;
        }
    }
    async fn stage_prepared(&self, prepared: PreparedUpdate) -> Result<(), InstallationFailure> {
        // A restored Ready flag is never trusted. Extract/check the signed
        // package before offering Restart, then check it again at installation.
        let mut releases = self.0.releases.lock().await;
        let previous = releases.installation.take();
        let previous_lock = releases.install_lock.take();
        drop(releases);
        let (installation, lock) = tokio::task::spawn_blocking(move || {
            let lock = match previous_lock {
                Some(lock) => lock,
                None => {
                    maple_agent_updater::installer::acquire_install_lock(&prepared.installed_path)
                        .map_err(InstallationFailure::from)?
                }
            };
            if let Some(previous) = previous {
                match maple_agent_updater::installer::revalidate(&previous, &prepared) {
                    Ok(()) => return Ok((previous, lock)),
                    Err(error) => {
                        let _ = maple_agent_updater::installer::discard_preparation(&previous);
                        return Err(error.into());
                    }
                }
            }
            let installation = maple_agent_updater::installer::prepare(&prepared)
                .map_err(InstallationFailure::from)?;
            Ok::<_, InstallationFailure>((installation, lock))
        })
        .await
        .map_err(|_| "Could not prepare the update".to_string())??;
        let mut releases = self.0.releases.lock().await;
        releases.installation = Some(installation);
        releases.install_lock = Some(lock);
        Ok(())
    }
    async fn installation_failed(
        &self,
        service: &Service,
        release: &VerifiedRelease,
        error: InstallationFailure,
    ) {
        if error.manual {
            service.manual_only.store(true, Ordering::Release);
            self.release_snapshot(service, release, State::Available)
                .await;
            let mut snapshot = self.snapshot();
            snapshot.label = format!(
                "{} Download v{} to install manually.",
                error.message, release.manifest.version
            );
            self.publish(snapshot);
        } else {
            self.fail(error.message);
        }
    }
    async fn release_snapshot(&self, service: &Service, release: &VerifiedRelease, state: State) {
        let version = release.manifest.version.clone();
        let mut snapshot = Snapshot::new(
            state,
            match state {
                State::Ready => format!("v{version} is ready to install"),
                State::Downloading => format!("Downloading v{version}…"),
                _ if service.manual_only() => {
                    format!("v{version} is available for manual installation")
                }
                _ => format!("v{version} is available"),
            },
        );
        snapshot.version = Some(version.clone());
        snapshot.remote_candidate = Some(release_identity(release));
        snapshot.manual_download_url = manual_download_url(release);
        if state == State::Ready {
            snapshot.primary = Some(UpdateAction::Restart);
            snapshot.primary_label = "Restart to Update";
        } else if state == State::Available {
            snapshot.primary = Some(if service.manual_only() {
                UpdateAction::ManualDownload
            } else {
                UpdateAction::Download
            });
            snapshot.primary_label = "Download";
        }
        snapshot.show_banner = state != State::Downloading
            && self.0.releases.lock().await.deferred.as_ref() != Some(&version);
        self.publish(snapshot);
    }
    pub async fn download(&self) {
        if self.0.cancel.is_cancelled() || self.restart_path().is_some() {
            return;
        }
        let Ok(_operation) = self.0.operation.try_lock() else {
            return;
        };
        let service = match self.service().await {
            Ok(service) => service,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        let release = self.0.releases.lock().await.release.clone();
        if let Some(release) = release {
            self.download_inner(service, release).await;
        }
    }
    async fn download_inner(&self, service: &Service, release: VerifiedRelease) {
        if service.manual_only() {
            self.release_snapshot(service, &release, State::Available)
                .await;
            return;
        }
        self.release_snapshot(service, &release, State::Downloading)
            .await;
        match service.updater.download(&release).await {
            Ok(prepared) => {
                self.0.releases.lock().await.prepared = Some(prepared.clone());
                match self.stage_prepared(prepared).await {
                    Ok(()) => self.release_snapshot(service, &release, State::Ready).await,
                    Err(error) => self.installation_failed(service, &release, error).await,
                }
            }
            Err(_) => {
                self.fail("Could not download or verify the update. Retry when you are ready.")
            }
        }
    }
    /// Apply on the UI thread accepting the current settings generation. A
    /// delayed runtime task must never overwrite a newer saved opt-out.
    pub fn set_auto_download(&self, enabled: bool) {
        self.0.automatic.store(enabled, Ordering::Relaxed);
    }
    pub async fn download_if_automatic(&self) {
        if self.0.automatic.load(Ordering::Relaxed) && self.snapshot().state == State::Available {
            self.download().await;
        }
    }
    pub async fn later(&self) {
        self.0.releases.lock().await.deferred = self.snapshot().version;
        let mut snapshot = self.snapshot();
        snapshot.show_banner = false;
        self.publish(snapshot);
    }
    /// Called only after the desktop's explicit task-stop confirmation. Returns
    /// the verified installed path for GPUI's deliberate restart handoff.
    pub async fn install(
        &self,
        backend: Arc<crate::backend::AgentBackend>,
    ) -> Result<Option<PathBuf>, String> {
        self.install_requested(backend, None).await
    }

    pub(crate) async fn install_remote(
        &self,
        backend: Arc<crate::backend::AgentBackend>,
        target: crate::remote::update::ReleaseIdentity,
        operation_id: String,
    ) -> Result<Option<PathBuf>, String> {
        target.validate()?;
        let operation = uuid::Uuid::parse_str(&operation_id)
            .map_err(|_| "Invalid remote update operation ID")?;
        if operation.to_string() != operation_id {
            return Err("Invalid remote update operation ID".into());
        }
        self.install_requested(backend, Some((target, *operation.as_bytes())))
            .await
    }

    async fn install_requested(
        &self,
        backend: Arc<crate::backend::AgentBackend>,
        remote: Option<(crate::remote::update::ReleaseIdentity, [u8; 16])>,
    ) -> Result<Option<PathBuf>, String> {
        if let Some(path) = self.restart_path() {
            return Ok(Some(path));
        }
        if self.0.cancel.is_cancelled() {
            return Ok(None);
        }
        let Ok(_operation) = self.0.operation.try_lock() else {
            return Ok(None);
        };
        let previous = self.snapshot();
        let expected = remote.as_ref().map(|(target, _)| target.clone());
        let result = self.install_inner(backend, remote).await;
        if let Some(path) = self.restart_path() {
            return Ok(Some(path));
        }
        if let Err(error) = &result {
            self.failed_install(previous, expected.as_ref(), error)
                .await;
        }
        result.map_err(|error| error.message)
    }

    async fn failed_install(
        &self,
        previous: Snapshot,
        remote: Option<&crate::remote::update::ReleaseIdentity>,
        error: &InstallationFailure,
    ) {
        if error.retry_ready
            && !error.recover_host
            && !error.manual
            && previous.state == State::Ready
            && let Some(expected) = remote
            && previous.remote_candidate.as_ref() == Some(expected)
        {
            let releases = self.0.releases.lock().await;
            let retained = releases
                .prepared
                .as_ref()
                .is_some_and(|prepared| release_identity(&prepared.release) == *expected)
                && releases.installation.is_some()
                && releases.install_lock.is_some();
            drop(releases);
            if retained {
                // The operation gate is still held. Staging was revalidated
                // before the refusal, and every future attempt checks it again.
                // Report this attempt's failure through its receipt, not a
                // misleading local installation error or restart instruction.
                self.publish(previous);
                return;
            }
        }
        if let Some(service) = self.0.service.get() {
            let release = self.0.releases.lock().await.release.clone();
            if let Some(release) = release {
                self.installation_failed(service, &release, error.clone())
                    .await;
                return;
            }
        }
        self.fail(error.message.clone());
    }
    async fn install_inner(
        &self,
        backend: Arc<crate::backend::AgentBackend>,
        remote: Option<(crate::remote::update::ReleaseIdentity, [u8; 16])>,
    ) -> Result<Option<PathBuf>, InstallationFailure> {
        let api_url = backend.api_url().to_owned();
        let service = self.service().await?;
        let prepared = self
            .0
            .releases
            .lock()
            .await
            .prepared
            .clone()
            .ok_or("No verified update is ready")?;
        if remote
            .as_ref()
            .is_some_and(|(expected, _)| *expected != release_identity(&prepared.release))
        {
            return Err("That exact remote update is no longer ready".into());
        }
        let mut installing = Snapshot::new(State::Installing, "Preparing to restart and update…");
        installing.version = Some(prepared.release.manifest.version.clone());
        installing.remote_candidate = Some(release_identity(&prepared.release));
        installing.manual_download_url = manual_download_url(&prepared.release);
        self.publish(installing);
        let installed_path = service.installed_path.clone();
        let staging = prepared.clone();
        let mut releases = self.0.releases.lock().await;
        let saved = releases.installation.take();
        let saved_lock = releases.install_lock.take();
        drop(releases);
        let (lock, preparation) = tokio::task::spawn_blocking(move || {
            let lock = match saved_lock {
                Some(lock) => lock,
                None => maple_agent_updater::installer::acquire_install_lock(&installed_path)
                    .map_err(InstallationFailure::from)?,
            };
            let preparation = match saved {
                Some(preparation) => {
                    if let Err(error) =
                        maple_agent_updater::installer::revalidate(&preparation, &staging)
                    {
                        let _ = maple_agent_updater::installer::discard_preparation(&preparation);
                        return Err(error.into());
                    }
                    preparation
                }
                None => maple_agent_updater::installer::prepare(&staging)
                    .map_err(InstallationFailure::from)?,
            };
            Ok::<_, InstallationFailure>((lock, preparation))
        })
        .await
        .map_err(|_| "Could not prepare installation".to_string())??;
        let fresh = match service.updater.revalidate(&prepared).await {
            Ok(fresh) => fresh,
            Err(_) => {
                if remote.is_some() {
                    let mut releases = self.0.releases.lock().await;
                    releases.installation = Some(preparation);
                    releases.install_lock = Some(lock);
                    return Err(InstallationFailure {
                        message: "Could not revalidate the update. Maple is still running.".into(),
                        manual: false,
                        recover_host: false,
                        retry_ready: true,
                    });
                }
                discard_preparation(preparation).await;
                return Err("Could not revalidate the update. Maple is still running.".into());
            }
        };
        match fresh {
            Revalidation::Ready(_) => {}
            Revalidation::Superseded(release) => {
                discard_preparation(preparation).await;
                drop(lock);
                self.0.releases.lock().await.prepared = None;
                service
                    .updater
                    .clear_prepared()
                    .await
                    .map_err(|e| e.to_string())?;
                self.accept_release(service, release).await;
                return Ok(None);
            }
            Revalidation::Withdrawn => {
                discard_preparation(preparation).await;
                drop(lock);
                self.0.releases.lock().await.prepared = None;
                self.0.releases.lock().await.release = None;
                service
                    .updater
                    .clear_prepared()
                    .await
                    .map_err(|e| e.to_string())?;
                self.publish(Snapshot::new(
                    State::Current,
                    "This update is no longer offered. Check again later.",
                ));
                return Ok(None);
            }
        }
        // Commit every pending frontend patch before asking the host to drain.
        // Failed saves leave the live store and installation untouched.
        if let Err(error) = crate::desktop::flush_frontend_state(&backend).await {
            discard_preparation(preparation).await;
            return Err(error.into());
        }
        let recovery_api = api_url.clone();
        let installed_version = prepared.release.manifest.version.clone();
        let controller = self.clone();
        let updater = service.updater.clone();
        let result = tokio::task::spawn_blocking(move || {
            // Refresh effective permissions after the asynchronous discovery
            // check, before stopping a healthy host or closing admission.
            if let Err(error) = maple_agent_updater::installer::preflight_installation(&prepared.installed_path) {
                let _ = maple_agent_updater::installer::discard_preparation(&preparation);
                return Err(InstallationFailure::from(error));
            }
            let manual_failure = std::cell::Cell::new(false);
            let fence = match remote {
                Some((_, operation)) => crate::host::acquire_remote_update_fence(&api_url, operation)
                    .map_err(|error| InstallationFailure {
                        message: error.message,
                        manual: false,
                        recover_host: !error.host_unchanged,
                        retry_ready: error.host_unchanged,
                    }),
                None => crate::host::acquire_update_fence(&api_url)
                    .map_err(|message| InstallationFailure { message, manual: false, recover_host: true, retry_ready: false }),
            };
            let mut host_fence = match fence {
                Ok(fence) => Some(fence),
                Err(error) => {
                    if error.retry_ready {
                        let mut releases = controller.0.releases.blocking_lock();
                        releases.installation = Some(preparation);
                        releases.install_lock = Some(lock);
                    } else {
                        let _ = maple_agent_updater::installer::discard_preparation(&preparation);
                    }
                    return Err(error);
                }
            };
            let _lock = lock;
            let result = updater.with_commit_guard(&prepared, || {
                // replace performs the final payload/tree/codesign checks
                // while both process fences and the commit guard are held.
                let installed = maple_agent_updater::installer::replace(&preparation, &prepared).map_err(|error| {
                    manual_failure.set(error.requires_manual_installation());
                    error.to_string()
                })?;
                if installed.durability_warning {
                    log::warn!("The update was installed; filesystem synchronization did not complete. Restarting the installed application.");
                }
                // Retire old-process RPC admission and retain host ownership
                // until this PID exits, including if the restart helper fails.
                let restart_path = installed.restart_path();
                backend.retire_after_update();
                controller.installed(restart_path.clone(), &installed_version, host_fence.take());
                Ok(restart_path)
            }).map_err(|error| error.to_string());
            drop(host_fence);
            if result.is_err() {
                let _ = maple_agent_updater::installer::discard_preparation(&preparation);
            }
            result.map_err(|message| InstallationFailure { message, manual: manual_failure.get(), recover_host: true, retry_ready: false })
        }).await.map_err(|_| "Installation did not complete; retry or reinstall Maple".to_string())?;
        match result {
            Ok(path) => Ok(Some(path)),
            Err(error) => {
                if !error.recover_host {
                    return Err(error);
                }
                // An Err is guaranteed to precede the atomic swap. The host
                // fence and application lock have both dropped before recovery.
                if crate::host::recover_after_failed_update(&recovery_api)
                    .await
                    .is_err()
                {
                    return Err(InstallationFailure {
                        message: format!(
                            "{}. Quit and reopen Maple to restore its host.",
                            error.message.trim_end_matches('.')
                        ),
                        ..error
                    });
                }
                Err(InstallationFailure {
                    message: format!(
                        "{}. Quit and reopen Maple before continuing work.",
                        error.message.trim_end_matches('.')
                    ),
                    ..error
                })
            }
        }
    }
}

fn release_identity(release: &VerifiedRelease) -> crate::remote::update::ReleaseIdentity {
    use sha2::{Digest, Sha256};
    crate::remote::update::ReleaseIdentity {
        version: release.manifest.version.clone(),
        build: release.manifest.build.clone(),
        source_sha: release.manifest.source_sha.clone(),
        manifest_sha256: format!("{:x}", Sha256::digest(release.envelope.as_bytes())),
    }
}

async fn discard_preparation(preparation: maple_agent_updater::installer::InstallPreparation) {
    discard_preparation_with_lock(preparation, None).await;
}

async fn cleanup_installed_update(service: &Service) {
    let installed = service.installed_path.clone();
    #[cfg(target_os = "linux")]
    {
        if service.manual_only() {
            return;
        }
        let Ok(origin) = compiled_feed_url(Target::LinuxX86_64) else {
            return;
        };
        let keys = crate::profile::UPDATE_PUBLIC_KEYS
            .iter()
            .map(|key| (*key).to_owned())
            .collect::<Vec<_>>();
        // Equal-version prepared downloads are deliberately rejected during
        // restore. Authenticate the install receipt against this running build
        // and the compiled feed/key pins before removing its retained backup.
        let _ = tokio::task::spawn_blocking(move || {
            maple_agent_updater::installer::cleanup_current_linux_bundle(
                &installed,
                env!("CARGO_PKG_VERSION"),
                env!("MAPLE_GIT_SOURCE_SHA"),
                &origin,
                &keys,
            )
        })
        .await;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = tokio::task::spawn_blocking(move || {
            maple_agent_updater::installer::cleanup_current_installation(
                &installed,
                env!("CARGO_PKG_VERSION"),
            )
        })
        .await;
    }
}

async fn discard_preparation_with_lock(
    preparation: maple_agent_updater::installer::InstallPreparation,
    lock: Option<maple_agent_updater::installer::InstallLock>,
) {
    let _ = tokio::task::spawn_blocking(move || {
        let _lock = lock;
        maple_agent_updater::installer::discard_preparation(&preparation)
    })
    .await;
}

fn manual_download_url(release: &VerifiedRelease) -> Option<String> {
    let target = if cfg!(target_os = "linux") {
        Target::LinuxX86_64
    } else {
        Target::DarwinAarch64
    };
    manual_download_url_for_target(release, target)
}

fn manual_download_url_for_target(release: &VerifiedRelease, target: Target) -> Option<String> {
    let artifact = match target {
        Target::DarwinAarch64 => release.manifest.downloads.macos.as_ref()?,
        Target::LinuxX86_64 => release.manifest.downloads.linux_bundle.as_ref()?,
    };
    release
        .origin
        .join(&artifact.path)
        .ok()
        .map(|url| url.to_string())
}

fn build_service() -> Result<Service, String> {
    let Installation {
        installed_path,
        manual_only,
    } = installation()?;
    let target = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Target::DarwinAarch64
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Target::LinuxX86_64
    } else {
        return Err("Updates are not supported on this platform".into());
    };
    let config = Config {
        feed_url: compiled_feed_url(target)?,
        public_keys: crate::profile::UPDATE_PUBLIC_KEYS
            .iter()
            .map(|key| (*key).to_owned())
            .collect(),
        current_version: env!("CARGO_PKG_VERSION")
            .parse()
            .map_err(|_| "Invalid app version")?,
        target,
        installed_path: installed_path.clone(),
        state_dir: update_state_dir(),
    };
    Ok(Service {
        updater: Arc::new(
            Updater::new(config).map_err(|_| "Invalid packaged update configuration")?,
        ),
        installed_path,
        manual_only: AtomicBool::new(manual_only),
    })
}

fn compiled_feed_url(target: Target) -> Result<maple_agent_updater::Url, String> {
    feed_url_for_target(
        crate::profile::UPDATE_FEED_URL.ok_or("Updates not configured")?,
        target,
    )
}

fn feed_url_for_target(feed: &str, target: Target) -> Result<maple_agent_updater::Url, String> {
    let feed = feed
        .parse::<maple_agent_updater::Url>()
        .map_err(|_| "Invalid packaged update configuration")?;
    // The profile and embedded build-info must describe the pointer we use.
    // Do not silently turn a shipped Mac v1 pointer into a Linux v2 pointer.
    if feed.path() != target.feed_path() {
        return Err("Invalid packaged update configuration".into());
    }
    Ok(feed)
}

fn update_state_dir() -> PathBuf {
    // Signed discovery/anti-rollback state survives local-store reset/repair.
    crate::backend::profile_data_root().join("updates")
}

struct Installation {
    installed_path: PathBuf,
    manual_only: bool,
}

impl Installation {
    #[cfg(any(not(target_os = "macos"), all(test, unix)))]
    fn manual(executable: &Path) -> Self {
        Self {
            installed_path: executable.to_owned(),
            manual_only: true,
        }
    }
}

fn installation() -> Result<Installation, String> {
    let exe = std::env::current_exe().map_err(|_| "Could not locate the installed app")?;
    #[cfg(target_os = "macos")]
    {
        let app = exe
            .ancestors()
            .nth(3)
            .filter(|path| path.extension().is_some_and(|e| e == "app"))
            .ok_or("Automatic updates require an installed Maple app")?;
        let app = std::fs::canonicalize(app).map_err(|_| "Could not locate the installed app")?;
        let manual = manual_macos_path(&app)
            || maple_agent_updater::installer::discover_installation(&exe, None).is_err();
        Ok(Installation {
            installed_path: app,
            manual_only: manual,
        })
    }
    #[cfg(target_os = "linux")]
    {
        Ok(linux_installation(
            &exe,
            maple_agent::self_launch::executable(),
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(Installation::manual(&exe))
    }
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn linux_installation(executable: &Path, resolved: Result<PathBuf, String>) -> Installation {
    // AppRun may execute through the bundled loader. Resolve that entrypoint
    // through the shared launcher, then let the installer validate its bundle
    // layout, compiled identity and replacement permissions.
    let Ok(path) = resolved else {
        return Installation::manual(executable);
    };
    match maple_agent_updater::installer::discover_linux_installation(
        &path,
        env!("CARGO_PKG_VERSION"),
        env!("MAPLE_GIT_SOURCE_SHA"),
    ) {
        Ok(installed_path) => Installation {
            installed_path,
            manual_only: false,
        },
        Err(_) => Installation::manual(executable),
    }
}
#[cfg(any(target_os = "macos", test))]
fn manual_macos_path(path: &Path) -> bool {
    path.starts_with("/Volumes")
        || path
            .components()
            .any(|component| component.as_os_str() == "AppTranslocation")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remote_refresh_invokes_a_new_check_even_for_cached_current_or_ready() {
        use std::sync::atomic::AtomicUsize;
        for cached in [State::Idle, State::Current, State::Ready, State::Error] {
            let controller = Controller::new(false);
            controller.set_test_state(cached);
            let calls = AtomicUsize::new(0);
            let result = controller
                .refresh_remote_with(|| async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    controller.publish(Snapshot::new(State::Available, "New signed offer"));
                    true
                })
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(result.state, crate::remote::update::UpdateState::Available);
        }
    }

    #[tokio::test]
    async fn remote_refresh_failure_never_reports_cached_ready_as_fresh() {
        let controller = Controller::new(false);
        controller.set_test_state(State::Ready);
        let result = controller.refresh_remote_with(|| async { false }).await;
        assert_eq!(
            result,
            Err(crate::remote_updates::RefreshFailure::CheckFailed)
        );
        assert_eq!(controller.snapshot().state, State::Ready);
        let operation = controller.0.operation.lock().await;
        assert_eq!(
            controller
                .refresh_remote_with(|| async { panic!("busy check must not run") })
                .await,
            Err(crate::remote_updates::RefreshFailure::Busy)
        );
        drop(operation);
        controller.set_test_state(State::Disabled);
        assert_eq!(
            controller
                .refresh_remote_with(|| async { panic!("disabled check must not run") })
                .await,
            Err(crate::remote_updates::RefreshFailure::Disabled)
        );
    }

    // Model the controller after preparation has passed, without replacing an
    // application or bypassing the installer's separate validation tests.
    async fn ready_remote_fixture(root: &Path) -> (Controller, Snapshot) {
        let root = root.canonicalize().unwrap();
        let installed = root.join("Maple Agent.app");
        std::fs::create_dir(&installed).unwrap();
        std::fs::write(installed.join("sentinel"), b"current application").unwrap();
        let feed = "https://updates.trymaple.ai/agent/stable/latest.json"
            .parse()
            .unwrap();
        let key =
            include_str!("../../crates/maple-agent-updater/tests/fixtures/public-test-key.pub")
                .to_owned();
        let release = maple_agent_updater::manifest::verify_envelope(
            include_bytes!("../../crates/maple-agent-updater/tests/fixtures/signed-envelope.json"),
            &feed,
            &[key],
        )
        .unwrap();
        let artifact = release.artifact(Target::DarwinAarch64).unwrap().clone();
        let staging = root.join(".maple-agent-update-abcdefghijklmnop");
        std::fs::create_dir(&staging).unwrap();
        let payload = staging.join("payload");
        let controller = Controller::new(true);
        {
            let mut releases = controller.0.releases.lock().await;
            releases.release = Some(release.clone());
            releases.prepared = Some(PreparedUpdate {
                release: release.clone(),
                payload_path: root.join("authenticated-download"),
                installed_path: installed.clone(),
            });
            releases.installation = Some(maple_agent_updater::installer::InstallPreparation {
                installed_path: installed.clone(),
                staging_dir: staging.clone(),
                payload_path: payload,
                staged_path: staging.join("Maple Agent.app"),
                version: release.manifest.version.clone(),
                build: release.manifest.build.clone(),
                size: artifact.size,
                sha256: artifact.sha256,
            });
            releases.install_lock =
                Some(maple_agent_updater::installer::acquire_install_lock(&installed).unwrap());
        }
        let mut ready = Snapshot::new(State::Ready, "v0.0.2 is ready to install");
        ready.version = Some(release.manifest.version.clone());
        ready.remote_candidate = Some(release_identity(&release));
        ready.primary = Some(UpdateAction::Restart);
        ready.primary_label = "Restart to Update";
        ready.show_banner = true;
        controller.publish(ready.clone());
        (controller, ready)
    }

    #[tokio::test]
    async fn remote_pre_stop_refusals_retain_ready_candidate_and_install_lock() {
        for message in [
            "This computer is busy with a local operation. Try again after it finishes.",
            "Remote access revoked",
            "Could not revalidate the update. Maple is still running.",
        ] {
            let root = tempfile::tempdir().unwrap();
            let (controller, ready) = ready_remote_fixture(root.path()).await;
            let target = ready.remote_candidate.clone().unwrap();
            controller.publish(Snapshot::new(State::Installing, "Preparing update"));
            controller
                .failed_install(
                    ready.clone(),
                    Some(&target),
                    &InstallationFailure {
                        message: message.into(),
                        manual: false,
                        recover_host: false,
                        retry_ready: true,
                    },
                )
                .await;
            assert_eq!(controller.snapshot(), ready);
            assert_eq!(
                controller.remote_status().state,
                crate::remote::update::UpdateState::Ready
            );
            assert!(controller.remote_status().blocked.is_none());
            assert_eq!(controller.remote_status().offered, Some(target));
            let installed = root.path().canonicalize().unwrap().join("Maple Agent.app");
            let contention = match maple_agent_updater::installer::acquire_install_lock(&installed)
            {
                Ok(_) => panic!("a refused remote update released its installation lock"),
                Err(error) => error,
            };
            assert!(
                matches!(
                    &contention,
                    maple_agent_updater::installer::InstallError::Busy
                ),
                "unexpected retained-lock result: {contention:?}"
            );
            assert_eq!(
                std::fs::read(root.path().join("Maple Agent.app/sentinel")).unwrap(),
                b"current application"
            );
            assert!(!controller.snapshot().label.contains("Quit and reopen"));
        }
    }

    #[tokio::test]
    async fn uncertain_shutdown_or_changed_candidate_never_restores_ready() {
        for scenario in [
            "uncertain",
            "different candidate",
            "missing staging",
            "local install",
        ] {
            let root = tempfile::tempdir().unwrap();
            let (controller, ready) = ready_remote_fixture(root.path()).await;
            let target = ready.remote_candidate.clone().unwrap();
            {
                let mut releases = controller.0.releases.lock().await;
                if scenario == "different candidate" {
                    releases
                        .prepared
                        .as_mut()
                        .unwrap()
                        .release
                        .envelope
                        .push('\n');
                } else if scenario == "missing staging" {
                    releases.installation = None;
                }
            }
            controller.publish(Snapshot::new(State::Installing, "Preparing update"));
            controller
                .failed_install(
                    ready,
                    (scenario != "local install").then_some(&target),
                    &InstallationFailure {
                        message: "Installation did not complete".into(),
                        manual: false,
                        recover_host: scenario == "uncertain",
                        retry_ready: true,
                    },
                )
                .await;
            assert_eq!(controller.snapshot().state, State::Error, "{scenario}");
            assert_eq!(
                controller.remote_status().blocked,
                Some(crate::remote::update::BlockedReason::UpdateNotReady)
            );
        }
    }

    #[test]
    fn current_and_unchecked_computers_have_no_false_missing_download_warning() {
        let controller = Controller::new(true);
        for state in [State::Current, State::Idle] {
            controller.publish(Snapshot::new(state, "No prepared update"));
            assert!(controller.remote_status().blocked.is_none());
            assert!(controller.remote_status().offered.is_none());
        }
    }

    #[cfg(unix)]
    fn linux_bundle_fixture(parent: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let bundle = parent.join("maple-agent.app");
        for directory in [
            bundle.clone(),
            bundle.join("usr"),
            bundle.join("usr/bin"),
            bundle.join("usr/lib"),
        ] {
            std::fs::create_dir(&directory).unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        for executable in [
            bundle.join("AppRun"),
            bundle.join("usr/bin/maple-agent"),
            bundle.join("usr/lib/ld-linux-x86-64.so.2"),
        ] {
            std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            bundle.join("bundle.json"),
            serde_json::to_vec(&serde_json::json!({
                "format": "maple-agent-linux-bundle-v1",
                "product": "maple-agent",
                "profile": "prod",
                "version": env!("CARGO_PKG_VERSION"),
                "build": "1",
                "source_sha": env!("MAPLE_GIT_SOURCE_SHA"),
                "bundle_id": "cloud.opensecret.maple.agent",
                "entrypoint": "AppRun",
                "executable": "usr/bin/maple-agent",
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(
            bundle.join("bundle.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        bundle
    }

    #[cfg(unix)]
    #[test]
    fn linux_bundle_discovery_uses_the_bundle_directory_and_manual_fallbacks() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().canonicalize().unwrap();
        let bundle = linux_bundle_fixture(&parent);
        let loader = bundle.join("usr/lib/ld-linux-x86-64.so.2");
        let legacy_appimage = parent.join("old-maple.AppImage");
        std::fs::write(&legacy_appimage, b"legacy AppImage context").unwrap();
        let automatic = linux_installation(&loader, Ok(bundle.join("AppRun")));
        assert!(!automatic.manual_only);
        assert_eq!(automatic.installed_path, bundle);

        for resolved in [
            Err("invalid launcher context".into()),
            Ok(loader.clone()),
            Ok(legacy_appimage),
        ] {
            let manual = linux_installation(&loader, resolved);
            assert!(manual.manual_only);
            assert_eq!(manual.installed_path, loader);
        }
        std::fs::remove_file(bundle.join("bundle.json")).unwrap();
        let invalid = linux_installation(&loader, Ok(bundle.join("AppRun")));
        assert!(invalid.manual_only);
        assert_eq!(invalid.installed_path, loader);
    }

    #[cfg(unix)]
    #[test]
    fn linux_bundle_with_read_only_parent_remains_manual_only() {
        use std::os::unix::fs::PermissionsExt;

        // A privileged test process can replace a mode-0555 directory's child.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().canonicalize().unwrap();
        let bundle = linux_bundle_fixture(&parent);
        let loader = bundle.join("usr/lib/ld-linux-x86-64.so.2");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let manual = linux_installation(&loader, Ok(bundle.join("AppRun")));
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(manual.manual_only);
        assert_eq!(manual.installed_path, loader);
    }

    #[cfg(unix)]
    #[test]
    fn quit_cleanup_without_tokio_preserves_download_and_installation() {
        use std::os::unix::fs::PermissionsExt;

        assert!(tokio::runtime::Handle::try_current().is_err());
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        #[cfg(target_os = "linux")]
        let installed = linux_bundle_fixture(&root);
        #[cfg(not(target_os = "linux"))]
        let installed = root.join("Maple Agent.app");
        #[cfg(target_os = "linux")]
        let staging = {
            use sha2::Digest;
            let binding = sha2::Sha256::digest(installed.as_os_str().as_encoded_bytes());
            root.join(format!(".maple-agent-update-{binding:x}-abcdefghijklmnop"))
        };
        #[cfg(not(target_os = "linux"))]
        let staging = root.join(".maple-agent-update-abcdefghijklmnop");
        let staged = staging.join(installed.file_name().unwrap());
        let cached = root.join("download.payload");
        #[cfg(not(target_os = "linux"))]
        std::fs::create_dir(&installed).unwrap();
        std::fs::create_dir(&staging).unwrap();
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(installed.join("sentinel"), b"current installation").unwrap();
        std::fs::write(&cached, b"authenticated download remains").unwrap();
        let payload = staging.join("payload");
        std::fs::write(&payload, b"derived staging copy").unwrap();
        let lock = maple_agent_updater::installer::acquire_install_lock(&installed).unwrap();
        let controller = Controller::new(true);
        {
            let mut releases = controller.0.releases.try_lock().unwrap();
            releases.installation = Some(maple_agent_updater::installer::InstallPreparation {
                installed_path: installed.clone(),
                staging_dir: staging.clone(),
                payload_path: payload,
                staged_path: staged,
                version: "0.0.2".into(),
                build: "489".into(),
                size: 20,
                sha256: "0".repeat(64),
            });
            releases.install_lock = Some(lock);
        }
        let backend = Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                .unwrap(),
        );
        // No other owner keeps this backend runtime alive. Hold the operation
        // gate until the handoff returns so cleanup cannot finish before then.
        let operation = controller.0.operation.try_lock().unwrap();
        controller.shutdown();
        let cleanup = controller.cleanup_on_exit(backend);
        drop(operation);
        assert!(tokio::runtime::Handle::try_current().is_err());
        // GPUI's test shutdown skips quit futures; poll the production handoff
        // directly on the same reactor-free kind of foreground executor.
        gpui::block_on(cleanup).unwrap();
        assert!(tokio::runtime::Handle::try_current().is_err());
        assert!(!staging.exists());
        assert_eq!(
            std::fs::read(installed.join("sentinel")).unwrap(),
            b"current installation"
        );
        assert_eq!(
            std::fs::read(cached).unwrap(),
            b"authenticated download remains"
        );
        maple_agent_updater::installer::acquire_install_lock(&installed).unwrap();
    }

    #[test]
    fn discovery_state_stays_outside_the_resettable_sql_store() {
        let profile = crate::backend::profile_data_root();
        let store = maple_agent::store::StorePaths::new(&profile);
        assert_eq!(update_state_dir(), profile.join("updates"));
        assert!(!update_state_dir().starts_with(store.root()));
    }

    #[test]
    fn platform_feed_selection_uses_the_exact_compiled_target_pointer() {
        for origin in ["https://updates.trymaple.ai", "https://updates.maple.ai"] {
            let mac = format!("{origin}/agent/stable/latest.json");
            let linux = format!("{origin}/agent/stable/linux/latest.json");
            assert_eq!(
                feed_url_for_target(&mac, Target::DarwinAarch64)
                    .unwrap()
                    .as_str(),
                mac
            );
            assert_eq!(
                feed_url_for_target(&linux, Target::LinuxX86_64)
                    .unwrap()
                    .as_str(),
                linux
            );
            assert!(feed_url_for_target(&mac, Target::LinuxX86_64).is_err());
            assert!(feed_url_for_target(&linux, Target::DarwinAarch64).is_err());
        }
        assert!(feed_url_for_target("not a URL", Target::LinuxX86_64).is_err());
    }

    #[test]
    fn mac_manual_download_remains_available_without_a_linux_bundle() {
        let feed = feed_url_for_target(
            "https://updates.trymaple.ai/agent/stable/latest.json",
            Target::DarwinAarch64,
        )
        .unwrap();
        let key =
            include_str!("../../crates/maple-agent-updater/tests/fixtures/public-test-key.pub")
                .to_owned();
        let release = maple_agent_updater::manifest::verify_envelope(
            include_bytes!("../../crates/maple-agent-updater/tests/fixtures/signed-envelope.json"),
            &feed,
            &[key],
        )
        .unwrap();
        assert!(
            manual_download_url_for_target(&release, Target::DarwinAarch64)
                .unwrap()
                .ends_with("/Maple-Agent.dmg")
        );
        assert!(manual_download_url_for_target(&release, Target::LinuxX86_64).is_none());
    }

    #[test]
    fn remote_candidate_binds_the_exact_verified_platform_document() {
        let feed = "https://updates.trymaple.ai/agent/stable/latest.json"
            .parse()
            .unwrap();
        let key =
            include_str!("../../crates/maple-agent-updater/tests/fixtures/public-test-key.pub")
                .to_owned();
        let release = maple_agent_updater::manifest::verify_envelope(
            include_bytes!("../../crates/maple-agent-updater/tests/fixtures/signed-envelope.json"),
            &feed,
            &[key],
        )
        .unwrap();
        let candidate = release_identity(&release);
        candidate.validate().unwrap();
        let mut same_version_different_document = release.clone();
        // Envelope whitespace does not change the displayed version, but a
        // prepared remote transaction must retain its exact authenticated bytes.
        same_version_different_document.envelope.push('\n');
        assert_ne!(
            candidate,
            release_identity(&same_version_different_document)
        );
        let controller = Controller::new(false);
        let mut ready = Snapshot::new(State::Ready, "Ready");
        ready.remote_candidate = Some(candidate.clone());
        controller.publish(ready.clone());
        assert_eq!(controller.remote_status().offered, Some(candidate));
        assert!(controller.remote_status().blocked.is_none());
        controller.begin_check();
        assert_eq!(
            controller.remote_status().blocked,
            Some(crate::remote::update::BlockedReason::Busy)
        );
        controller.recheck_failed(ready);
        assert!(controller.remote_status().blocked.is_none());
        controller.publish(Snapshot::new(State::Current, "No update"));
        assert!(controller.remote_status().offered.is_none());
    }

    #[test]
    fn linux_manual_download_uses_the_signed_bundle_without_a_mac_download() {
        let feed = feed_url_for_target(
            "https://updates.trymaple.ai/agent/stable/linux/latest.json",
            Target::LinuxX86_64,
        )
        .unwrap();
        let key =
            include_str!("../../crates/maple-agent-updater/tests/fixtures/public-test-key.pub")
                .to_owned();
        let release = maple_agent_updater::manifest::verify_envelope(
            include_bytes!(
                "../../crates/maple-agent-updater/tests/fixtures/signed-linux-v2-envelope.json"
            ),
            &feed,
            &[key],
        )
        .unwrap();
        assert_eq!(
            manual_download_url_for_target(&release, Target::LinuxX86_64).as_deref(),
            Some(
                "https://updates.trymaple.ai/agent/stable/releases/0.0.2/Maple-Agent-linux-x86_64.tar.gz"
            )
        );
        assert!(manual_download_url_for_target(&release, Target::DarwinAarch64).is_none());
    }

    #[tokio::test]
    async fn deferred_store_bootstrap_cannot_overwrite_a_newer_opt_out() {
        let controller = Controller::new(false);
        controller.set_test_state(State::Disabled);
        controller.set_auto_download(true);
        let older_bootstrap = controller.clone().run();
        controller.set_auto_download(false);
        older_bootstrap.await;
        assert_eq!(controller.startup_policy_for_test(), (true, false));
        controller.clone().run().await;
        assert_eq!(controller.startup_policy_for_test(), (true, false));
    }
    #[test]
    fn packaged_prod_needs_pins_and_dev_cannot_enable_updates() {
        assert_eq!(
            disabled_reason("prod", &[], false),
            Some("Updates not configured")
        );
        assert!(disabled_reason("prod", &["key"], false).is_none());
        for profile in ["dev", "unpackaged"] {
            assert!(disabled_reason(profile, &["key"], false).is_some());
        }
        assert_eq!(
            disabled_reason("prod", &["key"], true),
            Some("Update checks disabled")
        );
    }
    #[test]
    fn disk_image_and_translocated_apps_use_manual_installation() {
        assert!(manual_macos_path(Path::new(
            "/Volumes/Maple/Maple Agent.app"
        )));
        assert!(manual_macos_path(Path::new(
            "/private/var/folders/a/AppTranslocation/x/d/Maple Agent.app"
        )));
        assert!(!manual_macos_path(Path::new(
            "/Applications/Maple Agent.app"
        )));
    }
    #[tokio::test]
    async fn later_retains_verified_ready_action_without_a_banner() {
        let controller = Controller::new(true);
        let mut ready = Snapshot::new(State::Ready, "v0.0.2 is ready to install");
        ready.version = Some("0.0.2".into());
        ready.primary = Some(UpdateAction::Restart);
        ready.primary_label = "Restart to Update";
        ready.show_banner = true;
        controller.publish(ready);
        controller.later().await;
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, State::Ready);
        assert_eq!(snapshot.primary, Some(UpdateAction::Restart));
        assert!(!snapshot.show_banner);
        assert_eq!(
            controller.0.releases.lock().await.deferred.as_deref(),
            Some("0.0.2")
        );
    }
    #[test]
    fn failed_recheck_retains_ready_restart_and_manual_offer() {
        let controller = Controller::new(false);
        let mut downloading = Snapshot::new(State::Ready, "v0.0.2 is ready to install");
        downloading.version = Some("0.0.2".into());
        downloading.manual_download_url =
            Some("https://updates.trymaple.ai/agent/stable/releases/0.0.2/Maple.dmg".into());
        downloading.primary = Some(UpdateAction::Restart);
        downloading.primary_label = "Restart to Update";
        downloading.show_banner = true;
        controller.publish(downloading.clone());
        controller.begin_check();
        assert_eq!(controller.snapshot().state, State::Checking);
        controller.recheck_failed(downloading);
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, State::Ready);
        assert_eq!(snapshot.primary, Some(UpdateAction::Restart));
        assert!(snapshot.label.contains("verify it again"));
        assert!(snapshot.manual_download_url.is_some());
        assert!(snapshot.show_banner);
    }
    #[tokio::test]
    async fn repeated_checks_cannot_replace_an_active_transaction() {
        let controller = Controller::new(false);
        let _active = controller.0.operation.lock().await;
        controller.publish(Snapshot::new(State::Installing, "Installing…"));
        controller.check().await;
        controller.download().await;
        assert_eq!(controller.snapshot().state, State::Installing);
        assert!(controller.0.service.get().is_none());
    }
    #[tokio::test]
    async fn shutdown_prevents_new_network_or_install_transactions() {
        let controller = Controller::new(true);
        controller.publish(Snapshot::new(State::Idle, "Not checked yet"));
        controller.shutdown();
        controller.clone().run().await;
        controller.check().await;
        controller.download().await;
        assert!(
            controller
                .install(Arc::new(
                    crate::backend::AgentBackend::for_test(
                        "http://127.0.0.1:9".into(),
                        String::new()
                    )
                    .unwrap()
                ))
                .await
                .unwrap()
                .is_none()
        );
        assert!(controller.0.service.get().is_none());
        assert_eq!(controller.snapshot().state, State::Idle);
    }
    #[tokio::test]
    async fn disabled_controller_is_accurate_and_has_no_network_work() {
        let controller = Controller::new(true);
        controller.publish(Snapshot::new(State::Disabled, "Test disables updates"));
        assert_eq!(controller.snapshot().state, State::Disabled);
        controller.clone().run().await;
        controller.check().await;
        assert_eq!(controller.snapshot().state, State::Disabled);
        assert!(controller.0.service.get().is_none());
    }
    #[tokio::test]
    async fn committed_update_remains_restartable_without_reinstall_or_network() {
        let controller = Controller::new(true);
        let path = PathBuf::from("/Applications/Maple Agent.app");
        controller.set_test_installed(path.clone());
        assert_eq!(controller.snapshot().state, State::RestartRequired);
        assert_eq!(controller.snapshot().primary, Some(UpdateAction::Relaunch));
        assert!(!controller.snapshot().busy());
        controller.fail("A late pre-restart callback failed");
        controller.check().await;
        controller.download().await;
        controller.clone().run().await;
        let backend = Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                .unwrap(),
        );
        assert_eq!(
            controller.install(backend).await.unwrap(),
            Some(path.clone())
        );
        assert_eq!(controller.restart_path(), Some(path));
        assert_eq!(controller.snapshot().state, State::RestartRequired);
        assert!(controller.0.service.get().is_none());
    }
    // Each platform uses its authenticated inventory, including Linux's v2
    // bundle-only manifest, when replacement permissions require manual install.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn permission_failure_switches_to_signed_manual_download_before_host_stop() {
        let controller = Controller::new(true);
        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join(if cfg!(target_os = "linux") {
            "maple-agent.app"
        } else {
            "Maple Agent.app"
        });
        std::fs::create_dir_all(&installed).unwrap();
        #[cfg(target_os = "linux")]
        let (target, envelope, filename) = (
            Target::LinuxX86_64,
            include_bytes!(
                "../../crates/maple-agent-updater/tests/fixtures/signed-linux-v2-envelope.json"
            )
            .as_slice(),
            "Maple-Agent-linux-x86_64.tar.gz",
        );
        #[cfg(target_os = "macos")]
        let (target, envelope, filename) = (
            Target::DarwinAarch64,
            include_bytes!("../../crates/maple-agent-updater/tests/fixtures/signed-envelope.json")
                .as_slice(),
            "Maple-Agent.dmg",
        );
        let feed = feed_url_for_target(
            &format!("https://updates.trymaple.ai{}", target.feed_path()),
            target,
        )
        .unwrap();
        let key =
            include_str!("../../crates/maple-agent-updater/tests/fixtures/public-test-key.pub")
                .to_owned();
        let release = maple_agent_updater::manifest::verify_envelope(
            envelope,
            &feed,
            std::slice::from_ref(&key),
        )
        .unwrap();
        let service = Service {
            updater: Arc::new(
                Updater::new(Config {
                    feed_url: feed,
                    public_keys: vec![key],
                    current_version: "0.0.1".parse().unwrap(),
                    target,
                    installed_path: installed.clone(),
                    state_dir: temp.path().join("updates"),
                })
                .unwrap(),
            ),
            installed_path: installed,
            manual_only: AtomicBool::new(false),
        };
        let error = InstallationFailure::from(maple_agent_updater::installer::InstallError::Io(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        ));
        assert!(
            !error.recover_host,
            "known permission failures must precede host shutdown"
        );
        controller
            .installation_failed(&service, &release, error)
            .await;
        let snapshot = controller.snapshot();
        assert!(service.manual_only());
        assert_eq!(snapshot.state, State::Available);
        assert_eq!(snapshot.primary, Some(UpdateAction::ManualDownload));
        assert!(
            snapshot
                .manual_download_url
                .unwrap()
                .ends_with(&format!("/{filename}"))
        );
        assert!(snapshot.label.contains("install manually"));
        assert!(controller.restart_path().is_none());
    }
}
