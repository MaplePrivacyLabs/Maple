//! The gpui desktop window: login/chat routing, window state, and the
//! event pump that forwards agent service events into the active chat
//! screen. Compiled only with the `desktop` feature.

use std::{path::PathBuf, sync::Arc};

use gpui::{
    App, Bounds, Context, Entity, Render, Window, WindowBounds, WindowOptions, actions, div, point,
    prelude::*, px, size,
};

actions!(maple_app, [QuitApp, Hide, HideOthers, ShowAll]);

use crate::backend::AgentBackend;
use crate::ui;
use crate::ui::chat::computers::{
    Computer, Destination, Navigate, NavigationChanged, ProjectRenameRequest, ProjectTarget,
};
use crate::ui::chat::{ChatScreen, LoggedOut, SignOutFailed};
use crate::ui::login::{LoginScreen, LoginSucceeded};
use crate::ui::settings::{
    AccountDeleted, Section, SettingsClosed, SettingsScreen, ShortcutSettingsChange,
    ShortcutSettingsRequested, SignOutRequested,
};
use crate::ui::titlebar::TitleBar;

mod remote_updates;

enum Screen {
    /// Construction placeholder. `run` replaces it before the first paint
    /// with the chat screen (saved sign-in) or the login form.
    Restoring,
    StoreGate(Entity<ui::store_gate::StoreGate>),
    Login(Entity<LoginScreen>),
    Chat(Entity<ChatScreen>),
    Settings(Entity<SettingsScreen>),
}

/// One cache and one initial connection attempt per paired endpoint. Once
/// attached, the existing IPC/Iroh supervisor owns reconnects and snapshots.
struct RemoteComputer {
    name: String,
    grant: [u8; 16],
    chat: Option<Entity<ChatScreen>>,
    attempt: ConnectionAttempt,
}

#[derive(Clone)]
struct PendingNavigation {
    request: Navigate,
    source: Option<(gpui::EntityId, u64)>,
}

/// A rename belongs to the owner and pairing that admitted it, regardless of
/// which computer the user is viewing when its response arrives.
struct PendingProjectRename {
    target: ProjectTarget,
    account: String,
    owner: gpui::EntityId,
    grant: Option<[u8; 16]>,
    source: gpui::WeakEntity<ChatScreen>,
}

