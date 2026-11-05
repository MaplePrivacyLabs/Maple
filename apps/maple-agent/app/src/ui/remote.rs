//! Device setup and explicitly targeted remote update controls. Pairing codes
//! and local account settings never traverse the remote task protocol.
use crate::{
    backend::AgentBackend,
    remote::{
        manager::{PairingWindow, Status},
        update::{InstallReply, InstallRequest, OperationState, PeerInfo, UpdateState},
    },
    ui::{
        icons::icon,
        popup::{Menu, MenuItem, Placement, Popup},
        settings::setting_copy,
        task,
        text_input::TextInput,
        theme, widgets,
    },
};
use gpui::{
    Context, Div, Entity, EventEmitter, Focusable, Render, SharedString, Window, div, prelude::*,
    px,
};
use maple_sdk::remote::RemoteDevice;
use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(test)]
mod tests;

pub struct ComputersChanged(pub Status);
pub struct ComputerNamed {
    pub key: String,
    pub name: String,
}

pub(crate) fn computer_name(key: &str, advertised: &str, alias: &str) -> String {
    if !alias.trim().is_empty() {
        alias.to_string()
    } else if advertised == "Maple computer" {
        format!("Computer {}", key.chars().take(8).collect::<String>())
    } else {
        advertised.to_string()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PeerInfoPhase {
    #[default]
    Unchecked,
    Loading,
    Loaded,
    Unsupported,
    Unreachable,
    Incompatible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallPhase {
    Sending,
    Accepted,
    Uncertain,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PeerInfoQuery {
    #[default]
    Read,
    Check,
}

async fn request_peer_info<F, Fut>(
    query: PeerInfoQuery,
    account: String,
    peer: String,
    call: F,
) -> Result<Option<PeerInfo>, String>
where
    F: FnOnce(&'static str, serde_json::Value) -> Fut,
    Fut: Future<Output = Result<serde_json::Value, String>>,
{
    let method = match query {
        PeerInfoQuery::Read => "remote_peer_info",
        PeerInfoQuery::Check => "remote_refresh_update",
    };
    let value = call(method, serde_json::json!([account, peer])).await?;
    match query {
        PeerInfoQuery::Read => serde_json::from_value(value),
        PeerInfoQuery::Check => serde_json::from_value(value).map(Some),
    }
    .map_err(|_| "Could not read remote update status".to_owned())
}

struct InstallAttempt {
    request: InstallRequest,
    phase: InstallPhase,
    label: String,
    /// Recovered from the host's receipt rather than sent from this view.
    from_receipt: bool,
}

/// Colour of a card's badge or note. Every tone is an existing theme color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Tone {
    #[default]
    Muted,
    Accent,
    Success,
    Warning,
    Error,
}

impl Tone {
    fn color(self) -> u32 {
        match self {
            Self::Muted => theme::text_muted(),
            Self::Accent => theme::accent(),
            Self::Success => theme::status_success(),
            Self::Warning => theme::status_warning(),
            Self::Error => theme::status_error(),
        }
    }
}

/// What the card says next to the computer's name. One status line, at
/// most one short note, and the state's one action, all precomputed when
/// the observation changes rather than in render.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CardAction {
    #[default]
    None,
    Update,
    /// The state can only move on after a fresh status check.
    CheckAgain,
}

#[derive(Default)]
struct ComputerUpdate {
    generation: u64,
    phase: PeerInfoPhase,
    query: PeerInfoQuery,
    check_required: bool,
    info: Option<PeerInfo>,
    badge: Option<(&'static str, Tone)>,
    status_line: String,
    note: Option<(String, Tone)>,
    action: CardAction,
    install_available: bool,
    attempt: Option<InstallAttempt>,
}

struct InstallConfirmation {
    peer: String,
    grant: [u8; 16],
    generation: u64,
    title: String,
    action_label: String,
    request: InstallRequest,
    detail: String,
    previous_focus: Option<gpui::FocusHandle>,
    cancel_focus: gpui::FocusHandle,
    install_focus: gpui::FocusHandle,
}

struct InstallIntent {
    account: String,
    peer: String,
    grant: [u8; 16],
    request: InstallRequest,
}

/// A lost reply may already have taken effect: never resend it blindly.
const UNCONFIRMED_INSTALL: &str = "Not confirmed. Don’t resend; check again first.";
const PROTOCOL_UPDATE_REQUIRED: &str = "Update Maple on both computers to reconnect.";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A quiet note when the two computers run different app versions. Equal
/// or unknown versions say nothing.
fn version_note(info: &PeerInfo, client_version: &str) -> Option<String> {
    let client = semver::Version::parse(client_version).ok()?;
    let remote = semver::Version::parse(&info.running.version).ok()?;
    (client != remote).then(|| format!("This computer runs v{client}."))
}

fn install_blocked(info: &PeerInfo) -> Option<String> {
    if !info.supports_remote_updates() {
        return Some("This version can’t be updated remotely.".into());
    }
    // An older host reports an offer it has not downloaded as not ready. It
    // is still asked: it refuses before admitting work, and the reply says so.
    if let Some(reason) = &info.update.blocked
        && !info.install_requestable()
    {
        return Some(reason.message().into());
    }
    if operation_pending(info) {
        return Some("An update request is already in progress there.".into());
    }
    if !info.install_requestable() {
        return Some("The update isn’t ready there yet.".into());
    }
    let Some(target) = &info.update.offered else {
        return Some("No update is offered there.".into());
    };
    if info.running.matches_release(target) {
        return Some("It already runs this version.".into());
    }
    None
}

fn operation_pending(info: &PeerInfo) -> bool {
    info.update.operation.as_ref().is_some_and(|operation| {
        matches!(
            operation.state,
            OperationState::PendingAck
                | OperationState::Queued
                | OperationState::Installing
                | OperationState::Unknown
        )
    })
}

fn version_label(version: &str) -> String {
    format!("v{version}")
}

impl ComputerUpdate {
    fn refresh_presentation(&mut self) {
        self.install_available = false;
        self.badge = None;
        self.note = None;
        self.action = CardAction::None;
        let loading = self.phase == PeerInfoPhase::Loading;
        let checking = self.query == PeerInfoQuery::Check;
        let Some(info) = &self.info else {
            self.status_line = match self.phase {
                PeerInfoPhase::Loading if checking => "Checking for updates…",
                PeerInfoPhase::Loading => "Checking status…",
                PeerInfoPhase::Unsupported | PeerInfoPhase::Unchecked => "Version unknown",
                PeerInfoPhase::Unreachable => "Version unknown",
                PeerInfoPhase::Incompatible => "Different remote protocol",
                PeerInfoPhase::Loaded => "Version unknown",
            }
            .into();
            match self.phase {
                PeerInfoPhase::Unreachable => {
                    self.badge = Some(("Not reachable", Tone::Muted));
                    self.action = CardAction::CheckAgain;
                }
                PeerInfoPhase::Incompatible => {
                    self.badge = Some(("Update needed", Tone::Warning));
                    self.note = Some((PROTOCOL_UPDATE_REQUIRED.into(), Tone::Muted));
                }
                _ => {}
            }
            return;
        };
        let running = version_label(&info.running.version);
        let offered = info.update.offered.as_ref();
        if loading {
            self.status_line = format!(
                "{running} · {}",
                if checking {
                    "checking for updates…"
                } else {
                    "checking status…"
                }
            );
            return;
        }
        // Cached observations stay as history only: they never look like a
        // current offer, policy decision or version comparison.
        match self.phase {
            PeerInfoPhase::Unreachable => {
                self.status_line = format!("{running} when last seen");
                self.badge = Some(("Not reachable", Tone::Muted));
                if checking && !info.supports_remote_refresh() {
                    self.note = Some((
                        "This version checks for updates only on that computer.".into(),
                        Tone::Muted,
                    ));
                }
                self.action = CardAction::CheckAgain;
                return;
            }
            PeerInfoPhase::Incompatible => {
                self.status_line = running;
                self.badge = Some(("Update needed", Tone::Warning));
                self.note = Some((PROTOCOL_UPDATE_REQUIRED.into(), Tone::Muted));
                return;
            }
            PeerInfoPhase::Loaded => {}
            _ => {
                self.status_line = format!("{running} when last seen");
                return;
            }
        }
        let blocked = install_blocked(info);
        let up_to_date = offered.is_none_or(|target| info.running.matches_release(target))
            && matches!(info.update.state, UpdateState::Current | UpdateState::Idle);
        let target = offered.map(|target| version_label(&target.version));
        let (status_line, badge) = match (info.update.state, target.as_deref()) {
            _ if up_to_date && info.update.state == UpdateState::Current => {
                (running.clone(), Some(("Up to date", Tone::Success)))
            }
            // The Update button already says so; the line names the versions.
            (UpdateState::Available | UpdateState::Ready, Some(target)) => {
                (format!("{running} · {target} available"), None)
            }
            (UpdateState::Downloading, Some(target)) => {
                (format!("{running} · downloading {target}…"), None)
            }
            (UpdateState::Downloading, None) => (format!("{running} · downloading…"), None),
            (UpdateState::Checking, _) => (format!("{running} · checking for updates…"), None),
            (UpdateState::Installing, _) => (
                format!("{running} · updating…"),
                Some(("Updating", Tone::Accent)),
            ),
            (UpdateState::RestartRequired, _) => (
                format!("{running} · restart needed there"),
                Some(("Restart needed", Tone::Warning)),
            ),
            (UpdateState::Error, _) => (
                running.clone(),
                Some(("Updater needs attention", Tone::Warning)),
            ),
            _ => (running.clone(), None),
        };
        self.status_line = status_line;
        self.badge = badge;
        // At most one note, in order of what matters most: this view's own
        // request, a reason the offer cannot be installed, then versions.
        let healthy_without_offer = offered.is_none()
            && matches!(
                info.update.state,
                UpdateState::Current | UpdateState::Idle | UpdateState::Checking
            )
            && matches!(
                info.update.blocked,
                None | Some(crate::remote::update::BlockedReason::UpdateNotReady)
            )
            && !operation_pending(info);
        let pending = self
            .attempt
            .as_ref()
            .is_some_and(|attempt| match attempt.phase {
                InstallPhase::Sending | InstallPhase::Accepted | InstallPhase::Uncertain => true,
                InstallPhase::Succeeded => offered == Some(&attempt.request.target),
                InstallPhase::Failed => false,
            });
        self.install_available = !pending && !self.check_required && blocked.is_none();
        if let Some(attempt) = &self.attempt {
            let target = version_label(&attempt.request.target.version);
            match attempt.phase {
                InstallPhase::Sending => {
                    self.status_line = format!("{running} · sending update…");
                    self.badge = Some(("Updating", Tone::Accent));
                }
                InstallPhase::Accepted => {
                    self.status_line = format!("{running} · updating to {target}…");
                    self.badge = Some(("Updating", Tone::Accent));
                    self.note = Some((attempt.label.clone(), Tone::Muted));
                }
                InstallPhase::Uncertain => {
                    self.badge = Some(("Unconfirmed", Tone::Warning));
                    self.note = Some((attempt.label.clone(), Tone::Warning));
                    self.action = CardAction::CheckAgain;
                }
                // This view's own declined request explains itself until a
                // new check. A recovered receipt matters only while its
                // target is still the offer and is not already running.
                InstallPhase::Failed
                    if !attempt.from_receipt
                        || (offered == Some(&attempt.request.target)
                            && !info.running.matches_release(&attempt.request.target)) =>
                {
                    self.note = Some((attempt.label.clone(), Tone::Error));
                }
                // A past success is history once its target runs: say nothing.
                InstallPhase::Succeeded | InstallPhase::Failed => {}
            }
        }
        if self.note.is_none()
            && !healthy_without_offer
            && let Some(reason) = blocked.as_ref().filter(|_| !self.install_available)
            && !operation_pending(info)
            && offered.is_some()
            && !matches!(
                info.update.state,
                UpdateState::Downloading | UpdateState::Checking
            )
        {
            self.note = Some((reason.clone(), Tone::Muted));
        }
        if self.check_required {
            self.action = CardAction::CheckAgain;
        } else if self.install_available {
            self.action = CardAction::Update;
        }
        // Different versions matter only when nothing will bring them
        // together: an offered update or one in progress says enough.
        if self.note.is_none()
            && self.action == CardAction::None
            && self.badge.is_none_or(|(label, _)| label == "Up to date")
            && offered.is_none()
            && let Some(note) = version_note(info, CLIENT_VERSION)
        {
            self.note = Some((note, Tone::Muted));
        }
    }

    fn reconcile_operation(&mut self) {
        let Some(info) = &self.info else {
            return;
        };
        if self.attempt.is_none()
            && let Some(operation) = &info.update.operation
        {
            // A reopened Settings view can recover the authenticated host's
            // operation identity. This never resubmits that mutation.
            self.attempt = Some(InstallAttempt {
                request: InstallRequest {
                    operation_id: operation.operation_id.clone(),
                    target: operation.target.clone(),
                },
                phase: InstallPhase::Uncertain,
                label: UNCONFIRMED_INSTALL.into(),
                from_receipt: true,
            });
        }
        let Some(attempt) = &mut self.attempt else {
            return;
        };
        // Running a version is independent of the outcome of this request:
        // a local install or another controller may have installed it. Only
        // the matching durable receipt can establish this operation's result.
        if matches!(
            attempt.phase,
            InstallPhase::Succeeded | InstallPhase::Failed
        ) {
            return;
        }
        let Some(operation) = &info.update.operation else {
            // An absent receipt does not prove that a disconnected mutation
            // never happened. Keep its nonce and require manual recovery.
            return;
        };
        if operation.operation_id != attempt.request.operation_id
            || operation.target != attempt.request.target
        {
            return;
        }
        let target = version_label(&attempt.request.target.version);
        match operation.state {
            OperationState::PendingAck => {
                attempt.phase = InstallPhase::Accepted;
                attempt.label = "Waiting for it to acknowledge. Nothing has started.".into();
            }
            OperationState::Queued | OperationState::Installing => {
                attempt.phase = InstallPhase::Accepted;
                attempt.label = ACCEPTED_INSTALL.into();
            }
            OperationState::Succeeded => {
                attempt.phase = InstallPhase::Succeeded;
                attempt.label = format!("Updated to {target}.");
            }
            OperationState::Failed => {
                attempt.phase = InstallPhase::Failed;
                attempt.label = format!("The update to {target} didn’t finish.");
            }
            OperationState::Unknown => {}
        }
    }
}

const ACCEPTED_INSTALL: &str = "It restarts when done. Check again once it’s back.";

pub struct RemoteScreen {
    backend: Arc<AgentBackend>,
    account: String,
    active: bool,
    status: Option<Status>,
    devices: Option<Vec<RemoteDevice>>,
    selected_device: Option<String>,
    code_input: Entity<TextInput>,
    names: std::collections::HashMap<String, String>,
    renaming: Option<(String, Entity<TextInput>)>,
    local_name_input: Option<Entity<TextInput>>,
    local_code: Option<(String, Instant)>,
    error: Option<String>,
    notice: Option<String>,
    busy: Option<&'static str>,
    computer_updates: HashMap<String, ComputerUpdate>,
    next_update_generation: u64,
    install_confirmation: Option<InstallConfirmation>,
    /// One overflow menu per paired computer, keyed by its endpoint.
    popup: Popup<RemoteScreen, String>,
    bridges: RefCell<Vec<gpui::Task<()>>>,
}
impl EventEmitter<ComputersChanged> for RemoteScreen {}
impl EventEmitter<ComputerNamed> for RemoteScreen {}

impl RemoteScreen {
    pub fn new(
        backend: Arc<AgentBackend>,
        account: String,
        active: bool,
        names: std::collections::HashMap<String, String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak = cx.entity().downgrade();
        let code_input = cx.new(|cx| {
            TextInput::new("Eight-character pairing code", cx)
                .pairing_code()
                .on_enter(move |_, _, cx| {
                    let weak = weak.clone();
                    cx.defer(move |cx| {
                        weak.update(cx, |screen: &mut Self, cx| screen.pair_selected(cx))
                            .ok();
                    });
                })
        });
        let mut screen = Self {
            backend,
            account,
            active,
            status: None,
            devices: None,
            selected_device: None,
            code_input,
            names,
            renaming: None,
            local_name_input: None,
            local_code: None,
            error: None,
            notice: None,
            busy: None,
            computer_updates: HashMap::new(),
            next_update_generation: 0,
            install_confirmation: None,
            popup: Popup::new(|this| &mut this.popup, cx),
            bridges: RefCell::new(Vec::new()),
        };
        screen.refresh(false, cx);
        let timer = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this
                    .update(cx, |screen, cx| {
                        let counting = screen.local_code.is_some();
                        if screen
                            .local_code
                            .as_ref()
                            .is_some_and(|(_, deadline)| *deadline <= Instant::now())
                            && let Some((mut code, _)) = screen.local_code.take()
                        {
                            zeroize::Zeroize::zeroize(&mut code);
                            screen.notice = Some(
                                "Pairing code expired. Show a new code when you are ready.".into(),
                            );
                        }
                        if counting {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        screen.bridges.borrow_mut().push(timer);
        screen
    }

    /// The controller's account snapshot can arrive after Settings opens.
    /// Preserve any active editor while updating the labels it surrounds.
    pub(crate) fn set_names(
        &mut self,
        names: std::collections::HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        self.names = names;
        cx.notify();
    }

    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            self.refresh_peer_versions(cx);
        } else {
            self.install_confirmation = None;
        }
    }

    fn refresh_peer_versions(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let peers: Vec<_> = self
            .status
            .iter()
            .flat_map(|status| status.peers.iter())
            .filter(|(_, peer)| peer.can_connect)
            .map(|(key, _)| key.clone())
            .collect();
        for peer in peers {
            self.refresh_peer_info(peer, cx);
        }
    }

    fn peer_grant(&self, peer: &str) -> Option<[u8; 16]> {
        self.status
            .as_ref()?
            .peers
            .get(peer)
            .filter(|peer| peer.can_connect)
            .map(|peer| peer.grant)
    }

    fn install_grant(&self, peer: &str) -> Option<[u8; 16]> {
        self.status
            .as_ref()?
            .peers
            .get(peer)
            .filter(|peer| peer.can_connect)
            .and_then(|peer| peer.host_grant)
    }

    fn can_install(&self, peer: &str) -> bool {
        self.install_grant(peer).is_some()
            && self
                .computer_updates
                .get(peer)
                .is_some_and(|update| update.install_available)
    }

    fn confirmation_is_current(&self, confirmation: &InstallConfirmation) -> bool {
        self.install_grant(&confirmation.peer) == Some(confirmation.grant)
            && self
                .computer_updates
                .get(&confirmation.peer)
                .is_some_and(|update| {
                    update.generation == confirmation.generation
                        && update.install_available
                        && update
                            .info
                            .as_ref()
                            .and_then(|info| info.update.offered.as_ref())
                            == Some(&confirmation.request.target)
                })
    }

    fn invalidate_confirmation(&mut self) {
        if self
            .install_confirmation
            .as_ref()
            .is_some_and(|confirmation| !self.confirmation_is_current(confirmation))
        {
            self.install_confirmation = None;
        }
    }

    fn refresh_peer_info(&mut self, peer: String, cx: &mut Context<Self>) {
        self.query_peer_info(peer, PeerInfoQuery::Read, cx);
    }

    fn check_peer_update(&mut self, peer: String, cx: &mut Context<Self>) {
        self.query_peer_info(peer, PeerInfoQuery::Check, cx);
    }

    fn query_peer_info(&mut self, peer: String, query: PeerInfoQuery, cx: &mut Context<Self>) {
        let Some(grant) = self.peer_grant(&peer) else {
            return;
        };
        if self
            .computer_updates
            .get(&peer)
            .is_some_and(|update| update.phase == PeerInfoPhase::Loading)
        {
            return;
        }
        self.next_update_generation = self.next_update_generation.wrapping_add(1);
        let generation = self.next_update_generation;
        let update = self.computer_updates.entry(peer.clone()).or_default();
        update.generation = generation;
        update.phase = PeerInfoPhase::Loading;
        update.query = query;
        update.refresh_presentation();
        self.invalidate_confirmation();
        let backend = self.backend.clone();
        let account = self.account.clone();
        let target = peer.clone();
        let bridge = task::call(
            &self.backend,
            async move {
                request_peer_info(query, account, target, move |method, params| async move {
                    backend
                        .remote_setting::<serde_json::Value>(method, params)
                        .await
                })
                .await
            },
            cx,
            move |screen, result, cx| {
                screen.apply_peer_info_result(&peer, grant, generation, query, result);
                cx.notify();
            },
        );
        task::retain(&self.bridges, bridge);
        cx.notify();
    }

    #[cfg(test)]
    fn apply_peer_info(
        &mut self,
        peer: &str,
        grant: [u8; 16],
        generation: u64,
        result: Result<Option<PeerInfo>, String>,
    ) {
        self.apply_peer_info_result(peer, grant, generation, PeerInfoQuery::Read, result);
    }

    fn apply_peer_info_result(
        &mut self,
        peer: &str,
        grant: [u8; 16],
        generation: u64,
        query: PeerInfoQuery,
        result: Result<Option<PeerInfo>, String>,
    ) {
        if self.peer_grant(peer) != Some(grant) {
            return;
        }
        let Some(update) = self.computer_updates.get_mut(peer) else {
            return;
        };
        if update.generation != generation {
            return;
        }
        update.query = query;
        match result {
            Ok(Some(info)) => {
                update.phase = PeerInfoPhase::Loaded;
                if query == PeerInfoQuery::Check {
                    update.check_required = false;
                }
                update.info = Some(info);
                update.reconcile_operation();
            }
            Ok(None) => {
                update.phase = PeerInfoPhase::Unsupported;
                update.info = None;
            }
            Err(error) => {
                update.phase = if crate::remote::wire::is_protocol_incompatibility_refusal(&error) {
                    PeerInfoPhase::Incompatible
                } else {
                    PeerInfoPhase::Unreachable
                };
            }
        }
        update.refresh_presentation();
        self.invalidate_confirmation();
    }

    fn ask_remote_install(&mut self, peer: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.install_confirmation.is_some() {
            return;
        }
        let Some(grant) = self.install_grant(&peer) else {
            return;
        };
        let Some(update) = self
            .computer_updates
            .get(&peer)
            .filter(|update| update.install_available)
        else {
            return;
        };
        let Some(target) = update
            .info
            .as_ref()
            .and_then(|info| info.update.offered.clone())
        else {
            return;
        };
        let advertised = self
            .status
            .as_ref()
            .and_then(|status| status.peers.get(&peer))
            .map_or("Maple computer", |peer| peer.name.as_str());
        let name = computer_name(
            &peer,
            advertised,
            self.names.get(&peer).map_or("", String::as_str),
        );
        let cancel_focus = cx.focus_handle();
        let install_focus = cx.focus_handle();
        let previous_focus = window.focused(cx);
        window.focus(&cancel_focus, cx);
        let version = version_label(&target.version);
        self.install_confirmation = Some(InstallConfirmation {
            peer,
            grant,
            generation: update.generation,
            title: format!("Update {name} to {version}?"),
            action_label: format!("Update {name}"),
            detail: format!(
                "{name} stops its running tasks, installs {version} and restarts. Remote access may not return; if it doesn’t, open Maple on {name} directly. This computer stays on v{CLIENT_VERSION} and won’t restart."
            ),
            request: InstallRequest {
                operation_id: uuid::Uuid::new_v4().to_string(),
                target,
            },
            previous_focus,
            cancel_focus,
            install_focus,
        });
        cx.notify();
    }

    fn cancel_remote_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(confirmation) = self.install_confirmation.take()
            && let Some(previous) = confirmation.previous_focus
        {
            window.focus(&previous, cx);
        }
        cx.notify();
    }

    fn take_install_intent(&mut self) -> Option<InstallIntent> {
        let confirmation = self.install_confirmation.take()?;
        if !self.confirmation_is_current(&confirmation) {
            self.notice = Some("This computer’s update or pairing changed. Refresh its status before trying again.".into());
            return None;
        }
        Some(InstallIntent {
            account: self.account.clone(),
            peer: confirmation.peer,
            grant: confirmation.grant,
            request: confirmation.request,
        })
    }

    fn confirm_remote_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self
            .install_confirmation
            .as_ref()
            .and_then(|confirmation| confirmation.previous_focus.as_ref())
        {
            window.focus(previous, cx);
        }
        let Some(intent) = self.take_install_intent() else {
            cx.notify();
            return;
        };
        let peer = intent.peer.clone();
        let request = intent.request.clone();
        let Some(update) = self.computer_updates.get_mut(&peer) else {
            return;
        };
        // A lost reply is an uncertain mutation, not permission to submit it
        // again. Keep this exact operation identity through status refreshes.
        update.attempt = Some(InstallAttempt {
            request: request.clone(),
            phase: InstallPhase::Sending,
            label: "Sending the update request…".into(),
            from_receipt: false,
        });
        update.refresh_presentation();
        let backend = self.backend.clone();
        let grant = intent.grant;
        let bridge = task::call(
            &self.backend,
            async move {
                backend
                    .remote_setting::<InstallReply>(
                        "remote_install_update",
                        (intent.account, intent.peer, intent.grant, intent.request),
                    )
                    .await
            },
            cx,
            move |screen, result, cx| {
                screen.apply_install_reply(&peer, grant, &request, result);
                cx.notify();
            },
        );
        task::retain(&self.bridges, bridge);
        cx.notify();
    }

    fn apply_install_reply(
        &mut self,
        peer: &str,
        grant: [u8; 16],
        request: &InstallRequest,
        result: Result<InstallReply, String>,
    ) {
        if self.install_grant(peer) != Some(grant) {
            return;
        }
        let Some(update) = self.computer_updates.get_mut(peer) else {
            return;
        };
        let Some(attempt) = update.attempt.as_mut().filter(|attempt| {
            attempt.request == *request && attempt.phase == InstallPhase::Sending
        }) else {
            return;
        };
        match result {
            Ok(InstallReply::Accepted { receipt })
                if receipt.operation_id == request.operation_id
                    && receipt.target == request.target =>
            {
                attempt.phase = InstallPhase::Accepted;
                attempt.label = ACCEPTED_INSTALL.into();
            }
            Ok(InstallReply::Refused { reason }) => {
                attempt.phase = InstallPhase::Failed;
                attempt.label = format!("Not updated. {}", reason.message());
                update.check_required = true;
            }
            _ => {
                attempt.phase = InstallPhase::Uncertain;
                attempt.label = UNCONFIRMED_INSTALL.into();
            }
        }
        update.refresh_presentation();
    }

    fn clear_local_code(&mut self) {
        if let Some((mut code, _)) = self.local_code.take() {
            zeroize::Zeroize::zeroize(&mut code);
        }
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected_device = None;
        self.code_input.update(cx, |input, cx| input.clear(cx));
    }

    /// Fed by the desktop's existing local-status refresh, including pairing
    /// completed by the other computer. No new discovery polling is needed.
    pub fn update_status(&mut self, status: Status, cx: &mut Context<Self>) {
        if self.status.as_ref() == Some(&status) {
            return;
        }
        let paired_controller = self.status.is_some()
            && self.local_code.is_some()
            && status.peers.iter().any(|(key, peer)| {
                peer.controls_this_computer
                    && self
                        .status
                        .as_ref()
                        .and_then(|old| old.peers.get(key))
                        .is_none_or(|old| !old.controls_this_computer || old.grant != peer.grant)
            });
        if paired_controller || !status.enabled {
            self.clear_local_code();
        }
        if paired_controller {
            self.notice =
                Some("Computer paired. It can now use this computer’s projects and tools.".into());
        }
        if self
            .selected_device
            .as_ref()
            .is_some_and(|key| status.peers.get(key).is_some_and(|peer| peer.can_connect))
        {
            self.clear_selection(cx);
        }
        self.computer_updates.retain(|key, update| {
            let old = self.status.as_ref().and_then(|old| old.peers.get(key));
            let peer = status.peers.get(key);
            if old.zip(peer).is_some_and(|(old, peer)| {
                old.can_connect
                    && peer.can_connect
                    && old.grant == peer.grant
                    && old.host_grant == peer.host_grant
            }) {
                return true;
            }
            // Neither re-pairing direction settles an outstanding install.
            // Retain its nonce through revocation/recovery, but invalidate all
            // observations and callbacks made under the previous bindings.
            let Some(attempt) = update.attempt.as_mut().filter(|attempt| {
                matches!(
                    attempt.phase,
                    InstallPhase::Sending | InstallPhase::Accepted | InstallPhase::Uncertain
                )
            }) else {
                return false;
            };
            let outgoing_changed = old.zip(peer).is_none_or(|(old, peer)| {
                !old.can_connect || !peer.can_connect || old.host_grant != peer.host_grant
            });
            if outgoing_changed && attempt.phase == InstallPhase::Sending {
                attempt.phase = InstallPhase::Uncertain;
                attempt.label = UNCONFIRMED_INSTALL.into();
            }
            update.generation = update.generation.wrapping_add(1);
            update.phase = PeerInfoPhase::Unchecked;
            update.query = PeerInfoQuery::Read;
            update.refresh_presentation();
            true
        });
        self.status = Some(status);
        self.invalidate_confirmation();
        cx.notify();
    }

    fn can_connect(&self, endpoint: &str) -> bool {
        self.status
            .as_ref()
            .and_then(|s| s.peers.get(endpoint))
            .is_some_and(|peer| peer.can_connect)
    }

    fn select_device(&mut self, endpoint: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.clear_selection(cx);
        self.selected_device = Some(endpoint);
        self.error = None;
        self.notice = None;
        window.focus(&self.code_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn pair_selected(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(endpoint) = self.selected_device.clone() else {
            return;
        };
        let code =
            zeroize::Zeroizing::new(self.code_input.read(cx).text().trim().to_ascii_uppercase());
        if code.len() != 8 {
            self.error = Some("Enter all eight characters shown on the selected computer.".into());
            cx.notify();
            return;
        }
        self.code_input.update(cx, |input, cx| input.clear(cx));
        self.action(
            "remote_pair",
            serde_json::json!([endpoint, code.as_str()]),
            cx,
        );
    }

    fn rename(&mut self, key: String, current: String, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("Computer name", cx).on_enter(move |_, _, cx| {
                let weak = weak.clone();
                cx.defer(move |cx| {
                    weak.update(cx, |screen, cx| screen.save_name(cx)).ok();
                });
            });
            input.set_text(&current, cx);
            input
        });
        self.renaming = Some((key, input));
        cx.notify();
    }

    fn rename_this_computer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = &self.status else {
            return;
        };
        let current = status.name.clone();
        let weak = cx.entity().downgrade();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("Computer name", cx).on_enter(move |_, _, cx| {
                let weak = weak.clone();
                cx.defer(move |cx| {
                    weak.update(cx, |screen, cx| screen.save_this_computer_name(cx))
                        .ok();
                });
            });
            input.set_text(&current, cx);
            input
        });
        window.focus(&input.focus_handle(cx), cx);
        self.local_name_input = Some(input);
        self.error = None;
        cx.notify();
    }

    fn save_this_computer_name(&mut self, cx: &mut Context<Self>) {
        let Some(input) = &self.local_name_input else {
            return;
        };
        match crate::remote::validate_name(&input.read(cx).text()) {
            Ok(name) => self.action("remote_set_name", serde_json::json!([name]), cx),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn save_name(&mut self, cx: &mut Context<Self>) {
        let Some((key, input)) = &self.renaming else {
            return;
        };
        let name = input.read(cx).text().trim().to_string();
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            self.error = Some("Use a computer name between 1 and 64 characters.".into());
            cx.notify();
            return;
        }
        let key = key.clone();
        self.names.insert(key.clone(), name.clone());
        self.renaming = None;
        self.error = None;
        let backend = self.backend.clone();
        let user = backend.saved_user_id();
        let saved_key = key.clone();
        let saved_name = name.clone();
        let request = self.backend.spawn(async move {
            let user = user.ok_or("Sign in before naming a remote computer")?;
            backend
                .set_remote_sidebar(
                    &user,
                    saved_key,
                    serde_json::json!({"display_name":saved_name}),
                )
                .await
        });
        let bridge = cx.spawn(async move |this, cx| {
            let result = request
                .await
                .unwrap_or_else(|_| Err("Could not save the computer name".into()));
            if let Err(error) = result {
                this.update(cx, |screen, cx| {
                    screen.error = Some(error);
                    cx.notify();
                })
                .ok();
            }
        });
        task::retain(&self.bridges, bridge);
        cx.emit(ComputerNamed { key, name });
        cx.notify();
    }
    fn refresh(&mut self, discover: bool, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(if discover {
            "Finding computers…"
        } else {
            "Updating remote settings…"
        });
        if discover {
            self.clear_selection(cx);
            self.notice = None;
        }
        self.error = None;
        let backend = self.backend.clone();
        let bridge = task::call(
            &self.backend,
            async move {
                let devices = if discover {
                    backend.remote_setting("remote_discover", ()).await?
                } else {
                    Vec::new()
                };
                let status: Status = backend.remote_setting("remote_status", ()).await?;
                let window: Option<PairingWindow> =
                    backend.remote_setting("remote_pairing_window", ()).await?;
                Ok((status, devices, window))
            },
            cx,
            move |screen, result, cx| {
                screen.busy = None;
                match result {
                    Ok((status, devices, window)) => {
                        screen.clear_local_code();
                        if let Some(window) = window {
                            screen.local_code = Some((
                                window.code,
                                Instant::now() + Duration::from_secs(window.expires_in),
                            ));
                        }
                        cx.emit(ComputersChanged(status.clone()));
                        screen.update_status(status, cx);
                        screen.refresh_peer_versions(cx);
                        if discover {
                            screen.devices = Some(devices);
                        }
                        screen.error = None;
                    }
                    Err(error) => screen.error = Some(error),
                }
                cx.notify();
            },
        );
        task::retain(&self.bridges, bridge);
        cx.notify();
    }
    fn action(&mut self, method: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(match method {
            "remote_begin_pairing" => "Preparing pairing code…",
            "remote_pair" | "remote_reconcile" => "Pairing computer…",
            "remote_revoke" => "Unpairing computer…",
            _ => "Updating remote settings…",
        });
        self.error = None;
        self.notice = None;
        let backend = self.backend.clone();
        let bridge = task::call(
            &self.backend,
            async move {
                backend
                    .remote_setting::<serde_json::Value>(method, params)
                    .await
            },
            cx,
            move |screen, result, cx| {
                screen.busy = None;
                match result {
                    Ok(value) => {
                        if method == "remote_begin_pairing" {
                            match serde_json::from_value::<PairingWindow>(value) {
                                Ok(window) => {
                                    screen.local_code = Some((
                                        window.code,
                                        Instant::now()
                                            + Duration::from_secs(window.expires_in.min(300)),
                                    ))
                                }
                                Err(_) => {
                                    screen.error = Some("Could not read the pairing window".into());
                                    cx.notify();
                                    return;
                                }
                            }
                        } else if method == "remote_cancel_pairing" || method == "remote_enable" {
                            screen.clear_local_code();
                        } else if method == "remote_pair" || method == "remote_reconcile" {
                            screen.clear_selection(cx);
                            screen.notice = Some(
                                "Computer paired. Its projects are now available in the sidebar."
                                    .into(),
                            );
                        } else if method == "remote_revoke" {
                            screen.notice = Some("Computer unpaired.".into());
                        } else if method == "remote_set_name" {
                            screen.local_name_input = None;
                            screen.notice = Some("Computer name saved. Other computers pick it up when they refresh devices or reconnect.".into());
                        }
                        screen.refresh(false, cx);
                    }
                    Err(error) => screen.error = Some(error),
                }
                cx.notify();
            },
        );
        task::retain(&self.bridges, bridge);
        cx.notify();
    }
}
impl Drop for RemoteScreen {
    fn drop(&mut self) {
        self.clear_local_code();
    }
}
impl Render for RemoteScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.popup.sync_focus(window, cx);
        div()
            .id("remote-devices")
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(div().text_lg().child("Remote computers"))
            .child(
                div().text_color(gpui::rgb(theme::text_secondary())).child(
                    "Work on another computer, or allow a paired computer to work on this one.",
                ),
            )
            .when_some(self.busy, |view, message| {
                view.child(div().text_sm().child(message))
            })
            .when_some(self.error.as_ref(), |view, error| {
                view.child(
                    widgets::card_row()
                        .border_color(gpui::rgb(theme::status_error()))
                        .child(error.clone()),
                )
            })
            .when_some(self.notice.as_ref(), |view, notice| {
                view.child(widgets::card_row().child(notice.clone()))
            })
            .when_some(
                self.status.as_ref().and_then(|s| s.error.as_ref()),
                |view, error| view.child(widgets::card_row().child(error.clone())),
            )
            .child(self.render_this_computer(cx))
            .child(self.render_discovery(cx))
            .when(
                self.status.as_ref().is_some_and(|s| !s.peers.is_empty()),
                |view| view.child(self.render_paired(window, cx)),
            )
    }
}

impl RemoteScreen {
    fn render_this_computer(&self, cx: &mut Context<Self>) -> Div {
        let enabled = self.status.as_ref().is_some_and(|s| s.enabled);
        let access = if self.status.is_none() {
            "Loading…"
        } else if enabled && !self.status.as_ref().is_some_and(|s| s.ready) {
            "Starting. Waiting for a connection…"
        } else if enabled {
            "On. Only paired computers can connect."
        } else {
            "Off. Other computers can’t connect here."
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                widgets::card_row()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(setting_copy("Remote access", access))
                    .when(self.status.is_some(), |row| {
                        row.child(
                            widgets::switch("remote-enable", enabled)
                                .aria_label(if enabled {
                                    "Turn off remote access"
                                } else {
                                    "Enable remote access"
                                })
                                .on_click(cx.listener(move |screen, _, _, cx| {
                                    screen.action("remote_enable", serde_json::json!([!enabled]), cx)
                                })),
                        )
                    }),
            )
            .when_some(self.status.as_ref(), |view, status| {
                view.child(match &self.local_name_input {
                    None => widgets::card_row()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(setting_copy(&status.name, "This computer’s name on your other computers."))
                        .child(
                            widgets::ghost_button("rename-this-computer")
                                .aria_label("Rename this computer")
                                .child("Rename")
                                .debug_selector(|| "rename-this-computer".to_string())
                                .on_click(cx.listener(|screen, _, window, cx| {
                                    screen.rename_this_computer(window, cx)
                                })),
                        ),
                    Some(input) => widgets::card_row().child(
                        div()
                            .id("this-computer-name-editor")
                            .debug_selector(|| "this-computer-name-editor".to_string())
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(widgets::input_frame().flex_1().min_w_0().child(input.clone()))
                                    .child(
                                        widgets::secondary_button("save-this-computer-name")
                                            .aria_label("Save computer name")
                                            .child("Save")
                                            .on_click(cx.listener(|screen, _, _, cx| {
                                                screen.save_this_computer_name(cx)
                                            })),
                                    )
                                    .child(
                                        widgets::ghost_button("cancel-this-computer-name")
                                            .aria_label("Cancel rename")
                                            .child("Cancel")
                                            .debug_selector(|| "cancel-this-computer-name".to_string())
                                            .on_click(cx.listener(|screen, _, _, cx| {
                                                screen.local_name_input = None;
                                                screen.error = None;
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(gpui::rgb(theme::text_muted()))
                                    .child("Shown to your other computers. This doesn’t change the system hostname."),
                            ),
                    ),
                })
            })
            .when(enabled, |view| {
                view.child(match self.local_code.as_ref() {
                    None => widgets::card_row()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(setting_copy(
                            "Pair a new computer",
                            "Show a one-time code here, then enter it on the other computer.",
                        ))
                        .child(
                            widgets::primary_button("pairing-window")
                                .aria_label("Show pairing code")
                                .child("Show pairing code")
                                .on_click(cx.listener(|screen, _, _, cx| {
                                    screen.action("remote_begin_pairing", serde_json::json!([]), cx)
                                })),
                        ),
                    Some((code, deadline)) => {
                        let left = deadline.saturating_duration_since(Instant::now()).as_secs();
                        widgets::card_row()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(setting_copy(
                                "Pair a new computer",
                                "On the other computer, choose Find computers, select this one, and enter this code.",
                            ))
                            .child(pairing_code_boxes(code))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_4()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(gpui::rgb(theme::text_muted()))
                                            .child(format!(
                                                "One attempt · expires in {}:{:02}",
                                                left / 60,
                                                left % 60
                                            )),
                                    )
                                    .child(
                                        widgets::secondary_button("cancel-window")
                                            .aria_label("Cancel pairing")
                                            .child("Cancel pairing")
                                            .on_click(cx.listener(|screen, _, _, cx| {
                                                screen.action("remote_cancel_pairing", serde_json::json!([]), cx)
                                            })),
                                    ),
                            )
                    }
                })
            })
    }

    fn render_discovery(&self, cx: &mut Context<Self>) -> Div {
        let searched = self.devices.is_some();
        let no_devices = self.devices.as_ref().is_some_and(|devices| {
            devices
                .iter()
                .all(|device| self.can_connect(&device.endpoint))
        });
        widgets::card_row()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(setting_copy(
                        "Connect to another computer",
                        "Find computers signed in to your account. Turn on remote access there and show a pairing code.",
                    ))
                    .child(
                        widgets::secondary_button("discover")
                            .aria_label(if searched { "Refresh devices" } else { "Find computers" })
                            .child(if searched { "Search again" } else { "Find computers" })
                            .on_click(cx.listener(|screen, _, _, cx| screen.refresh(true, cx))),
                    ),
            )
            .when(no_devices, |view| {
                view.child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child("No new computers found. Check it’s online and signed in, then search again."),
                )
            })
            .children(
                self.devices
                    .iter()
                    .flatten()
                    .filter(|device| !self.can_connect(&device.endpoint))
                    .map(|device| self.render_device(device, cx)),
            )
    }

    fn render_device(&self, device: &RemoteDevice, cx: &mut Context<Self>) -> Div {
        let endpoint = device.endpoint.clone();
        let selected = self.selected_device.as_ref() == Some(&endpoint);
        let ready = device.window.is_some();
        div()
            .p_3()
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .rounded(theme::RADIUS_MD)
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(computer_name(
                                &endpoint,
                                &device.name,
                                self.names.get(&endpoint).map_or("", String::as_str),
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(gpui::rgb(theme::text_secondary()))
                                    .child(if ready {
                                        "Ready to pair"
                                    } else {
                                        "Show a pairing code on this computer, then refresh."
                                    }),
                            ),
                    )
                    .when(ready && !selected, |row| {
                        row.child(
                            widgets::secondary_button(format!("select-{endpoint}"))
                                .debug_selector({
                                    let endpoint = endpoint.clone();
                                    move || format!("select-{endpoint}")
                                })
                                .aria_label("Enter code")
                                .child("Enter code")
                                .on_click(cx.listener(move |screen, _, window, cx| {
                                    screen.select_device(endpoint.clone(), window, cx)
                                })),
                        )
                    })
                    .when(selected, |row| {
                        row.child(
                            widgets::ghost_button("cancel-code")
                                .debug_selector(|| "cancel-code".to_string())
                                .aria_label("Cancel code entry")
                                .child("Cancel")
                                .on_click(cx.listener(|screen, _, _, cx| {
                                    if screen.busy.is_none() {
                                        screen.clear_selection(cx);
                                        screen.error = None;
                                        cx.notify();
                                    }
                                })),
                        )
                    }),
            )
            .when(selected && ready, |view| {
                view.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(div().text_sm().child("Pairing code from this computer"))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .id("pairing-code-entry")
                                        .debug_selector(|| "pairing-code-entry".to_string())
                                        .w(px(384.))
                                        .child(self.code_input.clone()),
                                )
                                .child(
                                    widgets::primary_button("pair-selected")
                                        .aria_label("Pair computer")
                                        .child("Pair computer")
                                        .on_click(
                                            cx.listener(|screen, _, _, cx| {
                                                screen.pair_selected(cx)
                                            }),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(
                                    "Type or paste its eight-character code. The dash is optional.",
                                ),
                        ),
                )
            })
    }

    fn render_paired(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().pt_2().pb_1().text_lg().child("Paired computers"))
            .children(
                self.status
                    .iter()
                    .flat_map(|status| status.peers.iter())
                    .map(|(key, peer)| self.render_computer(key, peer, window, cx)),
            )
    }

    /// One paired computer: who it is, whether it is current, and the one
    /// action its state calls for. Nickname, status checks and Unpair live
    /// in its overflow menu.
    fn render_computer(
        &self,
        key: &str,
        peer: &maple_agent::store::records::RemotePeer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let name = computer_name(
            key,
            &peer.name,
            self.names.get(key).map_or("", String::as_str),
        );
        let editing = self
            .renaming
            .as_ref()
            .filter(|(target, _)| target == key)
            .map(|(_, input)| input.clone());
        let update = self.computer_updates.get(key).filter(|_| peer.can_connect);
        let loading = update.is_some_and(|update| update.phase == PeerInfoPhase::Loading);
        let (badge, status_line, mut note, action) = match update {
            Some(update) => (
                update.badge,
                update.status_line.clone(),
                update.note.clone(),
                update.action,
            ),
            None if peer.can_connect => {
                (None, "Version unknown".to_owned(), None, CardAction::None)
            }
            None => (
                None,
                if peer.controls_this_computer {
                    "Can use this computer’s projects and tools"
                } else {
                    "Pairing needs confirmation"
                }
                .to_owned(),
                None,
                CardAction::None,
            ),
        };
        let can_install = self.can_install(key);
        if note.is_none()
            && self.install_grant(key).is_none()
            && update.is_some_and(|update| {
                update.phase == PeerInfoPhase::Loaded
                    && update
                        .info
                        .as_ref()
                        .is_some_and(PeerInfo::supports_remote_updates)
            })
        {
            note = Some((
                crate::remote::update::BlockedReason::PairingConfirmationRequired
                    .message()
                    .to_owned(),
                Tone::Muted,
            ));
        }

        let renaming = editing.is_some();
        let title = match editing {
            Some(input) => div()
                .id(SharedString::from(format!("rename-editor-{key}")))
                .debug_selector({
                    let key = key.to_owned();
                    move || format!("rename-editor-{key}")
                })
                .flex()
                .items_center()
                .gap_2()
                .child(widgets::input_frame().flex_1().min_w_0().child(input))
                .child(
                    widgets::secondary_button(SharedString::from(format!("save-name-{key}")))
                        .aria_label("Save name")
                        .child("Save")
                        .on_click(cx.listener(|screen, _, _, cx| screen.save_name(cx))),
                )
                .child(
                    widgets::ghost_button(SharedString::from(format!("cancel-name-{key}")))
                        .aria_label("Cancel rename")
                        .child("Cancel")
                        .on_click(cx.listener(|screen, _, _, cx| {
                            screen.renaming = None;
                            screen.error = None;
                            cx.notify();
                        })),
                ),
            None => div()
                .id(SharedString::from(format!("computer-title-{key}")))
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(gpui::rgb(theme::text_primary()))
                        .child(name.clone()),
                )
                .when_some(badge, |row, (label, tone)| {
                    row.child(widgets::status_badge(label, tone.color()))
                }),
        };
        let details = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(title)
            .child(
                div()
                    .id(SharedString::from(format!("computer-status-{key}")))
                    .debug_selector({
                        let key = key.to_owned();
                        move || format!("computer-status-{key}")
                    })
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(status_line),
            )
            .when_some(note, |details, (note, tone)| {
                details.child(
                    div()
                        .id(SharedString::from(format!("computer-note-{key}")))
                        .debug_selector({
                            let key = key.to_owned();
                            move || format!("computer-note-{key}")
                        })
                        .text_xs()
                        .text_color(gpui::rgb(tone.color()))
                        .child(note),
                )
            });

        let install = key.to_owned();
        let check = key.to_owned();
        let confirm = key.to_owned();
        let confirming = self
            .install_confirmation
            .as_ref()
            .is_some_and(|confirmation| confirmation.peer == key);
        // While the name is edited, its Save and Cancel are the only actions.
        let actions = (!renaming).then(|| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_2()
                .when(
                    action == CardAction::Update && can_install && !confirming,
                    |row| {
                        row.child(
                            widgets::primary_button(SharedString::from(format!(
                                "update-remote-{key}"
                            )))
                            .debug_selector({
                                let key = key.to_owned();
                                move || format!("update-remote-{key}")
                            })
                            .aria_label(format!("Update {name}"))
                            .child("Update")
                            .on_click(cx.listener(
                                move |screen, _, window, cx| {
                                    screen.ask_remote_install(install.clone(), window, cx)
                                },
                            )),
                        )
                    },
                )
                .when(action == CardAction::CheckAgain && !loading, |row| {
                    row.child(
                        widgets::ghost_button(SharedString::from(format!("check-again-{key}")))
                            .debug_selector({
                                let key = key.to_owned();
                                move || format!("check-again-{key}")
                            })
                            .aria_label("Check this computer for updates again")
                            .child("Check again")
                            .on_click(cx.listener(move |screen, _, _, cx| {
                                screen.check_peer_update(check.clone(), cx)
                            })),
                    )
                })
                .when(peer.pending_confirmation.is_some(), |row| {
                    row.child(
                        widgets::secondary_button(SharedString::from(format!("confirm-{key}")))
                            .aria_label("Confirm previous pairing")
                            .child("Confirm pairing")
                            .on_click(cx.listener(move |screen, _, _, cx| {
                                screen.action("remote_reconcile", serde_json::json!([confirm]), cx)
                            })),
                    )
                })
                .child(self.render_computer_menu(key, &name, peer.can_connect, loading, window, cx))
        });

        widgets::card_row()
            .id(SharedString::from(format!("computer-{key}")))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(36.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::RADIUS_SM)
                            .bg(gpui::rgb(theme::bg_sidebar_card()))
                            .border_1()
                            .border_color(gpui::rgb(theme::border_subtle()))
                            .child(icon("monitor", widgets::ROW_ICON, theme::text_secondary())),
                    )
                    .child(details)
                    .children(actions),
            )
            .when_some(
                self.install_confirmation
                    .as_ref()
                    .filter(|confirmation| confirmation.peer == key),
                |card, confirmation| card.child(self.render_install_confirmation(confirmation, cx)),
            )
    }

    fn render_computer_menu(
        &self,
        key: &str,
        name: &str,
        can_connect: bool,
        loading: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let menu_key = key.to_owned();
        let open = self.popup.is_open(&menu_key);
        let menu = open.then(|| {
            let check = key.to_owned();
            let rename = key.to_owned();
            let rename_name = name.to_owned();
            let revoke = key.to_owned();
            let mut menu = Menu::new(SharedString::from(format!("computer-menu-{key}")), px(200.))
                .label(format!("{name} options"));
            if can_connect {
                menu = menu.item(
                    MenuItem::new(
                        SharedString::from(format!("refresh-update-{key}")),
                        "Check for updates",
                        move |screen: &mut Self, _, cx| screen.check_peer_update(check.clone(), cx),
                    )
                    .icon("download")
                    .enabled(!loading),
                );
            }
            menu = menu
                .item(
                    MenuItem::new(
                        SharedString::from(format!("rename-{key}")),
                        "Rename…",
                        move |screen: &mut Self, window, cx| {
                            screen.rename(rename.clone(), rename_name.clone(), cx);
                            if let Some((_, input)) = &screen.renaming {
                                window.focus(&input.focus_handle(cx), cx);
                            }
                        },
                    )
                    .icon("pencil"),
                )
                .separator()
                .item(
                    MenuItem::new(
                        SharedString::from(format!("revoke-{key}")),
                        "Unpair",
                        move |screen: &mut Self, _, cx| {
                            screen.action("remote_revoke", serde_json::json!([revoke]), cx)
                        },
                    )
                    .icon("trash-2"),
                );
            self.popup.render(menu, Placement::BelowEnd, window, cx)
        });
        let toggle = menu_key.clone();
        div()
            .relative()
            .flex_none()
            .child(
                self.popup.trigger(
                    menu_key,
                    widgets::icon_button(
                        SharedString::from(format!("computer-more-{key}")),
                        "ellipsis",
                        "More",
                        px(16.),
                        theme::text_secondary(),
                    )
                    .debug_selector({
                        let key = key.to_owned();
                        move || format!("computer-more-{key}")
                    }),
                    cx,
                    move |screen, _, cx| {
                        screen.popup.toggle(toggle.clone(), cx);
                    },
                ),
            )
            .children(menu)
    }

    fn render_install_confirmation(
        &self,
        confirmation: &InstallConfirmation,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        div()
            .id("remote-update-confirmation")
            .debug_selector(|| "remote-update-confirmation".into())
            .role(gpui::Role::Dialog)
            .aria_label("Experimental remote update")
            .key_context("Dialog")
            .w_full()
            .min_w_0()
            .p_4()
            .rounded(theme::RADIUS_MD)
            .bg(gpui::rgb(theme::bg_sidebar_card()))
            .border_1()
            .border_color(gpui::rgb(theme::border_subtle()))
            .flex()
            .flex_col()
            .gap_3()
            .on_key_down(
                cx.listener(|screen, event: &gpui::KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "escape" => screen.cancel_remote_install(window, cx),
                        "tab" => {
                            if let Some(confirmation) = &screen.install_confirmation {
                                let focus = if confirmation.install_focus.is_focused(window) {
                                    &confirmation.cancel_focus
                                } else {
                                    &confirmation.install_focus
                                };
                                window.focus(focus, cx);
                            }
                        }
                        "enter" | "space" | " " if !event.keystroke.modifiers.modified() => {
                            if screen
                                .install_confirmation
                                .as_ref()
                                .is_some_and(|confirmation| {
                                    confirmation.install_focus.is_focused(window)
                                })
                            {
                                screen.confirm_remote_install(window, cx);
                            } else {
                                screen.cancel_remote_install(window, cx);
                            }
                        }
                        _ => return,
                    }
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(confirmation.title.clone()),
                    )
                    .child(widgets::status_badge(
                        "Experimental",
                        theme::status_warning(),
                    )),
            )
            .child(
                div()
                    .id("remote-update-warning")
                    .debug_selector(|| "remote-update-warning".into())
                    .w_full()
                    .min_w_0()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(confirmation.detail.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .justify_end()
                    .child(
                        widgets::secondary_button("cancel-remote-update")
                            .debug_selector(|| "cancel-remote-update".into())
                            .track_focus(&confirmation.cancel_focus)
                            .focus_visible(widgets::focus_ring)
                            .child("Cancel")
                            .on_click(cx.listener(|screen, _, window, cx| {
                                screen.cancel_remote_install(window, cx)
                            })),
                    )
                    .child(
                        widgets::primary_button("confirm-remote-update")
                            .debug_selector(|| "confirm-remote-update".into())
                            .aria_label(confirmation.action_label.clone())
                            .track_focus(&confirmation.install_focus)
                            .focus_visible(widgets::focus_ring)
                            .child("Update")
                            .on_click(cx.listener(|screen, _, window, cx| {
                                screen.confirm_remote_install(window, cx)
                            })),
                    ),
            )
    }
}

fn pairing_code_boxes(code: &str) -> Div {
    div()
        .flex()
        .gap(px(6.))
        .items_center()
        .children(code.chars().map(|character| {
            let cell = div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(48.))
                .text_xl()
                .font_family(crate::assets::FONT_MONO)
                .child(character.to_string());
            if character == '-' {
                cell.w(px(16.)).text_color(gpui::rgb(theme::text_muted()))
            } else {
                cell.w(px(40.))
                    .rounded(theme::RADIUS_MD)
                    .bg(gpui::rgb(theme::bg_input()))
                    .border_1()
                    .border_color(gpui::rgb(theme::border()))
            }
        }))
}