enum ConnectionAttempt {
    Idle,
    Connecting(u64),
    Failed {
        message: String,
        retry_at: std::time::Instant,
    },
}
impl RemoteComputer {
    fn should_connect(&self, now: std::time::Instant) -> bool {
        self.chat.is_none()
            && match &self.attempt {
                ConnectionAttempt::Idle => true,
                ConnectionAttempt::Connecting(_) => false,
                ConnectionAttempt::Failed { message, retry_at } => {
                    !crate::remote::wire::is_protocol_incompatibility_refusal(message)
                        && now >= *retry_at
                }
            }
    }
    fn error(&self) -> Option<String> {
        match &self.attempt {
            ConnectionAttempt::Failed { message, .. } => Some(
                if crate::remote::wire::is_protocol_incompatibility_refusal(message) {
                    "Update required — select to retry after updating".into()
                } else {
                    message.clone()
                },
            ),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StoreAttentionKind {
    Operation,
    Persistence,
}

struct StoreAttention {
    kind: StoreAttentionKind,
    message: String,
    retry: bool,
    store_id: Option<String>,
    epoch: u64,
}

struct MapleApp {
    backend: Arc<AgentBackend>,
    screen: Screen,
    user_id: Option<String>,
    /// The chat screen is parked while settings is open so Back returns to
    /// it with its state intact.
    parked_chat: Option<Entity<ChatScreen>>,
    local_chat: Option<Entity<ChatScreen>>,
    selected_host: Option<String>,
    remote_computers: std::collections::BTreeMap<String, RemoteComputer>,
    /// An empty startup projection is provisional until this account's first
    /// paired-computer status arrives. It must not replace saved grouping.
    remote_inventory_loaded: bool,
    next_connection_attempt: u64,
    requested_navigation: Option<PendingNavigation>,
    computers_sync_pending: bool,
    remote_enabled: Arc<std::sync::atomic::AtomicBool>,
    store_resident: Arc<std::sync::atomic::AtomicBool>,
    store_load_epoch: u64,
    store_ready: bool,
    store_id: Option<String>,
    store_notice: Option<String>,
    store_notice_is_error: bool,
    seen_store_notice_operation: Option<String>,
    inline_password_completion: Option<String>,
    mode_change_notice: Option<ModeChangeNotice>,
    inline_mode_notice: Option<ModeChangeNotice>,
    password_settings_refresh_pending: bool,
    password_settings_refreshing: bool,
    persistence_status: maple_agent::store::PersistenceStatus,
    persistence_retrying: bool,
    #[cfg(feature = "proxy")]
    local_api_revision: u64,
    #[cfg(feature = "proxy")]
    local_api_host_epoch: u64,
    #[cfg(feature = "proxy")]
    local_api_refreshing: bool,
    #[cfg(feature = "proxy")]
    local_api_refresh_again: bool,
    pending_store_attention: std::collections::VecDeque<StoreAttention>,
    store_attention_open: bool,
    restore_window_geometry: bool,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    tray: Option<crate::tray::StatusIcon>,
    remote_bridges: std::cell::RefCell<Vec<gpui::Task<()>>>,
    settings: crate::settings::AppSettings,
    updater: crate::update::Controller,
    restart_prompt_pending: bool,
    #[cfg(target_os = "linux")]
    restart_confirmation: Option<Entity<ui::update_confirmation::UpdateConfirmation>>,
    /// The complete live shortcut map. Settings mutations prepare a full
    /// replacement before this runtime clears GPUI's process-wide bindings.
    shortcuts: crate::shortcuts::ShortcutRuntime,
    /// Created once; re-creating it per render leaked an entity per frame.
    titlebar: Entity<TitleBar>,
}

impl MapleApp {
    fn apply_store_completion_notice(
        &mut self,
        status: &DesktopStoreStatus,
        cx: &mut Context<Self>,
    ) {
        if self.store_id.is_some() && status.store_id.is_some() && self.store_id != status.store_id
        {
            // A local form can be queued while another client replaces the
            // store. Never reattach its old pending edits to that generation.
            crate::settings::discard_decrypted_cache();
            self.inline_mode_notice = None;
            self.inline_password_completion = None;
            self.store_id = status.store_id.clone();
            self.screen = Screen::Restoring;
            if status.state == maple_agent::store::StoreState::Open {
                self.clear_store_ui(cx);
                self.load_open_store(cx);
            } else {
                self.show_store_state(status.state.clone(), cx);
            }
            return;
        }
        if self.store_id.is_none() {
            self.store_id = status.store_id.clone();
        }
        self.apply_persistence_status(status.persistence_status.clone(), cx);
        let pending_operation = match &self.screen {
            Screen::Settings(settings) => settings
                .read(cx)
                .pending_password_operation_id(cx)
                .map(str::to_owned),
            _ => None,
        };
        let inline_recipient =
            status.matches_pending_password_operation(pending_operation.as_deref());
        let notice = status
            .current_mode_notice()
            .filter(|notice| self.inline_mode_notice.as_ref() != Some(*notice))
            .cloned();
        let defer_mode_notice = inline_recipient
            && notice.as_ref().is_some_and(|notice| {
                status
                    .completion_notice
                    .as_ref()
                    .and_then(|completion| completion.mode_notice_id.as_deref())
                    == Some(notice.notice_id.as_str())
            });
        if !defer_mode_notice && self.mode_change_notice != notice {
            self.mode_change_notice = notice;
            cx.notify();
        }
        if status.reconcile_password_completion_notice(
            &mut self.seen_store_notice_operation,
            &mut self.store_notice,
            &mut self.store_notice_is_error,
            inline_recipient,
            self.inline_password_completion.as_deref(),
        ) {
            if let Some(message) = self.store_notice.clone() {
                self.present_store_attention(message, StoreAttentionKind::Operation, false, cx);
            }
            cx.notify();
        }
    }

    /// Operational feedback belongs to its form. If that recipient is gone,
    /// use a proper window dialog for an attention-worthy result; never insert
    /// operation messages into the application's page chrome.
    fn present_store_attention(
        &mut self,
        message: String,
        kind: StoreAttentionKind,
        retry: bool,
        cx: &mut Context<Self>,
    ) {
        match &self.screen {
            Screen::StoreGate(gate)
                if self.backend.store_state() != maple_agent::store::StoreState::Open =>
            {
                gate.update(cx, |gate, cx| gate.show_local_store_notice(message, cx));
                return;
            }
            Screen::Settings(settings)
                if settings.update(cx, |settings, cx| {
                    settings.show_local_store_notice(message.clone(), cx)
                }) =>
            {
                return;
            }
            _ => {}
        }
        // Preserve the latest result from each independent latch. Deduplication
        // by the two kinds bounds this queue without one source hiding another.
        let attention = StoreAttention {
            kind,
            message,
            retry,
            store_id: self.store_id.clone(),
            epoch: self.store_load_epoch,
        };
        if let Some(pending) = self
            .pending_store_attention
            .iter_mut()
            .find(|pending| pending.kind == kind)
        {
            *pending = attention;
        } else {
            self.pending_store_attention.push_back(attention);
        }
        cx.notify();
    }

    fn present_pending_store_attention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store_attention_open {
            return;
        }
        let attention = loop {
            let Some(attention) = self.pending_store_attention.pop_front() else {
                return;
            };
            if self.attention_matches_current_store(&attention) {
                break attention;
            }
        };
        let prompt = window.prompt(
            gpui::PromptLevel::Warning,
            if attention.retry {
                "Maple couldn't save local data"
            } else {
                "Local data needs attention"
            },
            Some(&attention.message),
            if attention.retry {
                &["Retry saving", "Keep viewing"]
            } else {
                &["OK"]
            },
            cx,
        );
        self.store_attention_open = true;
        cx.spawn(async move |this, cx| {
            let response = prompt.await;
            let _ = this.update(cx, |this, cx| {
                this.store_attention_open = false;
                if this.attention_matches_current_store(&attention) {
                    if response.is_err()
                        && !this
                            .pending_store_attention
                            .iter()
                            .any(|pending| pending.kind == attention.kind)
                    {
                        // Closing a window before delivery does not consume
                        // the warning. A later foreground render can retry it.
                        this.pending_store_attention.push_front(attention);
                    } else if response == Ok(0) && attention.retry {
                        this.retry_persistence(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn attention_matches_current_store(&self, attention: &StoreAttention) -> bool {
        if attention.store_id.is_some() {
            attention.store_id == self.store_id
        } else {
            self.store_id.is_none() && attention.epoch == self.store_load_epoch
        }
    }

    fn password_settings_finished(
        &mut self,
        event: &ui::store_gate::PasswordFormFinished,
        cx: &mut Context<Self>,
    ) {
        if event.cancelled {
            return;
        }
        if event.notice.is_none() {
            if let Some(operation) = &event.completion_operation_id {
                self.inline_password_completion = Some(operation.clone());
                if self.seen_store_notice_operation.as_ref() == Some(operation)
                    && !self.store_notice_is_error
                {
                    self.store_notice = None;
                }
            }
            if event.completion_operation_id.is_some()
                && let Some(notice) = &event.mode_notice
                && self.store_id.as_deref() == Some(notice.store_id.as_str())
            {
                self.acknowledge_inline_mode_notice(notice.clone(), cx);
            }
        }
        self.refresh_password_settings(cx);
        cx.notify();
    }

    fn acknowledge_inline_mode_notice(&mut self, notice: ModeChangeNotice, cx: &mut Context<Self>) {
        self.inline_mode_notice = Some(notice.clone());
        if self.mode_change_notice.as_ref() == Some(&notice) {
            self.mode_change_notice = None;
        }
        let backend = self.backend.clone();
        let requested = notice.clone();
        let acknowledgement = backend.clone().spawn(async move {
            backend
                .local_control::<bool>(
                    "acknowledge_mode_change_notice",
                    (&requested.store_id, &requested.notice_id),
                )
                .await
        });
        cx.spawn(async move |this, cx| {
            let result = acknowledgement.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_inline_mode_notice_ack(notice, matches!(result, Ok(Ok(true))), cx);
            });
        })
        .detach();
    }

    fn finish_inline_mode_notice_ack(
        &mut self,
        notice: ModeChangeNotice,
        acknowledged: bool,
        cx: &mut Context<Self>,
    ) {
        if acknowledged && let Screen::Settings(settings) = &self.screen {
            settings.update(cx, |settings, cx| {
                settings.record_mode_notice_ack(&notice, cx)
            });
        }
        if !acknowledged && self.inline_mode_notice.as_ref() == Some(&notice) {
            self.inline_mode_notice = None;
            if self.store_id.as_deref() == Some(notice.store_id.as_str())
                && self.backend.store_state() == maple_agent::store::StoreState::Open
                && self
                    .mode_change_notice
                    .as_ref()
                    .is_none_or(|current| current == &notice)
            {
                self.mode_change_notice = Some(notice);
            }
        }
        // A failed stale acknowledgement never replaces a newer known notice.
        cx.notify();
    }

    fn refresh_password_settings(&mut self, cx: &mut Context<Self>) {
        self.password_settings_refresh_pending = true;
        if self.password_settings_refreshing
            || self.backend.store_state() != maple_agent::store::StoreState::Open
        {
            return;
        }
        self.password_settings_refresh_pending = false;
        self.password_settings_refreshing = true;
        let epoch = self.store_load_epoch;
        let previous_store = self.store_id.clone();
        let previous_account = self.user_id.clone();
        let backend = self.backend.clone();
        let load = backend.clone().spawn(async move {
            let (settings, status) = tokio::join!(
                backend.get_ui_settings(),
                backend.local_control::<DesktopStoreStatus>("store_status", ())
            );
            settings.and_then(|settings| status.map(|status| (settings, status)))
        });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            let _ = this.update(cx, |this, cx| {
                this.password_settings_refreshing = false;
                if this.store_load_epoch != epoch || this.user_id != previous_account {
                    return;
                }
                if let Ok(Ok((mut settings, status))) = result {
                    if this.backend.store_state() != maple_agent::store::StoreState::Open
                        || status.state != maple_agent::store::StoreState::Open
                    {
                        this.password_settings_refresh_pending = true;
                        return;
                    }
                    if previous_store.is_some() && previous_store != status.store_id {
                        this.apply_store_completion_notice(&status, cx);
                        return;
                    }
                    this.store_id = status.store_id.clone();
                    settings.store_password_mode = status.mode != "plain";
                    crate::settings::attach_backend(this.backend.clone(), settings);
                    this.apply_refreshed_password_settings(crate::settings::load_settings(), cx);
                    this.apply_store_completion_notice(&status, cx);
                    this.set_store_resident(status.keeps_resident(), cx);
                    if let Screen::Settings(settings) = &this.screen {
                        settings.update(cx, |settings, cx| settings.refresh_encryption_status(cx));
                    }
                    if let Some(chat) = &this.local_chat {
                        chat.update(cx, |chat, cx| chat.end_store_maintenance(cx));
                    }
                    this.load_dictionary(epoch, cx);
                    cx.notify();
                } else {
                    // Open may have reached the event stream after the reply,
                    // or a connection may have changed during this refresh.
                    this.password_settings_refresh_pending = true;
                }
            });
        })
        .detach();
    }

    fn apply_refreshed_password_settings(
        &mut self,
        settings: crate::settings::AppSettings,
        cx: &mut Context<Self>,
    ) {
        // Account presentation state has a separate owner API. A global
        // settings refresh must not discard its hydrated nicknames.
        let remote_sidebars = std::mem::take(&mut self.settings.remote_sidebars);
        self.settings = settings;
        self.settings.remote_sidebars = remote_sidebars;
        if let Screen::Settings(screen) = &self.screen {
            screen.update(cx, |screen, cx| {
                screen.refresh_store_password_mode(self.settings.store_password_mode, cx)
            });
        }
        // Parked and remote views can still emit notifications while Settings
        // is open. Apply the authoritative mode before either resumes work.
        for chat in self.local_chat.iter().chain(
            self.remote_computers
                .values()
                .filter_map(|computer| computer.chat.as_ref()),
        ) {
            chat.update(cx, |chat, _| {
                chat.apply_notification_settings(&self.settings)
            });
        }
    }

    fn verify_current_store_state(&mut self, cx: &mut Context<Self>) {
        crate::settings::clear_decrypted_cache();
        ui::spell::clear_user_words();
        self.password_settings_refresh_pending = true;
        let epoch = self.store_load_epoch;
        let backend = self.backend.clone();
        let query = backend.clone().spawn(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                backend.local_control::<DesktopStoreStatus>("store_status", ()),
            )
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                if this.store_load_epoch != epoch {
                    return;
                }
                if let Ok(Ok(Ok(status))) = result {
                    this.apply_store_completion_notice(&status, cx);
                    if this.store_load_epoch == epoch {
                        this.show_store_state(status.state, cx);
                    }
                } else {
                    this.show_store_state(maple_agent::store::StoreState::Maintenance, cx);
                }
            });
        })
        .detach();
    }

    fn apply_persistence_status(
        &mut self,
        status: maple_agent::store::PersistenceStatus,
        cx: &mut Context<Self>,
    ) {
        if self.store_id.is_some()
            && status.store_id != self.store_id
            && (status.store_id.is_some()
                || self.backend.store_state() == maple_agent::store::StoreState::Open)
        {
            return;
        }
        if self.persistence_status.store_id == status.store_id
            && self.persistence_status.revision > status.revision
        {
            return;
        }
        if self.persistence_status != status {
            let new_failure = status
                .failure
                .as_ref()
                .filter(|failure| {
                    self.persistence_status
                        .failure
                        .as_ref()
                        .is_none_or(|previous| {
                            previous.store_id != failure.store_id
                                || previous.reason != failure.reason
                        })
                })
                .cloned();
            self.persistence_status = status;
            if let Screen::Settings(settings) = &self.screen {
                settings.update(cx, |settings, cx| settings.refresh_encryption_status(cx));
            }
            if let Some(failure) = new_failure {
                let open = self.backend.store_state() == maple_agent::store::StoreState::Open;
                self.present_store_attention(
                    persistence_feedback_message(&failure, open).into(),
                    StoreAttentionKind::Persistence,
                    open,
                    cx,
                );
            }
            cx.notify();
        }
    }

    fn retry_persistence(&mut self, cx: &mut Context<Self>) {
        if self.persistence_retrying
            || self.backend.store_state() != maple_agent::store::StoreState::Open
        {
            return;
        }
        self.persistence_retrying = true;
        let backend = self.backend.clone();
        let retry = backend.clone().spawn(async move {
            crate::settings::flush_pending().await?;
            backend
                .local_control::<()>("retry_store_persistence", ())
                .await?;
            backend
                .local_control::<DesktopStoreStatus>("store_status", ())
                .await
        });
        cx.spawn(async move |this, cx| {
            let result = retry.await;
            let _ = this.update(cx, |this, cx| {
                this.persistence_retrying = false;
                if let Ok(Ok(status)) = result {
                    this.apply_store_completion_notice(&status, cx);
                }
                // An error leaves the contextual save-failure latch visible.
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Forward a batch of backend service events to the chat screen when
    /// one exists, parked behind Settings included, so a run that ends
    /// meanwhile shows ended on return. One batch is one render, however
    /// many events arrived.
    fn handle_service_events(
        &mut self,
        events: Vec<maple_agent::agent::AgentServiceEvent>,
        cx: &mut Context<Self>,
    ) {
        let mut batch = Vec::new();
        for event in events {
            #[cfg(feature = "proxy")]
            if matches!(
                event,
                maple_agent::agent::AgentServiceEvent::LocalApiChanged
            ) {
                self.refresh_local_api(cx);
                continue;
            }
            if let maple_agent::agent::AgentServiceEvent::PersistenceFailureChanged { status } =
                &event
            {
                self.apply_persistence_status(status.clone(), cx);
                // The local (possibly parked) chat owns the persistent Retry
                // control beside its composer as well as the desktop alert.
                batch.push(event);
            } else if matches!(
                &event,
                maple_agent::agent::AgentServiceEvent::HostReconnected
            ) {
                // A restarted host has a new in-memory revision baseline.
                self.persistence_status.revision = 0;
                #[cfg(feature = "proxy")]
                {
                    self.local_api_revision = 0;
                    self.local_api_host_epoch = self.local_api_host_epoch.wrapping_add(1);
                    self.local_api_refreshing = false;
                    self.local_api_refresh_again = false;
                    if let Screen::Settings(settings) = &self.screen {
                        settings.update(cx, |settings, cx| settings.local_api_host_reconnected(cx));
                    }
                    self.refresh_local_api(cx);
                }
                self.verify_current_store_state(cx);
                batch.push(event);
            } else if let maple_agent::agent::AgentServiceEvent::StoreStateChanged { state } =
                &event
            {
                self.forward_service_events(std::mem::take(&mut batch), cx);
                if *state == maple_agent::store::StoreState::Maintenance {
                    self.pause_local_chat_for_store_maintenance(cx);
                }
                if *state == maple_agent::store::StoreState::Maintenance
                    && matches!(&self.screen, Screen::Settings(_))
                {
                    self.verify_current_store_state(cx);
                } else {
                    self.show_store_state(state.clone(), cx);
                }
            } else if let maple_agent::agent::AgentServiceEvent::AccountChanged(user_id) = &event {
                // Deliver the old account's queued frames only to its screen.
                // A reconnect may discover a sign-out/account change while away.
                self.forward_service_events(std::mem::take(&mut batch), cx);
                if self.store_ready
                    && self.backend.store_state() == maple_agent::store::StoreState::Open
                    && &self.user_id != user_id
                {
                    match user_id {
                        Some(user_id) => self.open_chat(user_id.clone(), cx),
                        None => self.show_login(cx),
                    }
                }
            } else {
                batch.push(event);
            }
        }
        self.forward_service_events(batch, cx);
    }

    fn clear_store_ui(&mut self, cx: &mut Context<Self>) {
        self.store_load_epoch = self.store_load_epoch.wrapping_add(1);
        self.password_settings_refresh_pending = false;
        self.store_ready = false;
        self.updater.set_auto_download(false);
        crate::settings::clear_decrypted_cache();
        ui::spell::clear_user_words();
        self.settings = crate::settings::AppSettings::default();
        ui::theme::apply_preference(ui::theme::Preference::parse(&self.settings.theme), cx);
        ui::typography::apply(
            ui::typography::ChatFontFamily::parse(&self.settings.chat_font_family),
            self.settings.chat_font_size,
        );
        cx.set_reduce_motion(self.settings.reduce_motion);
        if let Err(error) = self
            .shortcuts
            .replace(&self.settings.shortcut_overrides, cx)
        {
            log::warn!("Could not reset locked-store shortcuts: {error}");
        }
        self.parked_chat = None;
        self.local_chat = None;
        self.remote_computers.clear();
        self.remote_inventory_loaded = false;
        self.requested_navigation = None;
        self.selected_host = None;
        self.user_id = None;
    }

    fn pause_local_chat_for_store_maintenance(&mut self, cx: &mut Context<Self>) {
        if let Some(chat) = &self.local_chat {
            chat.update(cx, |chat, cx| chat.begin_store_maintenance(cx));
        }
    }

    fn show_store_state(&mut self, state: maple_agent::store::StoreState, cx: &mut Context<Self>) {
        if state == maple_agent::store::StoreState::Open {
            if !self.store_ready {
                self.load_open_store(cx);
            } else if self.password_settings_refresh_pending {
                self.refresh_password_settings(cx);
            }
            return;
        }
        if state == maple_agent::store::StoreState::Maintenance
            && matches!(&self.screen, Screen::Settings(settings) if settings.read(cx).password_operation_busy(cx))
        {
            self.pause_local_chat_for_store_maintenance(cx);
            // This form owns the operation and already shows its progress.
            // Encryption export preserves the generation and account data;
            // locked/damaged/erased states still take the normal clearing path.
            self.set_store_resident(true, cx);
            self.password_settings_refresh_pending = true;
            return;
        }
        self.clear_store_ui(cx);
        self.set_store_resident(state == maple_agent::store::StoreState::Maintenance, cx);
        let gate = cx.new(|cx| ui::store_gate::StoreGate::new(self.backend.clone(), state, cx));
        cx.subscribe(
            &gate,
            |app, _, _event: &ui::store_gate::StoreGateChanged, cx| {
                let backend = app.backend.clone();
                let query = backend.clone().spawn(async move {
                    backend
                        .local_control::<DesktopStoreStatus>("store_status", ())
                        .await
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(Ok(status)) = query.await {
                        let _ = this.update(cx, |this, cx| {
                            this.apply_store_completion_notice(&status, cx);
                            this.show_store_state(status.state, cx);
                        });
                    }
                })
                .detach();
            },
        )
        .detach();
        self.screen = Screen::StoreGate(gate);
        cx.notify();
    }

    /// Settings bootstrap is a bounded host request. Dictionary pages load
    /// separately; no full account/project/task snapshot blocks first paint.
    fn load_open_store(&mut self, cx: &mut Context<Self>) {
        self.store_load_epoch = self.store_load_epoch.wrapping_add(1);
        let epoch = self.store_load_epoch;
        self.screen = Screen::Restoring;
        let backend = self.backend.clone();
        let load = backend.clone().spawn(async move {
            let (settings, status) = tokio::join!(
                backend.get_ui_settings(),
                backend.local_control::<DesktopStoreStatus>("store_status", ()),
            );
            settings.and_then(|settings| status.map(|status| (settings, status)))
        });
        cx.spawn(async move |this, cx| match load.await {
            Ok(Ok((settings, status))) => {
                let _ = this.update(cx, |this, cx| {
                    if this.store_load_epoch != epoch
                        || status.state != maple_agent::store::StoreState::Open
                        || this.backend.store_state() != maple_agent::store::StoreState::Open
                    {
                        return;
                    }
                    if this.store_id.is_some() && this.store_id != status.store_id {
                        crate::settings::discard_decrypted_cache();
                        ui::spell::clear_user_words();
                    }
                    this.store_id = status.store_id.clone();
                    this.apply_store_completion_notice(&status, cx);
                    crate::settings::attach_backend(this.backend.clone(), settings);
                    this.settings = crate::settings::load_settings();
                    this.store_ready = true;
                    let updater = this.updater.clone();
                    let automatic = this.settings.automatic_update_downloads;
                    updater.set_auto_download(automatic);
                    // Retain the previous app until current Open-store bootstrap
                    // succeeds; never use provisional download preferences.
                    drop(this.backend.spawn(async move {
                        updater.run().await;
                    }));
                    if std::mem::replace(&mut this.restore_window_geometry, false)
                        && let Some(saved) = this
                            .settings
                            .window
                            .clone()
                            .map(crate::settings::WindowState::clamped)
                    {
                        let root_id = cx.entity().entity_id();
                        for handle in cx.windows() {
                            let _ = handle.update(cx, |_, window, cx| {
                                if window
                                    .root::<MapleApp>()
                                    .flatten()
                                    .is_some_and(|root| root.entity_id() == root_id)
                                {
                                    #[cfg(target_os = "macos")]
                                    let placed = native_window::place(window, cx, &saved);
                                    #[cfg(not(target_os = "macos"))]
                                    let placed = {
                                        let _ = cx;
                                        false
                                    };
                                    if !placed {
                                        window.resize(size(px(saved.width), px(saved.height)));
                                    }
                                    if saved.maximized && !window.is_maximized() {
                                        window.zoom_window();
                                    }
                                }
                            });
                        }
                    }
                    this.set_store_resident(status.keeps_resident(), cx);
                    ui::theme::apply_preference(
                        ui::theme::Preference::parse(&this.settings.theme),
                        cx,
                    );
                    ui::typography::apply(
                        ui::typography::ChatFontFamily::parse(&this.settings.chat_font_family),
                        this.settings.chat_font_size,
                    );
                    cx.set_reduce_motion(this.settings.reduce_motion);
                    if let Err(error) = this
                        .shortcuts
                        .replace(&this.settings.shortcut_overrides, cx)
                    {
                        log::warn!("Could not load stored shortcuts: {error}");
                    }
                    match this.backend.saved_user_id() {
                        Some(user) => {
                            this.open_chat(user, cx);
                            let restore = this.backend.restore_in_background();
                            cx.spawn(async move |this, cx| {
                                if restore.await.ok()
                                    == Some(crate::backend::RestoreOutcome::Rejected)
                                {
                                    let _ = this.update(cx, |this, cx| {
                                        if this.store_load_epoch == epoch {
                                            this.show_login(cx);
                                        }
                                    });
                                }
                            })
                            .detach();
                        }
                        None => this.show_login(cx),
                    }
                    this.load_dictionary(epoch, cx);
                });
            }
            result => {
                log::warn!("Could not load Maple preferences: {result:?}");
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.store_load_epoch == epoch
                        && this.backend.store_state() == maple_agent::store::StoreState::Open
                    {
                        this.load_open_store(cx);
                    }
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn load_dictionary(&self, epoch: u64, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let load = backend.clone().spawn(async move {
            let mut words = Vec::new();
            let mut after = None;
            loop {
                let page = backend.dictionary(after, 200).await?;
                words.extend(page.words);
                match page.next {
                    Some(next) => after = Some(next),
                    None => return Ok::<_, String>(words),
                }
            }
        });
        cx.spawn(async move |this, cx| match load.await {
            Ok(Ok(words)) => {
                let _ = this.update(cx, |this, _| {
                    if this.store_load_epoch == epoch && this.store_ready {
                        ui::spell::install_user_words(words);
                    }
                });
            }
            result => log::warn!("Could not load Maple dictionary: {result:?}"),
        })
        .detach();
    }

    fn set_store_resident(&mut self, resident: bool, cx: &mut Context<Self>) {
        self.store_resident
            .store(resident, std::sync::atomic::Ordering::Release);
        self.update_tray();
        cx.notify();
    }

    #[cfg(feature = "proxy")]
    fn refresh_local_api(&mut self, cx: &mut Context<Self>) {
        if self.local_api_refreshing {
            self.local_api_refresh_again = true;
            return;
        }
        self.local_api_refreshing = true;
        let backend = self.backend.clone();
        let epoch = self.store_load_epoch;
        let host_epoch = self.local_api_host_epoch;
        let bridge = ui::task::call(
            &self.backend,
            async move {
                let status = backend.get_local_api_status().await?;
                let store = backend
                    .local_control::<DesktopStoreStatus>("store_status", ())
                    .await?;
                Ok((status, store))
            },
            cx,
            move |app, result, cx| {
                if app.local_api_host_epoch != host_epoch {
                    return;
                }
                app.local_api_refreshing = false;
                if app.store_load_epoch == epoch
                    && let Ok((status, store)) = result
                    && status.revision >= app.local_api_revision
                {
                    app.local_api_revision = status.revision;
                    app.set_store_resident(store.keeps_resident(), cx);
                    if let Screen::Settings(settings) = &app.screen {
                        settings
                            .update(cx, |settings, cx| settings.set_local_api_status(status, cx));
                    }
                }
                if std::mem::take(&mut app.local_api_refresh_again) {
                    app.refresh_local_api(cx);
                }
            },
        );
        ui::task::retain(&self.remote_bridges, bridge);
    }

    fn update_tray(&self) {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        if let Some(tray) = &self.tray {
            tray.set_state(
                self.remote_enabled
                    .load(std::sync::atomic::Ordering::Acquire),
                self.store_resident
                    .load(std::sync::atomic::Ordering::Acquire),
            );
        }
    }

    fn forward_service_events(
        &mut self,
        events: Vec<maple_agent::agent::AgentServiceEvent>,
        cx: &mut Context<Self>,
    ) {
        if let Some(chat) = &self.local_chat {
            chat.update(cx, |chat, cx| chat.handle_service_events(events, cx));
        }
    }

    fn set_update(&mut self, snapshot: crate::update::Snapshot, cx: &mut Context<Self>) {
        if self.updater.restart_path().is_some() {
            // No old chat/settings controls remain actionable after the host
            // stop and replacement. Backend admission is retired independently.
            cx.notify();
            return;
        }
        if let Some(chat) = &self.local_chat {
            chat.update(cx, |chat, cx| chat.set_update(snapshot.clone(), cx));
        }
        if let Screen::Settings(settings) = &self.screen {
            settings.update(cx, |settings, cx| settings.set_update(snapshot, cx));
        }
    }

    fn show_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.screen {
            Screen::Settings(settings) => {
                settings.update(cx, |settings, cx| settings.show_about(cx))
            }
            Screen::Chat(_) => self.show_settings(Section::About, cx),
            _ => {
                let details = format!(
                    "Version {}\n{}",
                    crate::ui::titlebar::TitleBar::version(),
                    self.updater.snapshot().label
                );
                drop(window.prompt(
                    gpui::PromptLevel::Info,
                    crate::profile::DISPLAY_NAME,
                    Some(&details),
                    &["OK"],
                    cx,
                ));
            }
        }
    }

    fn update_action(
        &mut self,
        action: crate::update::UpdateAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::update::UpdateAction;
        if self.updater.restart_path().is_some() && action != UpdateAction::Relaunch {
            return;
        }
        if action == UpdateAction::ManualDownload {
            if let Some(url) = self.updater.snapshot().manual_download_url {
                cx.open_url(&url);
            }
            return;
        }
        if action == UpdateAction::Relaunch {
            if let Some(path) = self.updater.restart_path() {
                restart_installed_update(path, cx);
            }
            return;
        }
        if action == UpdateAction::Restart {
            if self.restart_prompt_pending
                || self.updater.snapshot().state != crate::update::State::Ready
                || cx
                    .try_global::<DesktopLifecycle>()
                    .is_some_and(|state| state.closing.get())
            {
                return;
            }
            self.restart_prompt_pending = true;
            #[cfg(target_os = "linux")]
            let answer = {
                let (prompt, answer) = ui::update_confirmation::open(window, cx);
                self.restart_confirmation = Some(prompt);
                cx.notify();
                answer
            };
            #[cfg(not(target_os = "linux"))]
            let answer = window.prompt(gpui::PromptLevel::Warning, "Restart Maple to update?",
                Some("This stops running tasks and remote access on this computer. Editors using Maple must reconnect after the update."),
                &["Later", "Restart to Update"], cx);
            let controller = self.updater.clone();
            let backend = self.backend.clone();
            let install_backend = backend.clone();
            let bridge = cx.spawn(async move |this, cx| {
                let accepted = answer.await == Ok(1);
                let confirmed = this
                    .update(cx, |this, cx| {
                        #[cfg(target_os = "linux")]
                        {
                            this.restart_confirmation = None;
                            cx.notify();
                        }
                        if !accepted
                            || !this.store_ready
                            || this.backend.store_state() != maple_agent::store::StoreState::Open
                            || this.updater.snapshot().state != crate::update::State::Ready
                        {
                            return false;
                        }
                        if let Some(state) = cx.try_global::<DesktopLifecycle>()
                            && state.closing.replace(true)
                        {
                            return false;
                        }
                        persist_window_state();
                        true
                    })
                    .unwrap_or(false);
                let installed = if confirmed {
                    backend
                        .spawn(async move { controller.install(install_backend).await })
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .flatten()
                } else {
                    None
                };
                let _ = this.update(cx, |this, cx| {
                    this.restart_prompt_pending = false;
                    if let Some(path) = installed {
                        restart_installed_update(path, cx);
                    } else if confirmed {
                        cancel_quit(cx);
                    }
                });
            });
            ui::task::retain(&self.remote_bridges, bridge);
            return;
        }
        let controller = self.updater.clone();
        if let UpdateAction::AutomaticDownloads(enabled) = action {
            self.settings.automatic_update_downloads = enabled;
            controller.set_auto_download(enabled);
        }
        // Dropping a Tokio join handle deliberately detaches the operation.
        drop(self.backend.spawn(async move {
            match action {
                UpdateAction::Check => controller.check().await,
                UpdateAction::Download => controller.download().await,
                UpdateAction::Later => controller.later().await,
                UpdateAction::AutomaticDownloads(_) => controller.download_if_automatic().await,
                UpdateAction::Restart | UpdateAction::Relaunch | UpdateAction::ManualDownload => {}
            }
        }));
    }

    /// Tear down the chat screen and return to a fresh login form. A
    /// no-op when the login form is already showing, so a sign-out that
    /// is reported twice (Settings button, then the backend) keeps the
    /// form the user may already be typing in.
    fn show_login(&mut self, cx: &mut Context<Self>) {
        if let Some(user_id) = self.user_id.take() {
            crate::settings::retire_sidebar_account(&user_id);
        }
        self.settings.remote_sidebars.clear();
        self.set_remote_enabled(false, cx);
        self.parked_chat = None;
        self.local_chat = None;
        self.remote_computers.clear();
        self.remote_inventory_loaded = false;
        self.requested_navigation = None;
        self.selected_host = None;
        if matches!(self.screen, Screen::Login(_)) {
            return;
        }
        let backend = self.backend.clone();
        let login = cx.new(|cx| LoginScreen::new(backend, cx));
        self.subscribe_login(&login, cx);
        self.screen = Screen::Login(login);
        cx.notify();
    }

    /// Wire the login form: a successful sign-in opens the chat screen.
    fn subscribe_login(&mut self, login: &Entity<LoginScreen>, cx: &mut Context<Self>) {
        cx.subscribe(
            login,
            |app: &mut MapleApp, _emitter, event: &LoginSucceeded, cx| {
                app.open_chat(event.0.clone(), cx);
            },
        )
        .detach();
    }

    /// Show the chat screen for a signed-in account and wire its events.
    fn open_chat(&mut self, user_id: String, cx: &mut Context<Self>) {
        if self.user_id.as_deref() != Some(user_id.as_str())
            && let Some(previous) = self.user_id.as_deref()
        {
            crate::settings::retire_sidebar_account(previous);
        }
        log::debug!(
            "startup: chat screen open at {} ms",
            crate::startup_elapsed()
        );
        self.settings.remote_sidebars.clear();
        let backend = self.backend.clone();
        let chat = cx.new(|cx| ChatScreen::new(backend, user_id.clone(), cx));
        chat.update(cx, |chat, cx| chat.set_update(self.updater.snapshot(), cx));
        self.user_id = Some(user_id);
        self.parked_chat = None;
        self.screen = Screen::Chat(chat.clone());
        self.local_chat = Some(chat.clone());
        self.remote_computers.clear();
        self.remote_inventory_loaded = false;
        self.requested_navigation = None;
        self.selected_host = None;
        self.subscribe_chat(&chat, cx);
        self.load_remote_sidebars(cx);
        cx.notify();
    }

    /// Nicknames and remote pins belong to this desktop's signed-in account,
    /// even while its selected task is on another computer.
    fn load_remote_sidebars(&self, cx: &mut Context<Self>) {
        let (Some(account), Some(local_chat)) = (&self.user_id, &self.local_chat) else {
            return;
        };
        let account = account.clone();
        let owner = local_chat.entity_id();
        let epoch = self.store_load_epoch;
        let backend = self.backend.clone();
        let requested_account = account.clone();
        let load = backend
            .clone()
            .spawn(async move { backend.get_remote_sidebars(&requested_account).await });
        cx.spawn(async move |this, cx| match load.await {
            Ok(Ok(sidebars)) => {
                let _ = this.update(cx, |this, cx| {
                    this.apply_remote_sidebars(epoch, &account, owner, sidebars, cx);
                });
            }
            result => log::warn!("Could not load computer nicknames: {result:?}"),
        })
        .detach();
    }

    fn apply_remote_sidebars(
        &mut self,
        epoch: u64,
        account: &str,
        owner: gpui::EntityId,
        mut sidebars: std::collections::HashMap<String, crate::settings::RemoteSidebarSettings>,
        cx: &mut Context<Self>,
    ) {
        if self.store_load_epoch != epoch
            || !self.store_ready
            || self.backend.store_state() != maple_agent::store::StoreState::Open
            || self.user_id.as_deref() != Some(account)
            || self
                .local_chat
                .as_ref()
                .is_none_or(|chat| chat.entity_id() != owner)
        {
            return;
        }
        // The account projection starts empty on every attach. Entries already
        // present were edited locally while this read was in flight; retain
        // those names while taking the owner's independently stored pins.
        for (key, current) in &self.settings.remote_sidebars {
            sidebars.entry(key.clone()).or_default().display_name = current.display_name.clone();
        }
        self.settings.remote_sidebars = sidebars;
        for (key, computer) in &mut self.remote_computers {
            if let Some(sidebar) = self.settings.remote_sidebars.get(key)
                && !sidebar.display_name.trim().is_empty()
            {
                computer.name = sidebar.display_name.clone();
            }
        }
        if let Screen::Settings(screen) = &self.screen {
            screen.update(cx, |screen, cx| {
                screen.refresh_remote_sidebars(self.settings.remote_sidebars.clone(), cx);
            });
        }
        self.sync_computers(cx);
        cx.notify();
    }

    fn update_computers(&mut self, status: crate::remote::manager::Status, cx: &mut Context<Self>) {
        self.set_remote_enabled(status.enabled, cx);
        let computers = status
            .peers
            .into_iter()
            .filter(|(_, peer)| peer.can_connect)
            .map(|(key, peer)| {
                let alias = self
                    .settings
                    .remote_sidebars
                    .get(&key)
                    .map_or("", |s| s.display_name.as_str());
                let label = ui::remote::computer_name(&key, &peer.name, alias);
                (key, (label, peer.grant))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut changed =
            !self.remote_inventory_loaded || self.remote_computers.len() != computers.len();
        self.remote_inventory_loaded = true;
        let mut selected_grant_changed = false;
        self.remote_computers
            .retain(|key, _| computers.contains_key(key));
        for (key, (name, grant)) in computers {
            match self.remote_computers.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    changed = true;
                    entry.insert(RemoteComputer {
                        name,
                        grant,
                        chat: None,
                        attempt: ConnectionAttempt::Idle,
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    if entry.get().grant != grant {
                        changed = true;
                        selected_grant_changed |= self.selected_host.as_ref() == Some(entry.key());
                        let computer = entry.get_mut();
                        computer.grant = grant;
                        computer.chat = None;
                        computer.attempt = ConnectionAttempt::Idle;
                    }
                    if entry.get().name != name {
                        changed = true;
                        entry.get_mut().name = name;
                    }
                }
            }
        }
        if selected_grant_changed
            || self
                .selected_host
                .as_ref()
                .is_some_and(|key| !self.remote_computers.contains_key(key))
        {
            self.navigate(
                Navigate {
                    computer: None,
                    destination: Destination::Overview,
                },
                cx,
            );
        }
        if changed {
            self.sync_computers(cx);
        }
        let now = std::time::Instant::now();
        let ready = self
            .remote_computers
            .iter()
            .filter(|(_, computer)| computer.should_connect(now))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for peer in ready {
            self.connect_computer(peer, cx);
        }
        for chat in self.local_chat.iter().chain(
            self.remote_computers
                .values()
                .filter_map(|c| c.chat.as_ref()),
        ) {
            chat.update(cx, |chat, cx| chat.refresh_navigation_if_due(cx));
        }
    }

    fn sync_computers(&mut self, cx: &mut Context<Self>) {
        if !self.remote_inventory_loaded {
            return;
        }
        let Some(local) = &self.local_chat else {
            return;
        };
        let mut computers = vec![local.read(cx).computer_snapshot("This Computer".into(), cx)];
        for (key, computer) in &self.remote_computers {
            computers.push(match &computer.chat {
                Some(chat) => chat
                    .read(cx)
                    .computer_snapshot(computer.name.clone().into(), cx),
                None => Computer::pending(key.clone(), computer.name.clone(), computer.error()),
            });
        }
        let computers = Arc::new(computers);
        for chat in std::iter::once(local).chain(
            self.remote_computers
                .values()
                .filter_map(|c| c.chat.as_ref()),
        ) {
            chat.update(cx, |chat, cx| chat.set_computers(computers.clone(), cx));
        }
    }

    fn schedule_computers_sync(&mut self, cx: &mut Context<Self>) {
        if self.computers_sync_pending {
            return;
        }
        self.computers_sync_pending = true;
        let weak = cx.entity().downgrade();
        cx.defer(move |cx| {
            weak.update(cx, |app, cx| {
                app.computers_sync_pending = false;
                app.sync_computers(cx);
            })
            .ok();
        });
    }

    fn project_owner(
        &self,
        target: &ProjectTarget,
    ) -> Option<(Entity<ChatScreen>, Option<[u8; 16]>)> {
        match &target.computer {
            None => self.local_chat.clone().map(|chat| (chat, None)),
            Some(peer) => self.remote_computers.get(peer).and_then(|computer| {
                computer
                    .chat
                    .clone()
                    .map(|chat| (chat, Some(computer.grant)))
            }),
        }
    }

    fn has_chat(&self, id: gpui::EntityId) -> bool {
        self.local_chat
            .as_ref()
            .is_some_and(|chat| chat.entity_id() == id)
            || self.remote_computers.values().any(|computer| {
                computer
                    .chat
                    .as_ref()
                    .is_some_and(|chat| chat.entity_id() == id)
            })
    }

    fn rename_project(
        &mut self,
        source: Entity<ChatScreen>,
        request: ProjectRenameRequest,
        cx: &mut Context<Self>,
    ) {
        // Old screens may finish emitting while sign-out or unpairing removes
        // them. They cannot initiate a mutation for the replacement account.
        let Some(account) = self
            .user_id
            .clone()
            .filter(|_| self.has_chat(source.entity_id()))
        else {
            return;
        };
        let Some((owner, grant)) = self
            .project_owner(&request.target)
            .filter(|(chat, _)| chat.read(cx).project_names_available())
        else {
            source.update(cx, |chat, cx| {
                chat.finish_project_rename(
                    request.target,
                    Err(
                        "This computer is unavailable. Reconnect it before renaming the project."
                            .into(),
                    ),
                    cx,
                );
            });
            return;
        };
        let backend = owner.read(cx).execution_backend();
        let pending = PendingProjectRename {
            target: request.target.clone(),
            account: account.clone(),
            owner: owner.entity_id(),
            grant,
            source: source.downgrade(),
        };
        let executor = backend.clone();
        let bridge = ui::task::call(
            &executor,
            async move {
                backend
                    .rename_project_root(&account, request.target.path, request.name)
                    .await
            },
            cx,
            move |app, result, cx| app.complete_project_rename(pending, result, cx),
        );
        ui::task::retain(&self.remote_bridges, bridge);
    }

    fn complete_project_rename(
        &mut self,
        pending: PendingProjectRename,
        result: Result<maple_agent::agent::RecentProjectRoot, String>,
        cx: &mut Context<Self>,
    ) {
        // Never repopulate a removed computer, a new grant, or a replacement
        // account from an old response. No connection or rename is replayed.
        if self.user_id.as_deref() != Some(&pending.account) {
            return;
        }
        let owner = self.project_owner(&pending.target).filter(|(chat, grant)| {
            chat.entity_id() == pending.owner
                && *grant == pending.grant
                && !chat.read(cx).connection_ended()
        });
        let result = match owner {
            Some((owner, _)) => result.map(|root| {
                owner.update(cx, |chat, cx| chat.apply_project_names(vec![root], cx));
                // Publish the accepted name before removing the source's
                // optimistic label, so it never flashes back to the old name.
                self.sync_computers(cx);
            }),
            None => Err("The computer connection changed. Check its current project name before trying again.".into()),
        };
        if let Some(source) = pending.source.upgrade()
            && self.has_chat(source.entity_id())
        {
            source.update(cx, |chat, cx| {
                chat.finish_project_rename(pending.target, result, cx)
            });
        }
    }

    fn navigate(&mut self, request: Navigate, cx: &mut Context<Self>) {
        self.requested_navigation = Some(PendingNavigation {
            request: request.clone(),
            source: self.navigation_source(cx),
        });
        let chat = match &request.computer {
            None => self.local_chat.clone(),
            Some(peer) => self
                .remote_computers
                .get(peer)
                .and_then(|computer| computer.chat.as_ref())
                .filter(|chat| !chat.read(cx).connection_ended())
                .cloned(),
        };
        if let Some(chat) = chat {
            let browse = match &self.screen {
                Screen::Chat(previous) if previous.entity_id() != chat.entity_id() => {
                    Some(previous.read(cx).sidebar_browse_state(cx))
                }
                _ => None,
            };
            for other in self.local_chat.iter().chain(
                self.remote_computers
                    .values()
                    .filter_map(|c| c.chat.as_ref()),
            ) {
                other.update(cx, |other, cx| other.set_visible(false, cx));
            }
            self.selected_host = request.computer;
            self.screen = Screen::Chat(chat.clone());
            self.parked_chat = None;
            chat.update(cx, |chat, cx| {
                chat.set_visible(true, cx);
                if let Some(browse) = browse {
                    chat.restore_sidebar_browse_state(browse, cx);
                }
                chat.navigate(request.destination, cx);
            });
            self.requested_navigation = None;
            cx.notify();
        } else if let Some(peer) = request.computer {
            self.connect_computer(peer, cx);
        }
    }

    fn navigation_source(&self, cx: &gpui::App) -> Option<(gpui::EntityId, u64)> {
        match &self.screen {
            Screen::Chat(chat) => Some((chat.entity_id(), chat.read(cx).navigation_generation())),
            _ => None,
        }
    }

    fn set_remote_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self
            .remote_enabled
            .swap(enabled, std::sync::atomic::Ordering::AcqRel)
            == enabled
        {
            return;
        }
        self.update_tray();
        cx.notify();
    }

    fn connect_computer(&mut self, peer: String, cx: &mut Context<Self>) {
        let Some(user_id) = self.user_id.clone() else {
            return;
        };
        let Some(computer) = self.remote_computers.get_mut(&peer) else {
            return;
        };
        // Explicit navigation may retry after either computer was upgraded.
        // Only background discovery uses should_connect's incompatibility gate.
        if matches!(computer.attempt, ConnectionAttempt::Connecting(_)) {
            return;
        }
        self.next_connection_attempt += 1;
        let attempt = self.next_connection_attempt;
        computer.attempt = ConnectionAttempt::Connecting(attempt);
        let backend = self.backend.clone();
        let target = peer.clone();
        self.sync_computers(cx);
        let bridge = ui::task::call(
            &self.backend,
            async move { backend.remote(target).await },
            cx,
            move |app, result, cx| {
                if app.user_id.as_deref() != Some(&user_id) {
                    return;
                }
                // Unpairing/re-pairing or signing out discards an old attempt.
                // A late completion never resurrects that computer or its view.
                if !app.remote_computers.get(&peer).is_some_and(|computer| matches!(computer.attempt, ConnectionAttempt::Connecting(id) if id == attempt)) {
                    return;
                }
                match result {
                    Ok(backend) => {
                        let source = backend.clone();
                        let rx = backend.spawn(async move { source.take_events().await });
                        let chat = cx.new(|cx| ChatScreen::new_catalog(backend, user_id, cx));
                        app.subscribe_chat(&chat, cx);
                        let weak = chat.downgrade();
                        cx.spawn(async move |_, cx| {
                            let Some(mut rx) = rx.await.ok().flatten() else {
                                return;
                            };
                            while let Some(event) = rx.recv().await {
                                let mut batch = vec![event];
                                while batch.len() < 256 {
                                    if let Ok(event) = rx.try_recv() {
                                        batch.push(event);
                                    } else {
                                        break;
                                    }
                                }
                                if weak
                                    .update(cx, |chat, cx| chat.handle_service_events(batch, cx))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        })
                        .detach();
                        if let Some(computer) = app.remote_computers.get_mut(&peer) {
                            computer.chat = Some(chat.clone());
                            computer.attempt = ConnectionAttempt::Idle;
                        }
                        if let Some(pending) = app.requested_navigation.clone()
                            && pending.request.computer.as_deref() == Some(&peer)
                        {
                            if pending.source == app.navigation_source(cx) {
                                app.navigate(pending.request, cx);
                            } else {
                                app.requested_navigation = None;
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(computer) = app.remote_computers.get_mut(&peer) {
                            computer.attempt = ConnectionAttempt::Failed {
                                message: error.clone(),
                                retry_at: std::time::Instant::now()
                                    + std::time::Duration::from_secs(30),
                            };
                        }
                        if app.requested_navigation.as_ref().is_some_and(|pending| {
                            pending.request.computer.as_deref() == Some(&peer)
                        }) && let Screen::Chat(chat) = &app.screen
                        {
                            chat.update(cx, |chat, cx| chat.navigation_notice(error, cx));
                            app.requested_navigation = None;
                        }
                    }
                }
                app.sync_computers(cx);
                cx.notify();
            },
        );
        ui::task::retain(&self.remote_bridges, bridge);
    }

    /// Park the chat screen and show settings.
    fn show_settings(&mut self, section: Section, cx: &mut Context<Self>) {
        let Screen::Chat(chat) = &self.screen else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone().unwrap_or_default();
        let settings = self.settings.clone();
        let shortcut_snapshot = self.shortcuts.snapshot();
        let integrations_backend = chat.read(cx).execution_backend();
        chat.update(cx, |chat, cx| chat.set_visible(false, cx));
        let screen = cx.new(|cx| {
            SettingsScreen::new(
                backend,
                integrations_backend,
                user_id,
                settings,
                shortcut_snapshot,
                section,
                cx,
            )
        });
        screen.update(cx, |settings, cx| {
            settings.set_update(self.updater.snapshot(), cx)
        });
        cx.subscribe(
            &screen,
            |_, _, event: &crate::update::UpdateRequested, cx| {
                let action = event.0;
                cx.defer(move |cx| dispatch_update_action(action, cx));
            },
        )
        .detach();
        cx.subscribe(
            &screen,
            |app, emitter, event: &ui::remote::ComputersChanged, cx| {
                if app.is_current_settings_screen(&emitter) {
                    app.update_computers(event.0.clone(), cx);
                }
            },
        )
        .detach();
        cx.subscribe(
            &screen,
            |app, emitter, event: &ui::remote::ComputerNamed, cx| {
                if !app.is_current_settings_screen(&emitter) {
                    return;
                }
                app.settings
                    .remote_sidebars
                    .entry(event.key.clone())
                    .or_default()
                    .display_name = event.name.clone();
                if let Some(computer) = app.remote_computers.get_mut(&event.key) {
                    computer.name = event.name.clone();
                    app.sync_computers(cx);
                }
            },
        )
        .detach();
        cx.subscribe(
            &screen,
            |app, emitter, event: &ui::remote::ConnectComputer, cx| {
                if !app.is_current_settings_screen(&emitter) {
                    return;
                }
                let settings = emitter.read(cx).settings_snapshot();
                app.close_settings(settings, cx);
                app.navigate(
                    Navigate {
                        computer: Some(event.0.clone()),
                        destination: Destination::Overview,
                    },
                    cx,
                );
            },
        )
        .detach();
        cx.subscribe(
            &screen,
            |app: &mut MapleApp, emitter, event: &SettingsClosed, cx| {
                if app.is_current_settings_screen(&emitter) {
                    app.close_settings(event.0.clone(), cx);
                }
            },
        )
        .detach();
        cx.subscribe(&screen, |app: &mut MapleApp, emitter, event: &ui::store_gate::PasswordFormFinished, cx| {
            if matches!(&app.screen, Screen::Settings(active) if active.entity_id() == emitter.entity_id()) {
                app.password_settings_finished(event, cx);
            }
        }).detach();
        cx.subscribe(
            &screen,
            |app, _, _: &ui::store_gate::RetryStorePersistence, cx| {
                app.retry_persistence(cx);
            },
        )
        .detach();
        cx.subscribe(
            &screen,
            |app: &mut MapleApp, emitter, event: &ShortcutSettingsRequested, cx| {
                let result = app.apply_shortcut_change(&event.0, cx);
                let shortcut_overrides = app.settings.shortcut_overrides.clone();
                let snapshot = app.shortcuts.snapshot();
                emitter.update(cx, |screen, cx| {
                    screen.apply_shortcut_result(shortcut_overrides, snapshot, result, cx);
                });
            },
        )
        .detach();
        cx.subscribe(
            &screen,
            |app: &mut MapleApp, emitter, _event: &SignOutRequested, cx| {
                if !app.is_current_settings_screen(&emitter) {
                    return;
                }
                if let Some(chat) = app.local_chat.clone() {
                    chat.update(cx, |chat, cx| chat.sign_out(cx));
                }
            },
        )
        .detach();
        cx.subscribe(
            &screen,
            |app: &mut MapleApp, emitter, _event: &AccountDeleted, cx| {
                if !app.is_current_settings_screen(&emitter) {
                    return;
                }
                // The runtime stopped and the credentials went before the
                // event; the parked chat has nothing left to sign out of.
                app.parked_chat = None;
                app.show_login(cx);
            },
        )
        .detach();
        self.parked_chat = Some(chat.clone());
        self.screen = Screen::Settings(screen);
        cx.notify();
    }

    /// Wire per-chat events: sign-out and the settings gear.
    fn subscribe_chat(&mut self, chat: &Entity<ChatScreen>, cx: &mut Context<Self>) {
        cx.subscribe(chat, |_, _, event: &crate::update::UpdateRequested, cx| {
            let action = event.0;
            cx.defer(move |cx| dispatch_update_action(action, cx));
        })
        .detach();
        cx.subscribe(
            chat,
            |app, _, _: &ui::store_gate::RetryStorePersistence, cx| {
                app.retry_persistence(cx);
            },
        )
        .detach();
        cx.subscribe(chat, |app, _, _: &NavigationChanged, cx| {
            app.schedule_computers_sync(cx)
        })
        .detach();
        cx.subscribe(chat, |app, _, request: &Navigate, cx| {
            app.navigate(request.clone(), cx)
        })
        .detach();
        cx.subscribe(chat, |app, source, request: &ProjectRenameRequest, cx| {
            app.rename_project(source, request.clone(), cx)
        })
        .detach();
        cx.subscribe(
            chat,
            |app: &mut MapleApp, emitter, _event: &LoggedOut, cx| {
                if app
                    .local_chat
                    .as_ref()
                    .is_some_and(|chat| chat.entity_id() == emitter.entity_id())
                {
                    app.show_login(cx);
                }
            },
        )
        .detach();
        cx.subscribe(
            chat,
            |app: &mut MapleApp, emitter, event: &SignOutFailed, cx| {
                if app.user_id.as_deref() == Some(event.user_id.as_str())
                    && app
                        .local_chat
                        .as_ref()
                        .is_some_and(|chat| chat.entity_id() == emitter.entity_id())
                {
                    app.present_store_attention(
                        event.message.clone(),
                        StoreAttentionKind::Persistence,
                        true,
                        cx,
                    );
                }
            },
        )
        .detach();
        cx.subscribe(
            chat,
            |app: &mut MapleApp, _emitter, _event: &ui::chat::OpenSettings, cx| {
                app.show_settings(Section::General, cx);
            },
        )
        .detach();
        cx.subscribe(
            chat,
            |app: &mut MapleApp, _emitter, event: &ui::settings::OpenSettingsSection, cx| {
                app.show_settings(event.0, cx);
            },
        )
        .detach();
    }

    fn is_current_settings_screen(&self, screen: &Entity<SettingsScreen>) -> bool {
        matches!(&self.screen, Screen::Settings(active) if active.entity_id() == screen.entity_id())
    }

    /// Restore the parked chat screen, applying any changed defaults.
    fn close_settings(
        &mut self,
        mut updated: crate::settings::AppSettings,
        cx: &mut Context<Self>,
    ) {
        // A close event can have been queued before the owner refreshed mode.
        // Only the authoritative store snapshot may change this derived field.
        updated.store_password_mode = self.settings.store_password_mode;
        // A queued close may predate account-presentation hydration. Nickname
        // edits are forwarded separately and must not be replaced by that copy.
        updated.remote_sidebars = self.settings.remote_sidebars.clone();
        self.settings = updated;
        ui::theme::apply_preference(ui::theme::Preference::parse(&self.settings.theme), cx);
        ui::typography::apply(
            ui::typography::ChatFontFamily::parse(&self.settings.chat_font_family),
            self.settings.chat_font_size,
        );
        for chat in self.local_chat.iter().chain(
            self.remote_computers
                .values()
                .filter_map(|c| c.chat.as_ref()),
        ) {
            chat.update(cx, |chat, cx| chat.apply_defaults(&self.settings, cx));
        }
        if let Some(chat) = self.parked_chat.take() {
            chat.update(cx, |chat, cx| chat.set_visible(true, cx));
            self.screen = Screen::Chat(chat);
        }
        cx.notify();
    }

    fn apply_shortcut_change(
        &mut self,
        change: &ShortcutSettingsChange,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let mut overrides = self.settings.shortcut_overrides.clone();
        crate::shortcuts::migrate_renamed_slots(&mut overrides);
        match change {
            ShortcutSettingsChange::Set {
                slot_id,
                sequence,
                disable_conflicts,
            } => {
                for conflict in disable_conflicts {
                    if conflict != slot_id {
                        overrides.insert(conflict.clone(), None);
                    }
                }
                overrides.insert(slot_id.clone(), Some(sequence.clone()));
            }
            ShortcutSettingsChange::Disable { slot_id } => {
                overrides.insert(slot_id.clone(), None);
            }
            ShortcutSettingsChange::Reset { slot_id } => {
                overrides.remove(slot_id);
            }
            ShortcutSettingsChange::ResetAll => overrides.clear(),
        }

        self.shortcuts.replace(&overrides, cx)?;
        self.settings.shortcut_overrides = overrides.clone();
        crate::settings::update_settings_in_background(move |settings| {
            settings.shortcut_overrides = overrides;
        });
        Ok(())
    }
}

impl Render for MapleApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        static FIRST_RENDER: std::sync::Once = std::sync::Once::new();
        FIRST_RENDER.call_once(|| {
            log::debug!("startup: first render at {} ms", crate::startup_elapsed());
        });
        self.present_pending_store_attention(window, cx);
        remember_window_state(window, cx);
        if ui::theme::resolve(window.appearance()) {
            // Every view reads the palette in render; make them all redo it.
            cx.refresh_windows();
        }
        let titlebar = self.titlebar.clone();
        let root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .font_family(crate::assets::FONT_BODY)
            // The default for any text that sets no color of its own.
            // GPUI's own default is black, which vanishes on the dark
            // surfaces, so every omission used to be a dark-mode bug.
            .text_color(gpui::rgb(ui::theme::text_primary()))
            // Chrome default 14 px (`text_sm`). Chat reading size is set
            // on transcript/composer via `typography::chat_reading`.
            .text_sm()
            .child(titlebar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .debug_selector(|| "maple-page".to_string())
                    .child(if self.updater.restart_path().is_some() {
                        div()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .p_6()
                            .child(div().text_xl().child("Update installed"))
                            .child(self.updater.snapshot().label)
                            .child(
                                ui::widgets::primary_button("restart-installed-update")
                                    .child("Restart Maple")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.update_action(
                                            crate::update::UpdateAction::Relaunch,
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                ui::widgets::secondary_button("quit-installed-update")
                                    .child("Quit Maple")
                                    .on_click(|_, _, cx| quit_app(cx)),
                            )
                            .into_any_element()
                    } else {
                        match &self.screen {
                            Screen::Restoring => restoring_view().into_any_element(),
                            Screen::StoreGate(gate) => gate.clone().into_any_element(),
                            Screen::Login(login) => login.clone().into_any_element(),
                            Screen::Chat(chat) => chat.clone().into_any_element(),
                            Screen::Settings(screen) => screen.clone().into_any_element(),
                        }
                    }),
            );
        #[cfg(target_os = "linux")]
        let root = root.children(self.restart_confirmation.clone());
        root
    }
}

#[cfg(all(test, feature = "proxy"))]
mod local_api_tests;
#[cfg(target_os = "macos")]
mod native_quit;
#[cfg(target_os = "macos")]
mod native_window;

fn persistence_feedback_message(
    failure: &maple_agent::store::PersistenceFailure,
    open: bool,
) -> &'static str {
    if !open && failure.reason == maple_agent::store::PersistenceFailureReason::DiskFull {
        "The disk is full and Maple could not open its local store. Free disk space, quit and reopen Maple, then unlock again if prompted."
    } else {
        failure.message()
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct DesktopStoreStatus {
    state: maple_agent::store::StoreState,
    mode: String,
    setup_operation_id: Option<String>,
    store_id: Option<String>,
    #[serde(default)]
    completion_notice: Option<StoreCompletionNotice>,
    #[serde(default)]
    mode_change_notice: Option<ModeChangeNotice>,
    #[serde(default)]
    persistence_status: maple_agent::store::PersistenceStatus,
    #[serde(default)]
    local_api_running: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StoreCompletionNotice {
    operation_id: String,
    message: String,
    is_error: bool,
    #[serde(default)]
    requires_attention: bool,
    #[serde(default)]
    inline_password_success: bool,
    #[serde(default)]
    ui_operation_id: Option<String>,
    #[serde(default)]
    mode_notice_id: Option<String>,
}
type ModeChangeNotice = maple_agent::store::lifecycle::StoreModeNotice;
impl DesktopStoreStatus {
    fn matches_pending_password_operation(&self, pending: Option<&str>) -> bool {
        pending.is_some()
            && self.completion_notice.as_ref().is_some_and(|notice| {
                notice.inline_password_success && notice.ui_operation_id.as_deref() == pending
            })
    }
    fn reconcile_password_completion_notice(
        &self,
        seen_operation: &mut Option<String>,
        visible: &mut Option<String>,
        visible_is_error: &mut bool,
        busy_recipient: bool,
        handled_operation: Option<&str>,
    ) -> bool {
        if let Some(notice) = &self.completion_notice
            && notice.inline_password_success
        {
            if handled_operation == Some(notice.operation_id.as_str()) {
                if seen_operation.as_deref() == Some(notice.operation_id.as_str()) {
                    let changed = visible.take().is_some() || *visible_is_error;
                    *visible_is_error = false;
                    return changed;
                }
                if visible.is_none() {
                    *seen_operation = Some(notice.operation_id.clone());
                }
                // A delayed status for our success cannot clear a different
                // operation's current error or synchronization warning.
                return false;
            }
            if busy_recipient {
                // Defer, rather than consume: if the form or its reply is
                // lost, the next poll without a recipient shows the fallback.
                return false;
            }
        }
        self.reconcile_completion_notice(seen_operation, visible, visible_is_error)
    }
    fn reconcile_completion_notice(
        &self,
        seen_operation: &mut Option<String>,
        visible: &mut Option<String>,
        visible_is_error: &mut bool,
    ) -> bool {
        if let Some(message) = self.fresh_completion_notice(seen_operation) {
            *visible = Some(message);
            *visible_is_error = self
                .completion_notice
                .as_ref()
                .is_some_and(|notice| notice.is_error);
            return true;
        }
        // Successful unlock intentionally has no completion notice. Clear only
        // a previously displayed operational error when the host now reports
        // Open without that failure latch. Durable mode notices and successful
        // synchronization warnings are managed separately and remain visible.
        if self.state == maple_agent::store::StoreState::Open
            && self.completion_notice.is_none()
            && *visible_is_error
        {
            *visible = None;
            *visible_is_error = false;
            return true;
        }
        false
    }

    fn current_mode_notice(&self) -> Option<&ModeChangeNotice> {
        self.mode_change_notice.as_ref().filter(|notice| {
            self.state == maple_agent::store::StoreState::Open
                && self.store_id.as_deref() == Some(notice.store_id.as_str())
        })
    }

    fn fresh_completion_notice(&self, seen_operation: &mut Option<String>) -> Option<String> {
        let notice = self.completion_notice.as_ref()?;
        if seen_operation.as_deref() == Some(notice.operation_id.as_str()) {
            return None;
        }
        *seen_operation = Some(notice.operation_id.clone());
        (notice.is_error || notice.requires_attention).then(|| notice.message.clone())
    }

    fn keeps_resident(&self) -> bool {
        self.local_api_running
            || (self.mode == "password" && self.state == maple_agent::store::StoreState::Open)
            || self.state == maple_agent::store::StoreState::Maintenance
            || self.setup_operation_id.is_some()
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct HostActivity {
    acp_clients: usize,
    running_tasks: usize,
    remote_controllers: usize,
    #[serde(default)]
    background_tasks: usize,
    #[serde(default)]
    local_api_running: bool,
    #[serde(default)]
    local_api_requests: usize,
}
impl HostActivity {
    fn needs_confirmation(&self) -> bool {
        self.acp_clients > 0
            || self.running_tasks > 0
            || self.remote_controllers > 0
            || self.background_tasks > 0
            || self.local_api_running
            || self.local_api_requests > 0
    }
}

#[derive(Clone)]
struct DesktopLifecycle {
    backend: Arc<AgentBackend>,
    updater: crate::update::Controller,
    remote_enabled: Arc<std::sync::atomic::AtomicBool>,
    store_resident: Arc<std::sync::atomic::AtomicBool>,
    can_background: bool,
    closing: std::rc::Rc<std::cell::Cell<bool>>,
    quit_completed: std::rc::Rc<std::cell::Cell<bool>>,
    quit_requested: std::rc::Rc<std::cell::Cell<bool>>,
    frontend_detached: std::rc::Rc<std::cell::Cell<bool>>,
    #[cfg(target_os = "windows")]
    update_restart_pending: std::rc::Rc<std::cell::Cell<bool>>,
}
impl gpui::Global for DesktopLifecycle {}

/// Recheck final effects: an earlier Quit cannot exit an installation.
fn may_finish_quit(cx: &App) -> bool {
    !cx.try_global::<DesktopLifecycle>().is_some_and(|state| {
        state.updater.restart_path().is_none()
            && state.updater.snapshot().state == crate::update::State::Installing
    })
}

fn restart_installed_update(path: PathBuf, cx: &mut App) {
    let Some(state) = cx.try_global::<DesktopLifecycle>().cloned() else {
        return;
    };
    if state.updater.restart_path().as_ref() != Some(&path)
        || !state.backend.is_retired_after_update()
    {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        if state.update_restart_pending.replace(true) {
            return;
        }
        // Signature/tree verification can touch disk and trust services. Keep
        // it off the UI thread; repeated clicks share this one restart attempt.
        // Native argv and exact process waiting avoid shell path interpolation.
        let restart_path = path.clone();
        let restart = cx.background_executor().spawn(async move {
            crate::windows_bootstrap::schedule_restart(&restart_path, crate::profile::PROFILE)
        });
        cx.spawn(async move |cx| {
            let result = restart.await;
            cx.update(|cx| {
                if let Err(error) = result {
                    state.update_restart_pending.set(false);
                    log::error!("Windows update restart could not start: {error}");
                    show_update_about(cx);
                    return;
                }
                if state.updater.restart_path().as_ref() == Some(&path)
                    && state.backend.is_retired_after_update()
                {
                    state.quit_completed.set(true);
                    cx.quit();
                }
            });
        })
        .detach();
    }
    #[cfg(not(target_os = "windows"))]
    {
        state.quit_completed.set(true);
        #[cfg(target_os = "macos")]
        native_quit::allow_update_restart();
        cx.set_restart_path(path);
        cx.restart();
        #[cfg(target_os = "macos")]
        native_quit::finish_update_restart();
    }
}

fn finish_quit(cx: &mut App) {
    if !may_finish_quit(cx) {
        cancel_native_quit();
        show_update_about(cx);
        return;
    }
    #[cfg(target_os = "macos")]
    native_quit::complete(cx);
    #[cfg(not(target_os = "macos"))]
    cx.quit();
}

/// Menu, keyboard, tray and AppKit all enter the same coordinator. The
/// frontend remains alive until settings commit and the host releases owner.lock.
fn quit_app(cx: &mut App) {
    #[cfg(target_os = "macos")]
    if let Some(startup) = cx
        .try_global::<GuardedStartupFailure>()
        .map(|state| state.0.clone())
    {
        startup.update(cx, |startup, cx| startup.request_close(cx));
        return;
    }
    let Some(state) = cx.try_global::<DesktopLifecycle>().cloned() else {
        finish_quit(cx);
        return;
    };
    if state.updater.restart_path().is_some() && state.backend.is_retired_after_update() {
        // A committed install retired old-build RPCs and deliberately retains
        // owner.lock until process exit. Never probe or stop that host again.
        state.quit_completed.set(true);
        finish_quit(cx);
        return;
    }
    if !may_finish_quit(cx) {
        cancel_native_quit();
        show_update_about(cx);
        return;
    }
    // Once a soft close has committed its intentional detach, finishing that
    // frontend exit wins. It must never be reported as a completed host stop.
    if state.frontend_detached.get() {
        return;
    }
    state.quit_requested.set(true);
    if state.closing.replace(true) {
        return;
    }
    let backend = state.backend.clone();
    let activity = backend.clone().spawn(async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            backend.local_control::<HostActivity>("host_status", ()),
        )
        .await
        .map_err(|_| "The host did not answer its activity check".to_string())?
    });
    cx.spawn(async move |cx| match activity.await {
        Ok(Ok(activity)) if activity.needs_confirmation() => {
            cx.update(|cx| show_quit_prompt(QuitPromptKind::Confirm(activity), cx));
        }
        Ok(Ok(_)) => cx.update(|cx| drain_and_quit(state, cx)),
        result => {
            log::warn!("Could not check Maple host before Quit: {result:?}");
            cx.update(|cx| show_quit_prompt(QuitPromptKind::UnknownActivity, cx));
        }
    })
    .detach();
}

fn drain_and_quit(state: DesktopLifecycle, cx: &mut App) {
    // A locked startup window must not save its temporary/default geometry
    // over the encrypted preferences it has not loaded.
    if state.backend.store_state() == maple_agent::store::StoreState::Open {
        persist_window_state();
    }
    let backend = state.backend.clone();
    let stop = backend.clone().spawn(async move {
        let flush_backend = backend.clone();
        finish_quit_shutdown(
            flush_frontend_state(&flush_backend),
            crate::host::host_has_stopped(backend.api_url()),
            crate::host::request_stop(backend.api_url()),
        )
        .await
    });
    cx.spawn(async move |cx| {
        match stop.await {
            Ok(Ok(())) => cx.update(|cx| {
                if !may_finish_quit(cx) {
                    state.closing.set(false);
                    cancel_native_quit();
                    show_update_about(cx);
                    return;
                }
                state.quit_completed.set(true);
                #[cfg(target_os = "macos")]
                native_quit::complete(cx);
                #[cfg(not(target_os = "macos"))]
                cx.quit();
            }),
            result => {
                state.closing.set(false);
                cancel_native_quit();
                log::warn!("Could not complete Maple host shutdown: {result:?}");
                cx.update(|cx| show_quit_warning("Maple has not finished stopping. Local data may remain unlocked and tasks may still be active. Keep Maple open or retry Quit.".into(), cx));
            }
        }
    }).detach();
}

/// Flush presentation edits before either an explicit host stop or an
/// intentional frontend-only detach. Neither path may lose pending settings.
pub(crate) async fn flush_frontend_state(backend: &Arc<AgentBackend>) -> Result<(), String> {
    // A browser handoff holds the account writer while it waits. Cancellation
    // deliberately bypasses that writer in the host.
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        backend.local_control::<()>("cancel_oauth", ()),
    )
    .await
    .map_err(|_| "Maple could not cancel sign-in before quitting".to_string())??;
    crate::settings::flush_pending().await?;
    if backend.store_state() == maple_agent::store::StoreState::Open {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            backend.flush_ui_settings(),
        )
        .await
        .map_err(|_| "Maple could not save settings before quitting".to_string())??;
    }
    Ok(())
}

/// A plain, nonresident window close detaches this frontend. Other clients and
/// the host's ordinary idle policy retain ownership of their work.
fn detach_and_close(state: DesktopLifecycle, cx: &mut App) {
    let backend = state.backend.clone();
    let flush = backend
        .clone()
        .spawn(async move { flush_frontend_state(&backend).await });
    cx.spawn(async move |cx| {
        match flush.await {
            Ok(Ok(())) => cx.update(|cx| {
                #[cfg(target_os = "macos")]
                let native_quit_pending = native_quit::is_pending();
                #[cfg(not(target_os = "macos"))]
                let native_quit_pending = false;
                if state.quit_requested.get() || native_quit_pending {
                    // Quit accepted during the save barrier takes priority
                    // while this frontend is still attached to its host.
                    state.closing.set(false);
                    quit_app(cx);
                    return;
                }
                // Commit on the UI thread, without an intervening await. A
                // later Quit cannot turn this detach into a profile-wide stop.
                state.frontend_detached.set(true);
                let detach = state.backend.detach();
                cx.spawn(async move |cx| {
                    if let Err(error) = detach.await {
                        log::warn!("Maple frontend detach ended before acknowledgement: {error}");
                    }
                    cx.update(|cx| {
                        #[cfg(target_os = "macos")]
                        native_quit::complete_frontend_detach(cx);
                        #[cfg(not(target_os = "macos"))]
                        cx.quit();
                    });
                })
                .detach();
            }),
            result => {
                state.closing.set(false);
                cancel_native_quit();
                log::warn!("Could not save Maple settings before closing its window: {result:?}");
                cx.update(|cx| {
                    show_quit_warning(
                        "Maple could not save settings before closing. Keep Maple open and retry."
                            .into(),
                        cx,
                    )
                });
            }
        }
    })
    .detach();
}

/// A prior accepted Stop can finish after its ownership wait timed out. The
/// frontend can still cache Open, so a retry's save probe may hit a disconnected
/// socket. Only authoritative owner release lets that retry complete. Failed
/// saves with a live or unverified owner never cause another Stop or discard.
async fn finish_quit_shutdown<Flush, Stopped, Stop>(
    flush: Flush,
    stopped: Stopped,
    stop: Stop,
) -> Result<(), String>
where
    Flush: std::future::Future<Output = Result<(), String>>,
    Stopped: std::future::Future<Output = Result<bool, String>>,
    Stop: std::future::Future<Output = Result<(), String>>,
{
    match flush.await {
        Ok(()) => stop.await,
        Err(error) => match stopped.await {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(error),
        },
    }
}

fn cancel_native_quit() {
    #[cfg(target_os = "macos")]
    native_quit::cancel();
}

fn cancel_quit(cx: &mut App) {
    if let Some(state) = cx.try_global::<DesktopLifecycle>() {
        state.closing.set(false);
        state.quit_requested.set(false);
    }
    cancel_native_quit();
}

/// A separate window also works when Quit comes from a tray with no open window.
fn show_quit_prompt(kind: QuitPromptKind, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(560.), px(270.)), cx);
    if let Err(error) = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |window, cx| {
            window.on_window_should_close(cx, |_, cx| {
                cancel_quit(cx);
                true
            });
            cx.new(|_| QuitPrompt { kind })
        },
    ) {
        cancel_quit(cx);
        log::error!("Could not show Quit confirmation: {error}");
    }
    cx.activate(true);
}

fn show_quit_warning(message: String, cx: &mut App) {
    show_quit_prompt(QuitPromptKind::Failed(message), cx);
}

enum QuitPromptKind {
    Confirm(HostActivity),
    UnknownActivity,
    Failed(String),
}
struct QuitPrompt {
    kind: QuitPromptKind,
}
impl Render for QuitPrompt {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let (title, message, retry) = match &self.kind {
            QuitPromptKind::Confirm(activity) => (
                "Stop this computer's Maple host?",
                format!("Quitting stops {} running tasks, {} attached editors, {} remote controllers and {} background jobs. It also stops this computer's local API and its {} active requests. Maple waits for them to drain before closing.", activity.running_tasks, activity.acp_clients, activity.remote_controllers, activity.background_tasks, activity.local_api_requests),
                false,
            ),
            QuitPromptKind::Failed(message) => ("Maple is still open", message.clone(), true),
            QuitPromptKind::UnknownActivity => (
                "Stop this computer's Maple host?",
                "The host could not report its activity. Quitting will attempt to stop any attached editors, tasks and remote connections, and wait for the host to release local data.".into(),
                false,
            ),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .bg(gpui::rgb(ui::theme::bg_app()))
            .text_color(gpui::rgb(ui::theme::text_primary()))
            .child(div().text_lg().child(title))
            .child(message)
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(
                        ui::widgets::secondary_button("keep-maple-open")
                            .debug_selector(|| "keep-maple-open".to_string())
                            .child("Keep open")
                            .on_click(|_, window, cx| {
                                cancel_quit(cx);
                                window.remove_window();
                            }),
                    )
                    .child(
                        ui::widgets::secondary_button("retry-maple-quit")
                            .debug_selector(|| "retry-maple-quit".to_string())
                            .child(if retry { "Retry Quit" } else { "Quit Maple" })
                            .on_click(move |_, window, cx| {
                                window.remove_window();
                                if retry {
                                    quit_app(cx);
                                } else if let Some(state) =
                                    cx.try_global::<DesktopLifecycle>().cloned()
                                {
                                    drain_and_quit(state, cx);
                                }
                            }),
                    ),
            )
    }
}

/// Check current host mode rather than a possibly stale event flag. Closing
/// immediately after setting a password must hide the window, keeping its key
/// owner resident, even before a polling update reaches the GUI.
pub(crate) fn close_window(window: &mut Window, cx: &mut App) {
    let Some(state) = cx.try_global::<DesktopLifecycle>().cloned() else {
        finish_quit(cx);
        return;
    };
    if state.updater.restart_path().is_some() {
        quit_app(cx);
        return;
    }
    if !may_finish_quit(cx) {
        show_update_about(cx);
        return;
    }
    if state.closing.replace(true) {
        return;
    }
    if state.backend.store_state() == maple_agent::store::StoreState::Open {
        persist_window_state();
    }
    let backend = state.backend.clone();
    let query = backend.clone().spawn(async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            backend.local_control::<DesktopStoreStatus>("store_status", ()),
        )
        .await
        .map_err(|_| "The host did not answer its store status check".to_string())?
    });
    window.spawn(cx, async move |cx| {
        let result = query.await;
        let resident = match result {
            Ok(Ok(status)) => {
                state.store_resident.store(status.keeps_resident(), std::sync::atomic::Ordering::Release);
                // Make the tray visible before removing the last window,
                // including a close immediately after password setup.
                let _ = cx.update(|window, cx| {
                    if let Some(Some(root)) = window.root::<MapleApp>() {
                        root.update(cx, |app, cx| {
                            app.apply_store_completion_notice(&status, cx);
                            app.update_tray();
                        });
                    }
                });
                state.store_resident.load(std::sync::atomic::Ordering::Acquire)
                    || state.remote_enabled.load(std::sync::atomic::Ordering::Acquire)
            }
            result => {
                state.closing.set(false);
                log::warn!("Could not check Maple host before closing window: {result:?}");
                let _ = cx.update(|_, cx| show_quit_warning("Maple could not check its local data state. Keep this window open and retry.".into(), cx));
                return;
            }
        };
        if !resident {
            let _ = cx.update(|_, cx| detach_and_close(state, cx));
            return;
        }
        #[cfg(target_os = "macos")]
        {
            state.closing.set(false);
            let _ = cx.update(|_, cx| cx.hide());
        }
        #[cfg(not(target_os = "macos"))]
        {
            let can_background = state.can_background;
            let check = state.backend.spawn(async move {
                #[cfg(target_os = "linux")]
                { can_background && crate::tray::usable().await }
                #[cfg(target_os = "windows")]
                { can_background }
            });
            let usable = check.await.unwrap_or(false);
            state.closing.set(false);
            let prompt = cx.update(|window, cx| {
                if usable { window.remove_window(); None } else {
                    Some(window.prompt(gpui::PromptLevel::Warning, "Keep Maple open?",
                        Some("This desktop has no available tray to reopen Maple. Keep this window open while Maple holds local data unlocked, or Quit Maple to stop its host."),
                        &["Keep open", "Quit Maple"], cx))
                }
            }).ok().flatten();
            if let Some(prompt) = prompt && prompt.await == Ok(1) {
                let _ = cx.update(|_, cx| quit_app(cx));
            }
        }
    }).detach();
}

fn install_close_behavior(
    window: &mut Window,
    cx: &mut App,
    remote_enabled: Arc<std::sync::atomic::AtomicBool>,
    can_background: bool,
) {
    if cx.has_global::<DesktopLifecycle>() {
        let state = cx.global_mut::<DesktopLifecycle>();
        state.remote_enabled = remote_enabled;
        state.can_background = can_background;
    }
    window.on_window_should_close(cx, |window, cx| {
        close_window(window, cx);
        false
    });
}

/// Shown only if a paint happens before `run` picks the first screen,
/// which places it in the same event-loop turn as the window open.
fn restoring_view() -> gpui::Div {
    div()
        .relative()
        .flex_1()
        .min_h_0()
        .flex()
        .justify_center()
        .items_center()
        .bg(gpui::rgb(ui::theme::bg_app()))
        .child(ui::titlebar::drag_strip())
        .child(
            div()
                .text_sm()
                .text_color(gpui::rgb(ui::theme::text_secondary()))
                .child("Signing in…"),
        )
}

/// Last seen window geometry, read back when the app quits.
static WINDOW_STATE: std::sync::Mutex<Option<crate::settings::WindowState>> =
    std::sync::Mutex::new(None);

/// The UUID of the display the window was last seen on. Render runs every
/// frame, so the platform lookup only reruns when the display changes.
static WINDOW_DISPLAY: std::sync::Mutex<Option<(gpui::DisplayId, Option<String>)>> =
    std::sync::Mutex::new(None);

/// Runs on every root render, which includes every move, resize and maximize.
fn remember_window_state(window: &Window, cx: &App) {
    let bounds = window.window_bounds();
    let Some(mut state) = window_state_for(bounds) else {
        return;
    };
    let origin = bounds.get_bounds().origin;
    state.position = window.display(cx).and_then(|display| {
        let id = display.id();
        let mut cached = WINDOW_DISPLAY.lock().ok()?;
        if cached.as_ref().is_none_or(|(seen, _)| *seen != id) {
            *cached = Some((id, display.uuid().ok().map(|uuid| uuid.to_string())));
        }
        Some(crate::settings::WindowPosition {
            x: f32::from(origin.x),
            y: f32::from(origin.y),
            display: cached.as_ref()?.1.clone()?,
        })
    });
    if let Ok(mut slot) = WINDOW_STATE.lock() {
        *slot = Some(state);
    }
}

/// Opening bounds for the main window: the saved size and position, or a
/// centered default. Position applies only while its display is attached.
fn main_window_placement(
    saved: Option<crate::settings::WindowState>,
    cx: &App,
) -> (WindowBounds, Option<gpui::DisplayId>) {
    let saved = saved.map(crate::settings::WindowState::clamped);
    let window_size = saved
        .as_ref()
        .map(|state| size(px(state.width), px(state.height)))
        .unwrap_or_else(|| size(px(1280.), px(860.)));
    let placed = saved.as_ref().and_then(|state| {
        let position = state.position.as_ref()?;
        let display = saved_display(position, cx)?;
        Some((
            Bounds::new(point(px(position.x), px(position.y)), window_size),
            display.id(),
        ))
    });
    let (bounds, display) = match placed {
        Some((bounds, display)) => (bounds, Some(display)),
        None => (Bounds::centered(None, window_size, cx), None),
    };
    let bounds = if saved.is_some_and(|state| state.maximized) {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    };
    (bounds, display)
}

fn saved_display(
    position: &crate::settings::WindowPosition,
    cx: &App,
) -> Option<std::rc::Rc<dyn gpui::PlatformDisplay>> {
    cx.displays().into_iter().find(|display| {
        display
            .uuid()
            .is_ok_and(|uuid| uuid.to_string() == position.display)
    })
}

/// The state worth saving for `bounds`. Fullscreen yields `None`: its
/// bounds are the monitor, and saving them as "maximized" made the next
/// launch open maximized instead of at the last windowed size.
fn window_state_for(bounds: WindowBounds) -> Option<crate::settings::WindowState> {
    let (bounds, maximized) = match bounds {
        WindowBounds::Windowed(bounds) => (bounds, false),
        WindowBounds::Maximized(bounds) => (bounds, true),
        WindowBounds::Fullscreen(_) => return None,
    };
    Some(crate::settings::WindowState {
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
        maximized,
        position: None,
    })
}

/// Queue the last seen window state for the host-owned settings store.
fn persist_window_state() {
    let state = WINDOW_STATE.lock().ok().and_then(|slot| slot.clone());
    if let Some(state) = state {
        crate::settings::save_window_state(state);
    }
}

/// Every key binding the window uses. Tests register the same set, so
/// what they exercise is what ships.
#[cfg(test)]
pub(crate) fn register_key_bindings(cx: &mut App) {
    crate::keymap::bootstrap(cx);
}

fn main_window_options(
    (window_bounds, display_id): (WindowBounds, Option<gpui::DisplayId>),
) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(window_bounds),
        display_id,
        // X11 WM_CLASS and Wayland app_id must match the packaged desktop
        // entry, so Dev, Prod, Research, and local debug windows group apart.
        app_id: Some(crate::profile::BUNDLE_ID.to_string()),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(ui::titlebar::WINDOW_TITLE.into()),
            appears_transparent: ui::titlebar::TRANSPARENT_TITLEBAR,
            traffic_light_position: ui::titlebar::TRANSPARENT_TITLEBAR
                .then_some(ui::titlebar::TRAFFIC_LIGHT_POSITION),
        }),
        app_owns_titlebar_drag: ui::titlebar::TRANSPARENT_TITLEBAR,
        ..Default::default()
    }
}

struct StartupError {
    message: String,
    stopping: bool,
    retry_backend: Option<Arc<AgentBackend>>,
    requires_completed_stop: bool,
}

/// A native-hook startup failure must keep its newly owned host's stop proof
/// as the authority for Close and application Quit, independent of residency.
#[cfg(target_os = "macos")]
#[derive(Clone)]
struct GuardedStartupFailure(Entity<StartupError>);
#[cfg(target_os = "macos")]
impl gpui::Global for GuardedStartupFailure {}

impl StartupError {
    fn new(message: String) -> Self {
        Self {
            message,
            stopping: false,
            retry_backend: None,
            requires_completed_stop: false,
        }
    }

    #[cfg(target_os = "macos")]
    fn requiring_host_stop(message: String, backend: Arc<AgentBackend>) -> Self {
        Self {
            message,
            stopping: false,
            retry_backend: Some(backend),
            requires_completed_stop: true,
        }
    }

    fn request_close(&mut self, cx: &mut Context<Self>) {
        if !self.close_is_allowed() {
            self.retry_required_stop(true, cx);
        } else {
            cx.quit();
        }
    }

    fn close_is_allowed(&self) -> bool {
        !self.requires_completed_stop
    }

    fn finish_required_stop(&mut self, result: Result<(), String>) -> bool {
        self.stopping = false;
        match result {
            Ok(()) => {
                self.requires_completed_stop = false;
                self.retry_backend = None;
                self.message =
                    "The Maple host has stopped. You can close Maple and try starting it again."
                        .into();
                true
            }
            Err(error) => {
                self.message = format!(
                    "Maple could not finish stopping its host: {error}. Keep this window open and retry stopping the host before relaunching Maple."
                );
                false
            }
        }
    }

    fn retry_required_stop(&mut self, quit_when_stopped: bool, cx: &mut Context<Self>) {
        if !self.requires_completed_stop || self.stopping {
            return;
        }
        let Some(backend) = self.retry_backend.clone() else {
            // Losing the retry handle is not evidence that the owner lock was
            // released. Keep the window and its completed-stop guard intact.
            self.message = "Maple cannot retry stopping its host. Keep this process open until the host has been stopped.".into();
            cx.notify();
            return;
        };
        self.stopping = true;
        let stop_backend = backend.clone();
        let stop =
            backend.spawn(async move { crate::host::request_stop(stop_backend.api_url()).await });
        cx.spawn(async move |this, cx| {
            let result = stop
                .await
                .unwrap_or_else(|_| Err("The host stop request did not finish".into()));
            let _ = this.update(cx, |this, cx| {
                if this.finish_required_stop(result) {
                    if let Some(state) = cx.try_global::<DesktopLifecycle>() {
                        state.quit_completed.set(true);
                    }
                    if quit_when_stopped {
                        cx.quit();
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn stop_old_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stopping {
            return;
        }
        let answer = window.prompt(gpui::PromptLevel::Warning, "Stop the existing Maple host?",
            Some("This stops running tasks and remote access for this profile. Maple will reopen with this build."),
            &["Cancel", "Stop and reopen"], cx);
        self.stopping = true;
        cx.spawn(async move |this, cx| {
            let result = if answer.await == Ok(1) {
                Some(
                    cx.background_executor()
                        .spawn(async move {
                            crate::host::stop(&crate::configured_api_url())?;
                            crate::self_launch::command()?
                                .spawn()
                                .map_err(|e| e.to_string())?;
                            Ok::<(), String>(())
                        })
                        .await,
                )
            } else {
                None
            };
            let _ = this.update(cx, |this, cx| {
                this.stopping = false;
                match result {
                    Some(Ok(())) => finish_quit(cx),
                    Some(Err(error)) => this.message = format!("Could not restart Maple: {error}"),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for StartupError {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .bg(gpui::rgb(ui::theme::bg_app()))
            .text_color(gpui::rgb(ui::theme::text_primary()))
            .child(div().text_lg().child("Maple could not start"))
            .child(self.message.clone())
            .when(
                !self.requires_completed_stop && self.message.contains("Incompatible Maple host or profile"),
                |view| {
                    view.child(
                        ui::widgets::secondary_button("stop-incompatible-host")
                            .child(if self.stopping {
                                "Stopping…"
                            } else {
                                "Stop existing host and reopen"
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.stop_old_host(window, cx)),
                            ),
                    )
                },
            )
            .when(self.requires_completed_stop, |view| {
                view.child(div().text_sm().child("Keep Maple open until its host has finished stopping. Local data may remain unlocked while it is still running."))
                    .child(ui::widgets::secondary_button("retry-startup-host-stop")
                        .child(if self.stopping { "Stopping host…" } else { "Retry stopping host" })
                        .when(self.stopping, |button| button.opacity(0.5))
                        .on_click(cx.listener(|this, _, _, cx| this.retry_required_stop(false, cx))))
            })
            .when(self.close_is_allowed(), |view| view.child(
                div()
                    .id("close-startup-error")
                    .cursor_pointer()
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(gpui::rgb(ui::theme::bg_elevated()))
                    .child("Close")
                    .on_click(cx.listener(|this, _, _, cx| this.request_close(cx))),
            ))
    }
}

fn show_startup_error(error: String) {
    #[cfg(target_os = "linux")]
    let restart_path = crate::self_launch::executable().ok();
    gpui_platform::application().run(move |cx| {
        cx.set_app_identity(crate::profile::BUNDLE_ID, crate::profile::DISPLAY_NAME);
        #[cfg(target_os = "linux")]
        if let Some(path) = restart_path {
            cx.set_restart_path(path);
        }
        cx.on_action(|_: &QuitApp, cx| quit_app(cx));
        ui::menus::install(cx);
        let bounds = Bounds::centered(None, size(px(560.), px(260.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some(crate::profile::BUNDLE_ID.into()),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(crate::profile::DISPLAY_NAME.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                window.on_window_should_close(cx, |_, cx| {
                    finish_quit(cx);
                    true
                });
                cx.new(|_| StartupError::new(error))
            },
        );
        if let Err(error) = result {
            log::error!("Could not show the startup error: {error}");
            finish_quit(cx);
        } else {
            cx.activate(true);
        }
    });
}

fn dispatch_update_action(action: crate::update::UpdateAction, cx: &mut App) {
    for window in cx.windows() {
        let _ = window.update(cx, |root, window, cx| {
            if let Ok(app) = root.downcast::<MapleApp>() {
                app.update(cx, |app, cx| app.update_action(action, window, cx));
            }
        });
    }
}

fn show_update_about(cx: &mut App) {
    for window in cx.windows() {
        let _ = window.update(cx, |root, window, cx| {
            if let Ok(app) = root.downcast::<MapleApp>() {
                window.activate_window();
                app.update(cx, |app, cx| app.show_about(window, cx));
            }
        });
    }
}

pub fn run() {
    crate::init_logging(crate::LogOutput::FileAndStderr);
    log::debug!("startup: logging ready at {} ms", crate::startup_elapsed());
    #[cfg(target_os = "linux")]
    let restart_path = match crate::self_launch::executable() {
        Ok(path) => path,
        Err(error) => {
            log::error!("Could not start Maple: {error}");
            show_startup_error(error);
            return;
        }
    };

    // The frontend starts with defaults until the host's bounded settings
    // snapshot arrives. Locked startup never reads saved application data.
    let startup_settings = crate::settings::load_settings();
    let backend = match AgentBackend::new(
        crate::configured_api_url(),
        startup_settings.effective_harness_instructions(),
    ) {
        Ok(backend) => Arc::new(backend),
        Err(error) => {
            log::error!("Could not start Maple: {error}");
            show_startup_error(error);
            return;
        }
    };
    log::debug!("startup: backend ready at {} ms", crate::startup_elapsed());

    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .run(move |cx: &mut App| {
            cx.set_app_identity(crate::profile::BUNDLE_ID, crate::profile::DISPLAY_NAME);
            #[cfg(target_os = "linux")]
            cx.set_restart_path(restart_path);
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            log::debug!("startup: gpui app ready at {} ms", crate::startup_elapsed());
            if let Err(error) = cx.text_system().add_fonts(
                crate::assets::FONTS
                    .iter()
                    .map(|bytes| std::borrow::Cow::Borrowed(*bytes))
                    .collect(),
            ) {
                log::warn!("failed to register bundled fonts: {error}");
            }
            ui::spell::preload();
            cx.on_action(|_: &QuitApp, cx| quit_app(cx));
            // Menu actions dispatch while the active window is borrowed.
            // Re-enter its view only after the dispatch has unwound.
            cx.on_action(|_: &crate::update::AboutAgent, cx| cx.defer(show_update_about));
            cx.on_action(|_: &crate::update::CheckForUpdates, cx| {
                cx.defer(|cx| {
                    show_update_about(cx);
                    dispatch_update_action(crate::update::UpdateAction::Check, cx);
                });
            });
            #[cfg(target_os = "macos")]
            {
                cx.on_action(|_: &Hide, cx| cx.hide());
                cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
                cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
            }
            let shortcut_runtime = crate::shortcuts::ShortcutRuntime::bootstrap(
                &startup_settings.shortcut_overrides,
                cx,
            );
            ui::theme::apply_preference(ui::theme::Preference::parse(&startup_settings.theme), cx);
            #[cfg(target_os = "linux")]
            crate::platform::portal::start(&backend, cx);
            ui::typography::apply(
                ui::typography::ChatFontFamily::parse(&startup_settings.chat_font_family),
                startup_settings.chat_font_size,
            );
            ui::menus::install(cx);
            // A click on a notification brings the app forward and shows
            // the task it was about (tags are `task:<session id>`; see
            // `ChatScreen::notify_desktop`).
            cx.on_system_notification_response(|response, cx| {
                cx.activate(true);
                let session_id = response.tag.strip_prefix("task:").map(str::to_owned);
                let remote_task = response
                    .tag
                    .strip_prefix("remote-task:")
                    .and_then(|tag| tag.split_once(':'))
                    .map(|(host, task)| (host.to_owned(), task.to_owned()));
                for window in cx.windows() {
                    window
                        .update(cx, |root, window, cx| {
                            window.activate_window();
                            if let Ok(app) = root.downcast::<MapleApp>() {
                                app.update(cx, |app, cx| {
                                    if let Some((host, task)) = &remote_task {
                                        app.navigate(
                                            Navigate {
                                                computer: Some(host.clone()),
                                                destination: Destination::Task(task.clone()),
                                            },
                                            cx,
                                        );
                                    } else if let Some(task) = &session_id {
                                        app.navigate(
                                            Navigate {
                                                computer: None,
                                                destination: Destination::Task(task.clone()),
                                            },
                                            cx,
                                        );
                                    }
                                    cx.notify();
                                });
                            }
                        })
                        .ok();
                }
            });
            cx.set_reduce_motion(startup_settings.reduce_motion);
            let window_placement = main_window_placement(startup_settings.window.clone(), cx);
            let updater = crate::update::Controller::new(false);
            let mut update_events = updater.subscribe();
            let remote_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let store_resident = Arc::new(std::sync::atomic::AtomicBool::new(false));
            cx.set_global(DesktopLifecycle {
                backend: backend.clone(),
                updater: updater.clone(),
                remote_enabled: remote_enabled.clone(),
                store_resident: store_resident.clone(),
                can_background: false,
                closing: Default::default(),
                quit_completed: Default::default(),
                quit_requested: Default::default(),
                frontend_detached: Default::default(),
                #[cfg(target_os = "windows")]
                update_restart_pending: Default::default(),
            });
            #[cfg(target_os = "macos")]
            if let Err(error) = native_quit::install(cx) {
                // This view has no encryption controls and cannot close or
                // take the ordinary Quit path until request_stop proves EOF
                // and owner-lock release. The drain stays off the UI thread.
                log::error!("{error}");
                let startup = cx.new(|_| StartupError::requiring_host_stop(error, backend.clone()));
                cx.set_global(GuardedStartupFailure(startup.clone()));
                let view = startup.clone();
                let bounds = Bounds::centered(None, size(px(620.), px(320.)), cx);
                let opened = cx.open_window(
                    main_window_options((WindowBounds::Windowed(bounds), None)),
                    move |window, cx| {
                        let closing = view.clone();
                        window.on_window_should_close(cx, move |_, cx| {
                            closing.update(cx, |startup, cx| startup.request_close(cx));
                            false
                        });
                        view
                    },
                );
                let quit_after_stop = opened.is_err();
                if let Err(error) = opened {
                    log::error!("Could not show the guarded startup error: {error}");
                }
                startup.update(cx, |startup, cx| {
                    startup.retry_required_stop(quit_after_stop, cx)
                });
                cx.activate(true);
                return;
            }
            cx.on_app_quit(|cx| {
                // Host-stop Quit and a saved frontend-only detach complete
                // before AppKit/GPUI gets here. This callback is diagnostic
                // only: GPUI allows 200 ms, too little to own either operation.
                let expected = cx.try_global::<DesktopLifecycle>().is_some_and(|state| {
                    state.quit_completed.get() || state.frontend_detached.get()
                });
                let cleanup = cx.try_global::<DesktopLifecycle>().map(|state| {
                    state.updater.shutdown();
                    state.updater.cleanup_on_exit(state.backend.clone())
                });
                async move {
                    if !expected {
                        log::warn!(
                            "Maple frontend terminated outside completed Quit or intentional detach"
                        );
                    }
                    if let Some(cleanup) = cleanup {
                        // Best effort only; durable SQL shutdown was completed
                        // by the coordinator before this 200 ms GPUI callback.
                        // Only the JoinHandle is polled by GPUI; the cleanup's
                        // timer and filesystem work run on the backend runtime.
                        let _ = cleanup.await;
                    }
                }
            })
            .detach();
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            let (tray, tray_events) = match crate::tray::StatusIcon::new() {
                Ok((tray, events)) => (Some(tray), Some(events)),
                Err(error) => {
                    log::warn!("{error}");
                    (None, None)
                }
            };
            let root_enabled = remote_enabled.clone();
            let root_resident = store_resident.clone();
            let root_backend = backend.clone();
            let root_settings = startup_settings.clone();
            let root_updater = updater.clone();
            let window = cx
                .open_window(main_window_options(window_placement), move |_, cx| {
                    cx.new(move |cx| MapleApp {
                        backend: root_backend,
                        screen: Screen::Restoring,
                        user_id: None,
                        parked_chat: None,
                        local_chat: None,
                        selected_host: None,
                        remote_computers: Default::default(),
                        remote_inventory_loaded: false,
                        next_connection_attempt: 0,
                        requested_navigation: None,
                        computers_sync_pending: false,
                        remote_enabled: root_enabled,
                        store_resident: root_resident,
                        store_load_epoch: 0,
                        store_ready: false,
                        store_id: None,
                        store_notice: None,
                        store_notice_is_error: false,
                        seen_store_notice_operation: None,
                        inline_password_completion: None,
                        mode_change_notice: None,
                        inline_mode_notice: None,
                        password_settings_refresh_pending: false,
                        password_settings_refreshing: false,
                        persistence_status: maple_agent::store::PersistenceStatus::default(),
                        persistence_retrying: false,
                        #[cfg(feature = "proxy")]
                        local_api_revision: 0,
                        #[cfg(feature = "proxy")]
                        local_api_host_epoch: 0,
                        #[cfg(feature = "proxy")]
                        local_api_refreshing: false,
                        #[cfg(feature = "proxy")]
                        local_api_refresh_again: false,
                        pending_store_attention: Default::default(),
                        store_attention_open: false,
                        restore_window_geometry: true,
                        #[cfg(any(target_os = "linux", target_os = "windows"))]
                        tray,
                        remote_bridges: std::cell::RefCell::new(Vec::new()),
                        settings: root_settings,
                        updater: root_updater,
                        restart_prompt_pending: false,
                        #[cfg(target_os = "linux")]
                        restart_confirmation: None,
                        shortcuts: shortcut_runtime,
                        titlebar: cx.new(|_| TitleBar::new(ui::titlebar::WINDOW_TITLE)),
                    })
                })
                .expect("failed to open main window");
            log::debug!("startup: window open at {} ms", crate::startup_elapsed());
            window
                .update(cx, |app: &mut MapleApp, _window, cx| {
                    app.show_store_state(backend.store_state(), cx);
                })
                .ok();
            // Poll/checkpoint only after current-store settings bootstrap.
            let root_window = window;
            cx.spawn(async move |cx| {
                while update_events.changed().await.is_ok() {
                    let snapshot = update_events.borrow_and_update().clone();
                    if root_window
                        .update(cx, |app: &mut MapleApp, _, cx| app.set_update(snapshot, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            let root = window
                .update(cx, |app, window, cx| {
                    // The only window: closing it from the window manager
                    // ends the app the same way the title bar button does.
                    #[cfg(target_os = "macos")]
                    let can_background = {
                        let _ = app;
                        true
                    };
                    #[cfg(any(target_os = "linux", target_os = "windows"))]
                    let can_background = app.tray.is_some();
                    install_close_behavior(window, cx, remote_enabled.clone(), can_background);
                    // A system theme change must reach every view.
                    window
                        .observe_window_appearance(|_window, cx| cx.refresh_windows())
                        .detach();
                    cx.entity()
                })
                .expect("root entity");

            // Local status is cheap and does not contact the enrollment
            // service. This also notices changes made through the CLI.
            let status_backend = backend.clone();
            let status_root = root.clone();
            let remote_updater = updater.clone();
            let update_bridge = remote_updates::Bridge::default();
            cx.spawn(async move |cx| {
                loop {
                    let (account, epoch, owner) = status_root.update(cx, |app, _| {
                        (
                            app.user_id.clone(),
                            app.store_load_epoch,
                            app.local_chat.as_ref().map(Entity::entity_id),
                        )
                    });
                    let backend = status_backend.clone();
                    let expected_account = account.clone();
                    let bridge = update_bridge.clone();
                    let update_status = remote_updater.remote_status();
                    let check_updater = remote_updater.clone();
                    let task = status_backend.spawn(async move {
                        tokio::join!(
                            backend.remote_setting::<crate::remote::manager::Status>(
                                "remote_status",
                                ()
                            ),
                            backend.local_control::<DesktopStoreStatus>("store_status", ()),
                            bridge.poll(
                                backend.clone(),
                                expected_account,
                                update_status,
                                check_updater
                            ),
                        )
                    });
                    if let Ok((remote, store, update_command)) = task.await {
                        status_root.update(cx, |app, cx| {
                            if let Ok(store) = store {
                                app.apply_store_completion_notice(&store, cx);
                                app.set_store_resident(store.keeps_resident(), cx);
                                if store.state == maple_agent::store::StoreState::Open
                                    && app.password_settings_refresh_pending
                                    && !app.password_settings_refreshing
                                {
                                    app.refresh_password_settings(cx);
                                }
                            }
                            if let Some(command) = update_command {
                                app.begin_remote_install(command, cx);
                            }
                            let Ok(status) = remote else { return };
                            if app.user_id == account
                                && account.is_some()
                                && app.store_load_epoch == epoch
                                && app.local_chat.as_ref().map(Entity::entity_id) == owner
                                && app.store_ready
                                && app.backend.store_state() == maple_agent::store::StoreState::Open
                            {
                                if let Screen::Settings(settings) = &app.screen {
                                    settings.update(cx, |settings, cx| {
                                        settings.update_remote_status(status.clone(), cx)
                                    });
                                }
                                app.update_computers(status, cx);
                            }
                        });
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(5))
                        .await;
                }
            })
            .detach();
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            if let Some(mut events) = tray_events {
                let tray_root = root.clone();
                let enabled = remote_enabled.clone();
                cx.spawn(async move |cx| {
                    while let Some(action) = events.recv().await {
                        match action {
                            crate::tray::Action::Quit => {
                                cx.update(quit_app);
                            }
                            crate::tray::Action::CheckForUpdates => {
                                cx.update(|cx| {
                                    dispatch_update_action(crate::update::UpdateAction::Check, cx)
                                });
                            }
                            crate::tray::Action::Open => {
                                cx.update(|cx| {
                                    if cx.windows().is_empty() {
                                        let root = tray_root.clone();
                                        let enabled = enabled.clone();
                                        let placement = main_window_placement(
                                            crate::settings::load_settings().window,
                                            cx,
                                        );
                                        let _ = cx.open_window(
                                            main_window_options(placement),
                                            move |window, cx| {
                                                install_close_behavior(window, cx, enabled, true);
                                                root
                                            },
                                        );
                                    } else {
                                        for window in cx.windows() {
                                            let _ = window.update(cx, |_, window, _| {
                                                window.activate_window()
                                            });
                                        }
                                    }
                                });
                            }
                        }
                    }
                })
                .detach();
            }

            // The event pump runs once for the whole process and routes events to
            // whichever screen is active. It exits when the root entity is gone.
            let (spawn_backend, take_backend) = (backend.clone(), backend.clone());
            let rx = spawn_backend.spawn(async move { take_backend.take_events().await });
            cx.spawn(async move |cx| {
                let Some(mut rx) = rx.await.ok().flatten() else {
                    return;
                };
                while let Some(event) = rx.recv().await {
                    // Drain whatever else is queued so a burst of streaming
                    // chunks costs one update and one render, not one each.
                    let mut batch = vec![event];
                    while let Ok(next) = rx.try_recv() {
                        batch.push(next);
                        if batch.len() >= 256 {
                            break;
                        }
                    }
                    // This gpui's entity update is infallible; the pump ends
                    // with the channel instead.
                    root.update(cx, |app, cx| app.handle_service_events(batch, cx));
                }
            })
            .detach();

            cx.activate(true);
        });
}

#[cfg(test)]
mod maintenance_tests;
#[cfg(test)]
mod remote_sidebar_tests;

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn quit_retry_accepts_disconnected_flush_only_after_verified_owner_release() {
        let stop_calls = std::cell::Cell::new(0);
        let result = super::finish_quit_shutdown(
            async { Err("The host disconnected".to_string()) },
            async { Ok(true) },
            async {
                stop_calls.set(stop_calls.get() + 1);
                Ok(())
            },
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(stop_calls.get(), 0);
    }

    #[tokio::test]
    async fn failed_save_keeps_live_or_unverified_owner_and_never_requests_stop() {
        for stopped in [Ok(false), Err("Ownership is unknown".to_string())] {
            let stop_calls = std::cell::Cell::new(0);
            let result = super::finish_quit_shutdown(
                async { Err("Unsaved source remains in the host".to_string()) },
                async { stopped },
                async {
                    stop_calls.set(stop_calls.get() + 1);
                    Ok(())
                },
            )
            .await;
            assert_eq!(result.unwrap_err(), "Unsaved source remains in the host");
            assert_eq!(stop_calls.get(), 0);
        }
    }

    #[tokio::test]
    async fn successful_flush_still_requires_completed_stop() {
        let stop_calls = std::cell::Cell::new(0);
        let stopped_probes = std::cell::Cell::new(0);
        let result = super::finish_quit_shutdown(
            async { Ok(()) },
            async {
                stopped_probes.set(stopped_probes.get() + 1);
                Ok(false)
            },
            async {
                stop_calls.set(stop_calls.get() + 1);
                Err("The owner is still draining".to_string())
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), "The owner is still draining");
        assert_eq!(stop_calls.get(), 1);
        assert_eq!(stopped_probes.get(), 0);
    }

    #[test]
    fn disk_full_guidance_keeps_unsaved_open_sessions_alive_and_allows_restart_after_failed_open() {
        let failure = maple_agent::store::PersistenceFailure {
            store_id: Some("fixture-generation".into()),
            revision: 1,
            reason: maple_agent::store::PersistenceFailureReason::DiskFull,
        };
        let open = super::persistence_feedback_message(&failure, true);
        assert!(open.contains("Keep Maple open"));
        assert!(open.contains("retry saving"));
        assert!(!open.contains("quit and reopen"));
        let unopened = super::persistence_feedback_message(&failure, false);
        assert!(unopened.contains("quit and reopen Maple"));
        assert!(unopened.contains("unlock again if prompted"));
    }

    #[test]
    fn computer_connection_retries_are_independent_and_not_started_twice() {
        let now = std::time::Instant::now();
        let mut computer = super::RemoteComputer {
            name: "Office".into(),
            grant: [0; 16],
            chat: None,
            attempt: super::ConnectionAttempt::Idle,
        };
        assert!(computer.should_connect(now));
        computer.attempt = super::ConnectionAttempt::Connecting(1);
        assert!(!computer.should_connect(now));
        computer.attempt = super::ConnectionAttempt::Failed {
            message: "Offline".into(),
            retry_at: now + std::time::Duration::from_secs(30),
        };
        assert!(!computer.should_connect(now));
        assert!(computer.should_connect(now + std::time::Duration::from_secs(30)));
        let other = super::RemoteComputer {
            name: "Laptop".into(),
            grant: [1; 16],
            chat: None,
            attempt: super::ConnectionAttempt::Idle,
        };
        assert!(other.should_connect(now));
    }

    #[test]
    fn cold_catalog_protocol_refusal_stops_only_automatic_mismatch_retries() {
        use crate::remote::wire::{Error, Refusal};

        let now = std::time::Instant::now();
        for (message, mismatch) in [
            (
                Refusal::terminal(Error::Incompatible.to_string()).into_error(),
                true,
            ),
            (Error::Disconnected.to_string(), false),
            (
                Refusal::terminal("Remote control grant is missing or revoked").into_error(),
                false,
            ),
            (
                Refusal::terminal(Error::InvalidFrame.to_string()).into_error(),
                false,
            ),
        ] {
            let computer = super::RemoteComputer {
                name: "Office".into(),
                grant: [7; 16],
                chat: None,
                attempt: super::ConnectionAttempt::Failed {
                    message: message.clone(),
                    retry_at: now + std::time::Duration::from_secs(30),
                },
            };
            assert!(!computer.should_connect(now));
            assert_eq!(
                computer.should_connect(now + std::time::Duration::from_secs(3600)),
                !mismatch,
                "{message}"
            );
            assert_eq!(
                computer.error().as_deref(),
                Some(if mismatch {
                    "Update required — select to retry after updating"
                } else {
                    &message
                })
            );
        }
    }

    #[gpui::test]
    fn cold_catalog_protocol_refusal_allows_explicit_navigation_retry(
        cx: &mut gpui::TestAppContext,
    ) {
        let app = project_shell(cx);
        app.update(cx, |app, cx| {
            let computer = app.remote_computers.get_mut("east").unwrap();
            computer.chat = None;
            computer.attempt = ConnectionAttempt::Failed {
                message: crate::remote::wire::Refusal::terminal(
                    crate::remote::wire::Error::Incompatible.to_string(),
                )
                .into_error(),
                retry_at: std::time::Instant::now(),
            };
            assert!(!computer.should_connect(std::time::Instant::now()));
            // Exercise dispatch, without allowing this fixture to connect to
            // a developer's host: the backend rejects before any IPC or spawn.
            app.backend.retire_after_update();
            app.navigate(
                Navigate {
                    computer: Some("east".into()),
                    destination: Destination::Overview,
                },
                cx,
            );
            assert_eq!(app.next_connection_attempt, 1);
            assert!(matches!(
                app.remote_computers["east"].attempt,
                ConnectionAttempt::Connecting(1)
            ));
            assert!(app.requested_navigation.is_some());
            assert_eq!(app.remote_computers["east"].grant, [1; 16]);
        });
    }

    #[test]
    fn password_and_incomplete_setup_remain_resident_when_window_closes() {
        use maple_agent::store::StoreState;
        let mut status = super::DesktopStoreStatus {
            state: StoreState::Open,
            mode: "password".into(),
            setup_operation_id: None,
            store_id: Some("generation".into()),
            completion_notice: None,
            mode_change_notice: None,
            persistence_status: maple_agent::store::PersistenceStatus::default(),
            local_api_running: false,
        };
        assert!(status.keeps_resident());
        status.state = StoreState::Locked;
        assert!(!status.keeps_resident());
        status.mode = "plain".into();
        status.state = StoreState::Maintenance;
        assert!(status.keeps_resident());
        status.state = StoreState::NeedsSetup;
        status.setup_operation_id = Some("incomplete-rekey".into());
        assert!(status.keeps_resident());
        status.setup_operation_id = None;
        status.state = StoreState::Open;
        assert!(!status.keeps_resident());
    }

    #[test]
    fn attention_notice_survives_form_loss_and_is_presented_once_per_operation() {
        let mut status: super::DesktopStoreStatus = serde_json::from_value(serde_json::json!({
            "state": {"state": "open"},
            "mode": "password",
            "completion_notice": {
                "operation_id": "first-operation",
                "message": "Keep both passwords until synchronization finishes.",
                "is_error": false, "requires_attention": true
            }
        }))
        .unwrap();
        let mut seen = None;
        assert_eq!(
            status.fresh_completion_notice(&mut seen).as_deref(),
            Some("Keep both passwords until synchronization finishes.")
        );
        // Dismissing the visible string must not make the host's next poll
        // redisplay its still-latched copy.
        assert!(status.fresh_completion_notice(&mut seen).is_none());
        status.completion_notice = None;
        assert!(status.fresh_completion_notice(&mut seen).is_none());
        status.completion_notice = Some(super::StoreCompletionNotice {
            operation_id: "second-operation".into(),
            message: "The operation could not finish.".into(),
            is_error: true,
            inline_password_success: false,
            requires_attention: true,
            ui_operation_id: None,
            mode_notice_id: None,
        });
        assert_eq!(
            status.fresh_completion_notice(&mut seen).as_deref(),
            Some("The operation could not finish.")
        );
    }

    #[test]
    fn store_status_without_completion_notice_remains_compatible() {
        let status: super::DesktopStoreStatus = serde_json::from_value(serde_json::json!({
            "state": {"state": "open"}, "mode": "plain"
        }))
        .unwrap();
        assert!(status.completion_notice.is_none());
        assert!(status.current_mode_notice().is_none());
    }

    #[test]
    fn routine_success_never_becomes_a_global_dialog_even_if_its_form_is_gone() {
        let status: super::DesktopStoreStatus = serde_json::from_value(serde_json::json!({
            "state":{"state":"open"},"mode":"password",
            "completion_notice":{"operation_id":"completed","message":"Encryption enabled.","is_error":false,
                "inline_password_success":true,"requires_attention":false,"ui_operation_id":"mine","mode_notice_id":"durable"}
        })).unwrap();
        assert!(status.matches_pending_password_operation(Some("mine")));
        assert!(!status.matches_pending_password_operation(Some("another-client")));
        let (mut seen, mut visible, mut error) = (None, None, false);
        assert!(!status.reconcile_password_completion_notice(
            &mut seen,
            &mut visible,
            &mut error,
            true,
            None
        ));
        assert!(seen.is_none());
        assert!(!status.reconcile_password_completion_notice(
            &mut seen,
            &mut visible,
            &mut error,
            false,
            None
        ));
        assert_eq!(seen.as_deref(), Some("completed"));
        assert!(visible.is_none());
        assert!(!error);
        seen = Some("newer-error".into());
        visible = Some("Saving still needs attention.".into());
        error = true;
        assert!(!status.reconcile_password_completion_notice(
            &mut seen,
            &mut visible,
            &mut error,
            false,
            Some("completed")
        ));
        assert_eq!(visible.as_deref(), Some("Saving still needs attention."));
        assert!(error);
    }

    #[test]
    fn reset_setup_and_recovery_successes_are_not_fallback_dialogs() {
        for message in [
            "Maple local data is ready.",
            "Local data was erased. Choose how to set up Maple.",
            "The local store repair finished.",
            "Readable local data was saved. The damaged copy was kept.",
        ] {
            let status: super::DesktopStoreStatus = serde_json::from_value(serde_json::json!({
                "state":{"state":"open"},"mode":"plain",
                "completion_notice":{"operation_id":"routine","message":message,"is_error":false,"requires_attention":false}
            })).unwrap();
            let mut seen = None;
            assert!(status.fresh_completion_notice(&mut seen).is_none());
            assert_eq!(seen.as_deref(), Some("routine"));
        }
    }

    #[test]
    fn successful_unlock_clears_prior_error_and_preserves_durable_and_current_notices() {
        let mut status: super::DesktopStoreStatus = serde_json::from_value(serde_json::json!({
            "state": {"state": "locked"}, "mode": "password", "store_id": "current",
            "completion_notice": {
                "operation_id": "wrong-password",
                "message": "That password didn't unlock Maple.", "is_error": true
            },
            "mode_change_notice": {
                "store_id": "current", "notice_id": "saved-mode-change",
                "message": "An unlock password was set on this computer."
            }
        }))
        .unwrap();
        let mut seen = None;
        let mut visible = None;
        let mut is_error = false;
        assert!(status.reconcile_completion_notice(&mut seen, &mut visible, &mut is_error));
        assert_eq!(
            visible.as_deref(),
            Some("That password didn't unlock Maple.")
        );
        assert!(is_error);

        // A successful unlock can arrive from another local client. Its Open
        // status has no volatile completion notice, but keeps the durable one.
        status.state = maple_agent::store::StoreState::Open;
        status.completion_notice = None;
        assert!(status.reconcile_completion_notice(&mut seen, &mut visible, &mut is_error));
        assert!(visible.is_none());
        assert!(!is_error);
        assert_eq!(
            status.current_mode_notice().unwrap().notice_id,
            "saved-mode-change"
        );

        // Failure of a later operation while already Open remains visible as
        // long as that actual failed result remains the host's current latch.
        status.completion_notice = Some(super::StoreCompletionNotice {
            operation_id: "failed-remove".into(),
            message: "The current password was incorrect.".into(),
            is_error: true,
            inline_password_success: false,
            requires_attention: true,
            ui_operation_id: None,
            mode_notice_id: None,
        });
        assert!(status.reconcile_completion_notice(&mut seen, &mut visible, &mut is_error));
        assert!(!status.reconcile_completion_notice(&mut seen, &mut visible, &mut is_error));
        assert_eq!(
            visible.as_deref(),
            Some("The current password was incorrect.")
        );
        assert!(is_error);

        // A committed operation's synchronization warning is not an obsolete
        // unlock error and must survive a status with no volatile result.
        status.completion_notice = Some(super::StoreCompletionNotice {
            operation_id: "committed-with-warning".into(),
            message: "Keep both passwords until synchronization finishes.".into(),
            is_error: false,
            inline_password_success: false,
            requires_attention: true,
            ui_operation_id: None,
            mode_notice_id: None,
        });
        assert!(status.reconcile_completion_notice(&mut seen, &mut visible, &mut is_error));
        status.completion_notice = None;
        assert!(!status.reconcile_completion_notice(&mut seen, &mut visible, &mut is_error));
        assert_eq!(
            visible.as_deref(),
            Some("Keep both passwords until synchronization finishes.")
        );
    }

    #[test]
    fn durable_mode_notice_survives_new_frontend_and_rejects_other_generation() {
        let input = serde_json::json!({
            "state": {"state": "open"}, "mode": "password", "store_id": "current",
            "mode_change_notice": {
                "store_id": "current", "notice_id": "committed-change",
                "message": "Encryption is on for this computer."
            }
        });
        let first: super::DesktopStoreStatus = serde_json::from_value(input.clone()).unwrap();
        let mut restarted: super::DesktopStoreStatus = serde_json::from_value(input).unwrap();
        assert_eq!(first.current_mode_notice(), restarted.current_mode_notice());
        assert!(restarted.current_mode_notice().is_some());
        restarted.store_id = Some("replacement".into());
        assert!(restarted.current_mode_notice().is_none());
        restarted.store_id = Some("current".into());
        restarted.state = maple_agent::store::StoreState::Locked;
        assert!(restarted.current_mode_notice().is_none());
    }

    #[test]
    fn guarded_startup_error_requires_verified_stop_before_close() {
        let mut startup = super::StartupError::new("Native startup hook failed".into());
        startup.requires_completed_stop = true;
        startup.stopping = true;
        assert!(!startup.close_is_allowed());
        assert!(!startup.finish_required_stop(Err("Fixture stop timeout".into())));
        assert!(!startup.stopping);
        assert!(!startup.close_is_allowed());
        assert!(startup.requires_completed_stop);
        assert!(startup.message.contains("Keep this window open"));
        startup.stopping = true;
        assert!(!startup.close_is_allowed());
        assert!(startup.finish_required_stop(Ok(())));
        assert!(startup.close_is_allowed());
        assert!(!startup.requires_completed_stop);
        assert!(!startup.stopping);
    }

    #[test]
    fn incompatible_existing_host_error_keeps_ordinary_close() {
        let startup = super::StartupError::new("Incompatible Maple host or profile".into());
        assert!(startup.close_is_allowed());
        assert!(startup.retry_backend.is_none());
    }

    use super::*;

    fn project_roots(name: &str) -> Vec<maple_agent::agent::RecentProjectRoot> {
        vec![maple_agent::agent::RecentProjectRoot {
            path: "/same/project".into(),
            name: name.into(),
            last_used_ms: 0,
            is_default_workspace: false,
            name_revision: 1,
        }]
    }

    pub(super) fn project_shell(cx: &mut gpui::TestAppContext) -> Entity<MapleApp> {
        let settings = crate::settings::SETTINGS_IO_LOCK.lock();
        project_shell_locked(cx, &settings)
    }

    fn project_shell_locked(
        cx: &mut gpui::TestAppContext,
        settings: &parking_lot::MutexGuard<'_, ()>,
    ) -> Entity<MapleApp> {
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        project_shell_with_backend(cx, backend, settings)
    }

    pub(super) fn project_shell_with_backend(
        cx: &mut gpui::TestAppContext,
        backend: Arc<AgentBackend>,
        _settings: &parking_lot::MutexGuard<'_, ()>,
    ) -> Entity<MapleApp> {
        cx.executor().allow_parking();
        let mut chat = |name: &str| {
            cx.new(|cx| {
                let mut chat = ChatScreen::new_without_start(backend.clone(), "user".into(), cx);
                let mut roots = project_roots(name);
                roots[0].name_revision = 0;
                chat.seed_projects_for_test(roots, cx);
                chat
            })
        };
        let local = chat("Local name");
        let east = chat("East name");
        let west = chat("West name");
        cx.new(|cx| {
            let settings = crate::settings::AppSettings::default();
            let shortcuts =
                crate::shortcuts::ShortcutRuntime::bootstrap(&settings.shortcut_overrides, cx);
            MapleApp {
                backend,
                screen: Screen::Chat(local.clone()),
                user_id: Some("user".into()),
                parked_chat: None,
                local_chat: Some(local),
                selected_host: None,
                remote_computers: [
                    (
                        "east".into(),
                        RemoteComputer {
                            name: "East".into(),
                            grant: [1; 16],
                            chat: Some(east),
                            attempt: ConnectionAttempt::Idle,
                        },
                    ),
                    (
                        "west".into(),
                        RemoteComputer {
                            name: "West".into(),
                            grant: [2; 16],
                            chat: Some(west),
                            attempt: ConnectionAttempt::Idle,
                        },
                    ),
                ]
                .into(),
                remote_inventory_loaded: true,
                next_connection_attempt: 0,
                requested_navigation: None,
                computers_sync_pending: false,
                remote_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                store_resident: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                store_load_epoch: 0,
                store_ready: true,
                store_id: None,
                store_notice: None,
                store_notice_is_error: false,
                seen_store_notice_operation: None,
                inline_password_completion: None,
                mode_change_notice: None,
                inline_mode_notice: None,
                password_settings_refresh_pending: false,
                password_settings_refreshing: false,
                persistence_status: Default::default(),
                persistence_retrying: false,
                #[cfg(feature = "proxy")]
                local_api_revision: 0,
                #[cfg(feature = "proxy")]
                local_api_host_epoch: 0,
                #[cfg(feature = "proxy")]
                local_api_refreshing: false,
                #[cfg(feature = "proxy")]
                local_api_refresh_again: false,
                pending_store_attention: Default::default(),
                store_attention_open: false,
                restore_window_geometry: false,
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                tray: None,
                remote_bridges: Default::default(),
                settings,
                updater: crate::update::Controller::new(true),
                restart_prompt_pending: false,
                #[cfg(target_os = "linux")]
                restart_confirmation: None,
                shortcuts,
                titlebar: cx.new(|_| TitleBar::new(ui::titlebar::WINDOW_TITLE)),
            }
        })
    }

    fn password_settings_shell_locked(
        cx: &mut gpui::TestAppContext,
        busy: bool,
        settings: &parking_lot::MutexGuard<'_, ()>,
    ) -> Entity<MapleApp> {
        let app = project_shell_locked(cx, settings);
        install_password_settings_shell(app, cx, busy)
    }

    pub(super) fn password_settings_shell_with_backend(
        cx: &mut gpui::TestAppContext,
        busy: bool,
        backend: Arc<AgentBackend>,
        settings: &parking_lot::MutexGuard<'_, ()>,
    ) -> Entity<MapleApp> {
        let app = project_shell_with_backend(cx, backend, settings);
        install_password_settings_shell(app, cx, busy)
    }

    fn install_password_settings_shell(
        app: Entity<MapleApp>,
        cx: &mut gpui::TestAppContext,
        busy: bool,
    ) -> Entity<MapleApp> {
        let backend = app.read_with(cx, |app, _cx| app.backend.clone());
        let form = cx.new(|cx| {
            ui::store_gate::PasswordForm::new(
                backend.clone(),
                ui::store_gate::PasswordAction::Set,
                Vec::new(),
                cx,
            )
        });
        form.update(cx, |form, _| form.set_busy_for_test(busy));
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
                backend.clone(),
                "user".into(),
                crate::settings::AppSettings::default(),
                crate::shortcuts::ShortcutSnapshot {
                    generation: 1,
                    rows: Vec::new(),
                    last_error: None,
                    compatibility_warning: None,
                },
                Section::Advanced,
                cx,
            )
        });
        settings.update(cx, |settings, _| {
            settings.set_password_form_for_test(Some(form))
        });
        app.update(cx, |app, _| {
            app.store_id = Some("original".into());
            app.screen = Screen::Settings(settings);
            app.parked_chat = app.local_chat.clone();
        });
        app
    }

    fn active_settings(
        app: &Entity<MapleApp>,
        cx: &mut gpui::TestAppContext,
    ) -> Entity<SettingsScreen> {
        app.read_with(cx, |app, _| match &app.screen {
            Screen::Settings(screen) => screen.clone(),
            _ => panic!("expected the active settings screen"),
        })
    }

    fn assert_chat_notification_policy(app: &MapleApp, details: bool, cx: &Context<MapleApp>) {
        let chats = app.local_chat.iter().chain(
            app.remote_computers
                .values()
                .filter_map(|computer| computer.chat.as_ref()),
        );
        let mut checked = 0;
        for chat in chats {
            assert_eq!(
                chat.read(cx).notification_preferences_for_test(),
                (true, details)
            );
            checked += 1;
        }
        assert_eq!(
            checked, 3,
            "local and both existing remote views must update"
        );
    }

    #[gpui::test]
    fn password_mode_refresh_updates_notifications_before_back_and_rejects_stale_mode(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = crate::settings::SETTINGS_IO_LOCK.lock();
        let app = project_shell_locked(cx, &settings);
        for (initial_password_mode, password_mode, opt_in) in [
            (false, true, false),
            (true, false, false),
            (false, true, true),
        ] {
            app.update(cx, |app, cx| {
                app.apply_refreshed_password_settings(
                    crate::settings::AppSettings {
                        desktop_notifications: true,
                        store_password_mode: initial_password_mode,
                        password_notification_details: opt_in,
                        ..Default::default()
                    },
                    cx,
                );
                app.show_settings(Section::Advanced, cx);
            });
            let screen = active_settings(&app, cx);
            let stale_close = screen.read_with(cx, |screen, _| screen.settings_snapshot());
            app.update(cx, |app, cx| {
                let mut refreshed = app.settings.clone();
                refreshed.store_password_mode = password_mode;
                app.apply_refreshed_password_settings(refreshed, cx);
                let snapshot = screen.read(cx).settings_snapshot();
                assert_eq!(snapshot.store_password_mode, password_mode);
                assert_eq!(snapshot.password_notification_details, opt_in);
                assert_chat_notification_policy(app, !password_mode || opt_in, cx);
            });
            // Exercise the real SettingsClosed subscription with a snapshot
            // captured before the authoritative mode changed.
            screen.update(cx, |_, cx| cx.emit(SettingsClosed(stale_close)));
            cx.run_until_parked();
            app.update(cx, |app, cx| {
                assert!(matches!(app.screen, Screen::Chat(_)));
                assert_eq!(app.settings.store_password_mode, password_mode);
                assert_eq!(app.settings.password_notification_details, opt_in);
                assert_chat_notification_policy(app, !password_mode || opt_in, cx);
            });
        }
    }

    #[gpui::test]
    fn connecting_from_settings_keeps_the_refreshed_notification_policy(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = crate::settings::SETTINGS_IO_LOCK.lock();
        let app = project_shell_locked(cx, &settings);
        app.update(cx, |app, cx| {
            app.settings.store_password_mode = false;
            app.settings.desktop_notifications = true;
            app.show_settings(Section::Remote, cx);
            let mut refreshed = app.settings.clone();
            refreshed.store_password_mode = true;
            app.apply_refreshed_password_settings(refreshed, cx);
        });
        let screen = active_settings(&app, cx);
        screen.update(cx, |_, cx| {
            cx.emit(ui::remote::ConnectComputer("east".into()));
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            assert!(matches!(app.screen, Screen::Chat(_)));
            assert_eq!(app.selected_host.as_deref(), Some("east"));
            assert!(app.settings.store_password_mode);
            assert_chat_notification_policy(app, false, cx);
        });
    }

    #[gpui::test]
    fn retired_settings_events_cannot_replace_the_active_screen_or_notification_mode(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = crate::settings::SETTINGS_IO_LOCK.lock();
        let app = project_shell_locked(cx, &settings);
        app.update(cx, |app, cx| {
            app.settings.store_password_mode = false;
            app.settings.desktop_notifications = true;
            app.show_settings(Section::General, cx);
        });
        let retired = active_settings(&app, cx);
        let stale = retired.read_with(cx, |screen, _| screen.settings_snapshot());
        retired.update(cx, |_, cx| cx.emit(SettingsClosed(stale.clone())));
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.show_settings(Section::Advanced, cx);
            let mut refreshed = app.settings.clone();
            refreshed.store_password_mode = true;
            app.apply_refreshed_password_settings(refreshed, cx);
        });
        let current = active_settings(&app, cx);
        retired.update(cx, |_, cx| {
            cx.emit(SettingsClosed(stale));
            cx.emit(ui::remote::ConnectComputer("east".into()));
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            assert!(app.is_current_settings_screen(&current));
            assert!(app.settings.store_password_mode);
            assert!(app.selected_host.is_none());
            assert_chat_notification_policy(app, false, cx);
        });
    }

    #[gpui::test]
    fn operation_notices_never_insert_rows_above_the_active_page(cx: &mut gpui::TestAppContext) {
        struct AppHost(Entity<MapleApp>);
        impl Render for AppHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(self.0.clone())
            }
        }
        let app = project_shell(cx);
        let (_host, cx) = cx.add_window_view(|window, _cx| {
            window.activate_window();
            AppHost(app.clone())
        });
        cx.simulate_resize(size(px(1100.), px(850.)));
        cx.run_until_parked();
        let original = cx.debug_bounds("maple-page").expect("active page");
        app.update(cx, |app, cx| {
            app.store_notice = Some("Maple local data is ready.".into());
            app.mode_change_notice = Some(ModeChangeNotice {
                store_id: "fixture".into(),
                notice_id: "change".into(),
                message: "Encryption is on.".into(),
            });
            app.persistence_status.failure = Some(maple_agent::store::PersistenceFailure {
                store_id: Some("fixture".into()),
                revision: 1,
                reason: maple_agent::store::PersistenceFailureReason::DiskFull,
            });
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(cx.debug_bounds("maple-page"), Some(original));
    }

    #[gpui::test]
    fn unhandled_attention_waits_for_a_live_window_and_uses_a_dialog(
        cx: &mut gpui::TestAppContext,
    ) {
        struct AppHost(Entity<MapleApp>);
        impl Render for AppHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(self.0.clone())
            }
        }
        let app = project_shell(cx);
        app.update(cx, |app, cx| {
            let status = serde_json::from_value(serde_json::json!({
                "state":{"state":"open"}, "mode":"password", "store_id":"fixture",
                "completion_notice":{"operation_id":"pending-sync", "message":"Keep both passwords until synchronization finishes.",
                    "is_error":false,"requires_attention":true}
            })).unwrap();
            app.apply_store_completion_notice(&status, cx);
            app.present_store_attention(
                "Recent changes could not be saved.".into(),
                StoreAttentionKind::Persistence, true, cx,
            );
            app.present_store_attention(
                "The disk is full. Keep Maple open and retry saving.".into(),
                StoreAttentionKind::Persistence, true, cx,
            );
            assert_eq!(app.pending_store_attention.len(), 2);
            assert!(!app.store_attention_open);
        });
        assert!(!cx.has_pending_prompt());
        let (_host, cx) = cx.add_window_view(|_, _| AppHost(app.clone()));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        let (title, detail) = cx.pending_prompt().expect("local-data dialog");
        assert_eq!(title, "Local data needs attention");
        assert!(detail.contains("both passwords"));
        cx.simulate_prompt_answer("OK");
        cx.run_until_parked();
        let (title, detail) = cx
            .pending_prompt()
            .expect("independent save-failure dialog");
        assert_eq!(title, "Maple couldn't save local data");
        assert!(detail.contains("disk is full"));
        cx.simulate_prompt_answer("Keep viewing");
        cx.run_until_parked();
        app.update(cx, |app, _| {
            assert!(app.pending_store_attention.is_empty());
            assert!(!app.store_attention_open);
        });
    }

    #[gpui::test]
    fn live_save_failures_reach_the_local_composer_after_returning_to_chat(
        cx: &mut gpui::TestAppContext,
    ) {
        struct AppHost(Entity<MapleApp>);
        impl Render for AppHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(self.0.clone())
            }
        }
        let app = project_shell(cx);
        app.update(cx, |app, _| app.store_id = Some("fixture".into()));
        let (_host, cx) = cx.add_window_view(|_, _| AppHost(app.clone()));
        cx.simulate_resize(size(px(1100.), px(850.)));
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.handle_service_events(
                vec![
                    maple_agent::agent::AgentServiceEvent::PersistenceFailureChanged {
                        status: maple_agent::store::PersistenceStatus {
                            store_id: Some("fixture".into()),
                            revision: 1,
                            failure: Some(maple_agent::store::PersistenceFailure {
                                store_id: Some("fixture".into()),
                                revision: 1,
                                reason: maple_agent::store::PersistenceFailureReason::DiskFull,
                            }),
                        },
                    },
                ],
                cx,
            );
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Keep viewing");
        cx.run_until_parked();
        assert!(cx.debug_bounds("chat-persistence-feedback").is_some());
        assert!(cx.debug_bounds("retry-local-store-persistence").is_some());
        app.update(cx, |app, cx| {
            app.handle_service_events(
                vec![
                    maple_agent::agent::AgentServiceEvent::PersistenceFailureChanged {
                        status: maple_agent::store::PersistenceStatus {
                            store_id: Some("fixture".into()),
                            revision: 2,
                            failure: None,
                        },
                    },
                ],
                cx,
            );
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("chat-persistence-feedback").is_none());
    }

    #[gpui::test]
    fn verified_maintenance_and_open_keep_the_busy_settings_form(cx: &mut gpui::TestAppContext) {
        let settings = crate::settings::SETTINGS_IO_LOCK.lock();
        let app = password_settings_shell_locked(cx, true, &settings);
        app.update(cx, |app, cx| {
            let epoch = app.store_load_epoch;
            app.show_store_state(maple_agent::store::StoreState::Maintenance, cx);
            assert!(matches!(app.screen, Screen::Settings(_)));
            assert_eq!(app.store_load_epoch, epoch);
            app.show_store_state(maple_agent::store::StoreState::Open, cx);
            assert!(matches!(app.screen, Screen::Settings(_)));
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn maintenance_after_form_completion_waits_for_authoritative_status(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = crate::settings::SETTINGS_IO_LOCK.lock();
        let app = password_settings_shell_locked(cx, false, &settings);
        app.update(cx, |app, cx| {
            app.handle_service_events(
                vec![maple_agent::agent::AgentServiceEvent::StoreStateChanged {
                    state: maple_agent::store::StoreState::Maintenance,
                }],
                cx,
            );
            assert!(matches!(app.screen, Screen::Settings(_)));
            assert!(app.password_settings_refresh_pending);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn replacement_generation_retires_busy_settings_and_parked_account_state(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = crate::settings::SETTINGS_IO_LOCK.lock();
        let app = password_settings_shell_locked(cx, true, &settings);
        app.update(cx, |app, cx| {
            let epoch = app.store_load_epoch;
            let status = serde_json::from_value(serde_json::json!({"state":{"state":"locked"},"mode":"password","store_id":"replacement"})).unwrap();
            app.apply_store_completion_notice(&status, cx);
            assert!(matches!(app.screen, Screen::StoreGate(_)));
            assert!(app.store_load_epoch > epoch);
            assert!(app.user_id.is_none());
            assert!(app.parked_chat.is_none());
            let replacement_epoch = app.store_load_epoch;
            app.apply_store_completion_notice(&status, cx);
            assert_eq!(app.store_load_epoch, replacement_epoch);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn stale_inline_acknowledgement_cannot_replace_a_newer_security_notice(
        cx: &mut gpui::TestAppContext,
    ) {
        let app = project_shell(cx);
        app.update(cx, |app, cx| {
            let old = ModeChangeNotice {
                store_id: "original".into(),
                notice_id: "old".into(),
                message: "Old change".into(),
            };
            let newer = ModeChangeNotice {
                notice_id: "newer".into(),
                message: "Newer change".into(),
                ..old.clone()
            };
            app.store_id = Some("original".into());
            app.inline_mode_notice = Some(old.clone());
            app.mode_change_notice = Some(newer.clone());
            app.finish_inline_mode_notice_ack(old, false, cx);
            assert_eq!(app.mode_change_notice, Some(newer));
            assert!(app.inline_mode_notice.is_none());
        });
    }

    fn rename_attempt(app: &MapleApp, computer: Option<&str>) -> PendingProjectRename {
        let target = ProjectTarget {
            computer: computer.map(str::to_string),
            path: "/same/project".into(),
        };
        let (owner, grant) = app.project_owner(&target).unwrap();
        let Screen::Chat(source) = &app.screen else {
            panic!("chat fixture")
        };
        PendingProjectRename {
            target,
            account: "user".into(),
            owner: owner.entity_id(),
            grant,
            source: source.downgrade(),
        }
    }

    fn label(app: &MapleApp, computer: Option<&str>, cx: &App) -> String {
        let (chat, _) = app
            .project_owner(&ProjectTarget {
                computer: computer.map(str::to_string),
                path: "/same/project".into(),
            })
            .unwrap();
        chat.read(cx)
            .computer_snapshot("Fixture".into(), cx)
            .projects
            .into_iter()
            .find(|project| project.path == "/same/project")
            .unwrap()
            .label
            .to_string()
    }

    #[gpui::test]
    fn project_rename_results_route_by_computer_without_navigation(cx: &mut gpui::TestAppContext) {
        let app = project_shell(cx);
        app.update(cx, |app, cx| {
            let Screen::Chat(active) = &app.screen else { panic!("chat fixture") };
            let active_id = active.entity_id();
            let navigation = active.read(cx).navigation_generation();
            let pending = rename_attempt(app, Some("east"));
            app.complete_project_rename(pending, Ok(project_roots("Renamed east").into_iter().next().unwrap()), cx);
            assert_eq!(label(app, Some("east"), cx), "Renamed east");
            assert_eq!(label(app, Some("west"), cx), "West name");
            assert_eq!(label(app, None, cx), "Local name");
            assert!(matches!(&app.screen, Screen::Chat(chat) if chat.entity_id() == active_id && chat.read(cx).navigation_generation() == navigation));
            assert!(app.selected_host.is_none());
            assert!(app.requested_navigation.is_none());

            let east = app.remote_computers["east"].chat.as_ref().unwrap().clone();
            app.screen = Screen::Chat(east.clone());
            app.selected_host = Some("east".into());
            let pending = rename_attempt(app, None);
            app.complete_project_rename(pending, Ok(project_roots("Renamed local").into_iter().next().unwrap()), cx);
            assert_eq!(label(app, None, cx), "Renamed local");
            assert_eq!(label(app, Some("east"), cx), "Renamed east");
            assert!(matches!(&app.screen, Screen::Chat(chat) if chat.entity_id() == east.entity_id()));
            assert_eq!(app.selected_host.as_deref(), Some("east"));
            assert!(app.requested_navigation.is_none());

            let pending = rename_attempt(app, Some("west"));
            app.complete_project_rename(pending, Err("Connection closed".into()), cx);
            assert_eq!(label(app, Some("west"), cx), "West name");
            assert!(matches!(&app.screen, Screen::Chat(chat) if chat.entity_id() == east.entity_id()));
        });
    }

    #[gpui::test]
    fn stale_project_rename_never_repopulates_an_account_or_pairing(cx: &mut gpui::TestAppContext) {
        let app = project_shell(cx);
        app.update(cx, |app, cx| {
            let pending = rename_attempt(app, Some("east"));
            app.remote_computers.get_mut("east").unwrap().grant = [3; 16];
            app.complete_project_rename(
                pending,
                Ok(project_roots("Old grant").into_iter().next().unwrap()),
                cx,
            );
            assert_eq!(label(app, Some("east"), cx), "East name");

            let pending = rename_attempt(app, Some("east"));
            let west = app.remote_computers["west"].chat.clone();
            app.remote_computers.get_mut("east").unwrap().chat = west;
            app.complete_project_rename(
                pending,
                Ok(project_roots("Old owner").into_iter().next().unwrap()),
                cx,
            );
            assert_eq!(label(app, Some("east"), cx), "West name");

            let pending = rename_attempt(app, Some("east"));
            app.remote_computers.remove("east");
            app.complete_project_rename(
                pending,
                Ok(project_roots("Unpaired").into_iter().next().unwrap()),
                cx,
            );
            assert!(!app.remote_computers.contains_key("east"));

            let pending = rename_attempt(app, None);
            app.user_id = Some("another-account".into());
            app.complete_project_rename(
                pending,
                Ok(project_roots("Old account").into_iter().next().unwrap()),
                cx,
            );
            assert_eq!(label(app, None, cx), "Local name");
        });
    }

    #[gpui::test]
    fn unavailable_project_rename_does_not_connect_or_queue_navigation(
        cx: &mut gpui::TestAppContext,
    ) {
        let app = project_shell(cx);
        app.update(cx, |app, cx| {
            let source = app.local_chat.as_ref().unwrap().clone();
            // The cached fixture has not finished its initial navigation
            // snapshot, so no mutation may be sent through it yet.
            app.rename_project(
                source.clone(),
                ProjectRenameRequest {
                    target: ProjectTarget {
                        computer: Some("east".into()),
                        path: "/same/project".into(),
                    },
                    name: "Not sent".into(),
                },
                cx,
            );
            assert!(app.remote_bridges.borrow().is_empty());
            assert_eq!(app.next_connection_attempt, 0);
            assert!(app.requested_navigation.is_none());
            assert!(app.selected_host.is_none());
            assert!(
                matches!(&app.screen, Screen::Chat(chat) if chat.entity_id() == source.entity_id())
            );
            assert_eq!(label(app, Some("east"), cx), "East name");
        });
    }

    fn bounds(width: f32, height: f32) -> Bounds<gpui::Pixels> {
        Bounds::new(gpui::point(px(0.), px(0.)), size(px(width), px(height)))
    }

    #[gpui::test]
    fn failed_tray_quit_opens_a_visible_confirmation_without_an_existing_window(
        cx: &mut gpui::TestAppContext,
    ) {
        assert!(cx.windows().is_empty());
        cx.update(|cx| show_quit_warning("A host drain is still pending.".into(), cx));
        assert_eq!(cx.windows().len(), 1);
        let mut view = gpui::VisualTestContext::from_window(cx.windows()[0], cx);
        view.run_until_parked();
        assert!(view.debug_bounds("retry-maple-quit").is_some());
        assert!(view.debug_bounds("quit-maple-anyway").is_none());
        let keep = view.debug_bounds("keep-maple-open").unwrap();
        view.simulate_click(keep.center(), gpui::Modifiers::default());
        view.run_until_parked();
        assert!(
            cx.windows().is_empty(),
            "Keep open dismisses the confirmation, not the app"
        );
    }

    #[test]
    fn fullscreen_does_not_replace_the_windowed_state() {
        let windowed = window_state_for(WindowBounds::Windowed(bounds(1280., 860.))).unwrap();
        assert_eq!((windowed.width, windowed.height), (1280., 860.));
        assert!(!windowed.maximized);

        let maximized = window_state_for(WindowBounds::Maximized(bounds(1280., 860.))).unwrap();
        assert!(maximized.maximized);

        assert!(window_state_for(WindowBounds::Fullscreen(bounds(3840., 2160.))).is_none());
    }

    #[test]
    fn window_identity_and_title_match_the_packaging_metadata() {
        let options = main_window_options((WindowBounds::Windowed(bounds(1280., 860.)), None));
        let info = serde_json::to_value(crate::profile::build_info()).unwrap();
        assert_eq!(options.app_id.as_deref(), info["bundle_id"].as_str());
        assert_eq!(
            options.titlebar.unwrap().title.unwrap().as_ref(),
            info["display_name"].as_str().unwrap()
        );
    }
}
