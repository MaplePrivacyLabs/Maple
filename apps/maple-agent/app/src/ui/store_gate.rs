//! Local-only store entry and password forms. These screens ask the host for
//! lifecycle status before loading any account-owned application data.

use super::i18n::{UiText, tr};
use gpui::{
    Context, Entity, EventEmitter, Focusable, Render, Subscription, Window, div, prelude::*, px,
};
use maple_agent::store::records::SecretString;
use maple_agent::store::{
    DamageReason, RefusalReason, StoreState,
    keys::{self, Password, PasswordAssessment, StrengthLevel},
    lifecycle::{StoreMode, StoreModeNotice},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{cell::RefCell, sync::Arc, time::Duration};
use zeroize::Zeroizing;

use super::store_repair::{RepairPanel, RepairPanelKind, RepairProgress, RepairStateChanged};
use super::{
    secret_input::{SecretInput, SecretInputChanged, SecretInputSubmitted},
    text_input::TextInput,
    theme, widgets,
};
use crate::backend::AgentBackend;

#[derive(Clone, Deserialize)]
pub(crate) struct StoreStatus {
    pub state: StoreState,
    pub mode: StoreMode,
    #[serde(default)]
    pub store_id: Option<String>,
    #[serde(default)]
    pub mode_change_notice: Option<StoreModeNotice>,
    #[serde(default)]
    pub completion_notice: Option<StoreOperationNotice>,
    #[serde(default)]
    pub persistence_status: maple_agent::store::PersistenceStatus,
    #[serde(default)]
    pub password_settings_available: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StoreOperationNotice {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub pending: bool,
    #[serde(default)]
    #[serde(deserialize_with = "crate::backend::HostFailure::deserialize_optional")]
    pub failure: Option<crate::backend::HostFailure>,
    pub operation_id: String,
    pub message: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub requires_attention: bool,
}

impl StoreOperationNotice {
    pub(crate) fn presentation(&self) -> super::i18n::UiText {
        if let Some(failure) = &self.failure {
            return super::host_errors::failure_text(failure);
        }
        let key = match (self.label.as_deref(), self.pending) {
            (Some("unlock-previous"), false) => "runtime-notice-unlock-previous",
            (Some("unlock-previous"), true) => "runtime-notice-unlock-previous-pending",
            (Some("unlock"), false) => "runtime-notice-unlock",
            (Some("unlock"), true) => "runtime-notice-unlock-pending",
            (Some("set_store_password"), false) => "runtime-notice-set-store-password",
            (Some("set_store_password"), true) => "runtime-notice-set-store-password-pending",
            (Some("change_store_password"), false) => "runtime-notice-change-store-password",
            (Some("change_store_password"), true) => "runtime-notice-change-store-password-pending",
            (Some("remove_store_password"), false) => "runtime-notice-remove-store-password",
            (Some("remove_store_password"), true) => "runtime-notice-remove-store-password-pending",
            (Some("erase_local_data"), false) => "runtime-notice-erase-local-data",
            (Some("erase_local_data"), true) => "runtime-notice-erase-local-data-pending",
            (Some("start_fresh"), false) => "runtime-notice-start-fresh",
            (Some("start_fresh"), true) => "runtime-notice-start-fresh-pending",
            (Some("setup_store"), false) => "runtime-notice-setup-store",
            (Some("setup_store"), true) => "runtime-notice-setup-store-pending",
            (Some("salvage_store"), false) => "runtime-notice-salvage-store",
            (Some("salvage_store"), true) => "runtime-notice-salvage-store-pending",
            _ => return super::i18n::UiText::Raw(self.message.clone()),
        };
        super::i18n::UiText::key(key)
    }
}

impl StoreStatus {
    pub(crate) fn current_mode_notice(&self) -> Option<&StoreModeNotice> {
        self.mode_change_notice.as_ref().filter(|notice| {
            self.state == StoreState::Open
                && self.store_id.as_deref() == Some(notice.store_id.as_str())
        })
    }

    pub(crate) fn attention_notice(&self) -> Option<&StoreOperationNotice> {
        self.completion_notice.as_ref().filter(|notice| {
            self.state == StoreState::Open
                && self.store_id.is_some()
                && !notice.operation_id.is_empty()
                && (notice.is_error || notice.requires_attention)
        })
    }

    pub(crate) fn current_persistence_failure(
        &self,
    ) -> Option<&maple_agent::store::PersistenceFailure> {
        self.persistence_status.failure.as_ref().filter(|failure| {
            self.state == StoreState::Open
                && self.store_id.is_some()
                && self.persistence_status.store_id == self.store_id
                && failure.store_id == self.store_id
        })
    }
}

/// Ask the desktop coordinator to retry all pending local persistence, including
/// Settings edits and loaded sessions, before refreshing the local status.
pub struct RetryStorePersistence;

#[derive(Clone, Default, Deserialize)]
pub(crate) struct OperationProgress {
    #[serde(default)]
    pub busy: bool,
    pub operation: Option<String>,
    pub ui_operation_id: Option<String>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub completed_bytes: u64,
    #[serde(default)]
    pub total_bytes: u64,
    pub remaining_ms: Option<u64>,
    #[serde(default)]
    pub cancellable: bool,
}

pub struct StoreGateChanged;

pub struct StoreGate {
    messages: super::i18n::MessageCache,
    focus: gpui::FocusHandle,
    backend: Arc<AgentBackend>,
    state: StoreState,
    status: Option<StoreStatus>,
    password: Entity<SecretInput>,
    confirmation: Entity<TextInput>,
    form: Option<Entity<PasswordForm>>,
    repair: Option<Entity<RepairPanel>>,
    subscriptions: Vec<Subscription>,
    bridges: RefCell<Vec<gpui::Task<()>>>,
    busy: bool,
    erase: bool,
    message: Option<UiText>,
    progress: OperationProgress,
    repair_progress: RepairProgress,
    progress_task: Option<gpui::Task<()>>,
    focus_pending: bool,
}

impl EventEmitter<StoreGateChanged> for StoreGate {}

impl StoreGate {
    pub(crate) fn show_local_store_notice(
        &mut self,
        message: impl Into<UiText>,
        cx: &mut Context<Self>,
    ) {
        if let Some(form) = &self.form {
            form.update(cx, |form, cx| form.show_local_store_notice(message, cx));
        } else {
            self.message = Some(message.into());
            cx.notify();
        }
    }

    pub fn new(backend: Arc<AgentBackend>, state: StoreState, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new_mounted(backend, state, cx);
        this.refresh(cx);
        if this.state == StoreState::Maintenance {
            this.poll_maintenance(cx);
        }
        this
    }

    /// Mount the gate without starting host requests. Production starts them
    /// immediately in `new`; UI fixtures keep their supplied state independent
    /// of unrelated Tokio completions, as with `ChatScreen::new_mounted`.
    fn new_mounted(backend: Arc<AgentBackend>, state: StoreState, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            messages: Default::default(),
            focus: cx.focus_handle(),
            backend,
            state,
            status: None,
            password: cx.new(|cx| {
                SecretInput::localized("flows-store-unlock-password", cx).with_tab_index(1)
            }),
            confirmation: cx.new(|cx| {
                TextInput::localized("flows-store-type-start-fresh", cx).with_tab_index(1)
            }),
            form: None,
            repair: None,
            subscriptions: Vec::new(),
            bridges: RefCell::new(Vec::new()),
            busy: false,
            erase: false,
            message: None,
            progress: OperationProgress::default(),
            repair_progress: RepairProgress::default(),
            progress_task: None,
            focus_pending: true,
        };
        let changed = cx.subscribe(
            &this.password,
            |_this, _input, _event: &SecretInputChanged, cx| cx.notify(),
        );
        this.subscriptions.push(changed);
        this.subscriptions.push(cx.subscribe(
            &this.password,
            |this, _input, _event: &SecretInputSubmitted, cx| this.unlock(cx),
        ));
        let weak = cx.entity().downgrade();
        this.confirmation.update(cx, |input, _cx| {
            input.set_on_enter(move |confirmation, _window, cx| {
                if let Some(this) = weak.upgrade() {
                    this.update(cx, |this, cx| {
                        if this.erase {
                            this.reset_with_confirmation(false, confirmation, cx);
                        } else if matches!(this.state, StoreState::Damaged { .. }) {
                            this.reset_with_confirmation(true, confirmation, cx);
                        }
                    });
                }
            });
        });
        this
    }

    fn poll_maintenance(&mut self, cx: &mut Context<Self>) {
        self.progress_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let Ok(backend) = this.update(cx, |this, _cx| this.backend.clone()) else {
                    break;
                };
                let control = backend.clone();
                let response = backend
                    .spawn(async move {
                        tokio::join!(
                            control
                                .local_control_presented::<OperationProgress>("store_progress", ()),
                            control
                                .local_control_presented::<RepairProgress>("repair_progress", ())
                        )
                    })
                    .await;
                if let Ok((progress, repair)) = response
                    && this
                        .update(cx, |this, cx| {
                            if let Ok(progress) = progress {
                                this.progress = progress;
                            }
                            if let Ok(repair) = repair {
                                this.repair_progress = repair;
                            }
                            cx.notify();
                        })
                        .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn cancel_maintenance(&mut self, cx: &mut Context<Self>) {
        let repair = self.repair_progress.busy;
        if !(if repair {
            self.repair_progress.cancellable
        } else {
            self.progress.cancellable
        }) {
            return;
        }
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control_presented::<bool>(
                        if repair {
                            "cancel_store_repair"
                        } else {
                            "cancel_store_operation"
                        },
                        (),
                    )
                    .await
            },
            cx,
            |this, result, cx| {
                this.message = Some(match result {
                    Ok(true) => UiText::key("flows-store-cancelling"),
                    Ok(false) => UiText::key("flows-store-committing"),
                    Err(message) => message.into(),
                });
                cx.notify();
            },
        );
    }

    fn call<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = Result<T, crate::backend::HostFailure>>
        + Send
        + 'static,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<T, super::task::Failure>, &mut Context<Self>) + 'static,
    ) {
        let bridge =
            super::task::call_semantic(&self.backend, future, "flows-task-cancelled", cx, then);
        super::task::retain(&self.bridges, bridge);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control_presented::<StoreStatus>("store_status", ())
                    .await
            },
            cx,
            |this, result, cx| {
                match result {
                    Ok(status) => {
                        if this.state != status.state {
                            this.focus_pending = true;
                        }
                        this.state = status.state.clone();
                        if matches!(status.state, StoreState::Refused { .. }) {
                            this.erase = false;
                            this.form = None;
                            this.repair = None;
                            this.password.update(cx, |input, cx| input.clear(cx));
                            this.confirmation.update(cx, |input, cx| input.clear(cx));
                        }
                        if matches!(status.state, StoreState::Damaged { .. })
                            && this.repair.is_none()
                        {
                            let backend = this.backend.clone();
                            let password = status.mode == StoreMode::Password;
                            let available = status.password_settings_available;
                            let repair = cx.new(|cx| {
                                RepairPanel::new(
                                    backend,
                                    RepairPanelKind::Damaged { password },
                                    available,
                                    cx,
                                )
                            });
                            this.subscriptions.push(cx.subscribe(
                                &repair,
                                |this, _panel, _event: &RepairStateChanged, cx| {
                                    cx.emit(StoreGateChanged);
                                    this.refresh(cx);
                                },
                            ));
                            this.repair = Some(repair);
                        }
                        this.status = Some(status);
                    }
                    Err(failure) => this.message = Some(failure.into()),
                }
                cx.notify();
            },
        );
    }

    fn unlock(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.erase || !accepts_unlock(&self.state) {
            return;
        }
        if self.password.read(cx).is_empty() {
            self.message = Some(UiText::key("flows-store-enter-password"));
            cx.notify();
            return;
        }
        let password = self.password.update(cx, |input, cx| input.take_secret(cx));
        self.busy = true;
        self.message = None;
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control_presented::<Value>(
                        "unlock",
                        PasswordRequest {
                            password: &password,
                            generated_id: None,
                            ui_operation_id: None,
                        },
                    )
                    .await
            },
            cx,
            |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(_) => cx.emit(StoreGateChanged),
                    Err(message) => {
                        this.message = Some(message.into());
                        this.refresh(cx);
                    }
                }
                cx.notify();
            },
        );
    }

    fn open_erase(&mut self, cx: &mut Context<Self>) {
        if self.busy || matches!(self.state, StoreState::Refused { .. }) {
            return;
        }
        self.password.update(cx, |input, cx| input.clear(cx));
        self.form = None;
        self.confirmation.update(cx, |input, cx| {
            input.clear(cx);
            input.set_placeholder_key("flows-store-type-erase", cx);
        });
        self.erase = true;
        self.focus_pending = true;
        self.message = None;
        cx.notify();
    }

    fn close_erase(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.erase = false;
        self.confirmation.update(cx, |input, cx| {
            input.clear(cx);
            input.set_placeholder_key("flows-store-type-start-fresh", cx);
        });
        self.message = None;
        self.focus_pending = true;
        cx.notify();
    }

    fn setup_password(&mut self, cx: &mut Context<Self>) {
        if self.busy
            || matches!(self.state, StoreState::Refused { .. })
            || !self
                .status
                .as_ref()
                .is_some_and(|status| status.password_settings_available)
        {
            return;
        }
        let backend = self.backend.clone();
        let form = cx.new(|cx| PasswordForm::new(backend, PasswordAction::Setup, Vec::new(), cx));
        self.subscriptions.push(cx.subscribe(
            &form,
            |this, _form, event: &PasswordFormFinished, cx| {
                if event.cancelled {
                    this.form = None;
                    cx.notify();
                    return;
                }
                cx.emit(StoreGateChanged);
                this.form = None;
                this.refresh(cx);
            },
        ));
        self.form = Some(form);
        cx.notify();
    }

    fn plain_setup(&mut self, cx: &mut Context<Self>) {
        if self.busy || matches!(self.state, StoreState::Refused { .. }) {
            return;
        }
        self.busy = true;
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control_presented::<Value>(
                        "setup_store",
                        SetupRequest {
                            password: None,
                            generated_id: None,
                            ui_operation_id: None,
                        },
                    )
                    .await
            },
            cx,
            |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(_) => cx.emit(StoreGateChanged),
                    Err(message) => this.message = Some(message.into()),
                }
                this.refresh(cx);
                cx.notify();
            },
        );
    }

    fn reset(&mut self, start_fresh: bool, cx: &mut Context<Self>) {
        let confirmation = self.confirmation.read(cx).text();
        self.reset_with_confirmation(start_fresh, confirmation, cx);
    }

    fn reset_with_confirmation(
        &mut self,
        start_fresh: bool,
        confirmation: String,
        cx: &mut Context<Self>,
    ) {
        if self.busy || matches!(self.state, StoreState::Refused { .. }) {
            return;
        }
        let required = if start_fresh { "START FRESH" } else { "ERASE" };
        if confirmation != required {
            self.message = Some(UiText::message(
                "flows-store-confirm-required",
                [("confirmation", required.into())],
            ));
            cx.notify();
            return;
        }
        crate::settings::discard_decrypted_cache();
        super::spell::clear_user_words();
        self.busy = true;
        self.message = None;
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control_presented::<Value>(
                        if start_fresh {
                            "start_fresh"
                        } else {
                            "erase_local_data"
                        },
                        ConfirmationRequest {
                            confirmation: &confirmation,
                        },
                    )
                    .await
            },
            cx,
            |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(_) => {
                        this.erase = false;
                        cx.emit(StoreGateChanged);
                    }
                    Err(message) => this.message = Some(message.into()),
                }
                this.refresh(cx);
                cx.notify();
            },
        );
    }
}

impl Render for StoreGate {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_pending
            && (self.erase
                || (matches!(self.state, StoreState::Damaged { .. })
                    && !accepts_unlock(&self.state)))
        {
            window.focus(&self.confirmation.read(cx).focus_handle(cx), cx);
            self.focus_pending = false;
        }
        if self.focus_pending && accepts_unlock(&self.state) && !self.erase {
            self.password
                .update(cx, |input, cx| input.focus(window, cx));
            self.focus_pending = false;
        }
        if self.focus_pending
            && matches!(
                self.state,
                StoreState::NeedsSetup | StoreState::Maintenance | StoreState::Refused { .. }
            )
        {
            window.focus(&self.focus, cx);
            self.focus_pending = false;
        }
        let mut body = div()
            .w(px(520.))
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .rounded(theme::RADIUS_MD)
            .bg(gpui::rgb(theme::bg_elevated()))
            .text_color(gpui::rgb(theme::text_primary()));
        if let Some(form) = &self.form {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(form.clone());
        }
        let (title, description) = match &self.state {
            StoreState::Locked => (tr("flows-store-locked"), tr("flows-store-locked-body")),
            StoreState::NeedsSetup => (tr("flows-store-setup"), tr("flows-store-setup-body")),
            StoreState::Damaged { .. } => {
                (tr("flows-store-damaged"), tr("flows-store-damaged-body"))
            }
            StoreState::Refused { reason } => {
                (refusal_title(*reason), refusal_description(*reason))
            }
            StoreState::Maintenance => (
                tr("flows-store-maintenance"),
                tr("flows-store-maintenance-body"),
            ),
            StoreState::Open => (tr("flows-store-opening"), tr("flows-store-ready")),
        };
        body = body
            .child(div().text_xl().child(title))
            .child(div().text_sm().child(description));
        if self.erase {
            body = body
                .child(div().text_sm().child(tr(ERASE_COPY)))
                .child(
                    widgets::input_frame()
                        .id("store-gate-confirmation")
                        .debug_selector(|| "store-gate-confirmation".into())
                        .min_h(px(40.))
                        .child(self.confirmation.clone()),
                )
                .child(
                    widgets::primary_button("store-gate-erase")
                        .debug_selector(|| "store-gate-erase".into())
                        .tab_index(2)
                        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                            if password_action_key(event, window, cx) {
                                this.reset(false, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .when(self.busy, |button| button.opacity(0.5))
                        .on_click(cx.listener(|this, _event, _window, cx| this.reset(false, cx)))
                        .child(gpui::text!(tr("flows-store-erase"))),
                )
                .child(
                    widgets::ghost_button("store-gate-erase-cancel")
                        .debug_selector(|| "store-gate-erase-cancel".into())
                        .tab_index(3)
                        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                            if password_action_key(event, window, cx) {
                                this.close_erase(cx);
                                cx.stop_propagation();
                            }
                        }))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.close_erase(cx);
                        }))
                        .child(gpui::text!(tr("flows-store-cancel"))),
                );
        } else {
            body = match self.state {
                StoreState::Locked => body
                    .child(password_field(
                        "settings-password-unlock-password",
                        self.password.clone(),
                    ))
                    .child(
                        widgets::primary_button("store-unlock")
                            .debug_selector(|| "store-unlock".into())
                            .tab_index(2)
                            .on_key_down(cx.listener(
                                |this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) {
                                        this.unlock(cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .when(self.busy, |button| button.opacity(0.5))
                            .on_click(cx.listener(|this, _event, _window, cx| this.unlock(cx)))
                            .child(gpui::text!(if self.busy {
                                tr("flows-store-unlocking")
                            } else {
                                tr("flows-store-unlock")
                            })),
                    )
                    .child(
                        widgets::ghost_button("store-forgot")
                            .debug_selector(|| "store-forgot".into())
                            .tab_index(3)
                            .on_key_down(cx.listener(
                                |this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) {
                                        this.open_erase(cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .on_click(cx.listener(|this, _event, _window, cx| this.open_erase(cx)))
                            .child(gpui::text!(tr("flows-store-forgot"))),
                    ),
                StoreState::NeedsSetup => body
                    .when(
                        self.status
                            .as_ref()
                            .is_some_and(|status| status.password_settings_available),
                        |body| {
                            body.child(
                                widgets::primary_button("store-setup-password")
                                    .debug_selector(|| "store-setup-password".into())
                                    .tab_index(1)
                                    .on_key_down(cx.listener(
                                        |this, event: &gpui::KeyDownEvent, window, cx| {
                                            if password_action_key(event, window, cx) {
                                                this.setup_password(cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.setup_password(cx)
                                    }))
                                    .child(gpui::text!(tr("flows-store-set-password"))),
                            )
                        },
                    )
                    .child(
                        widgets::secondary_button("store-setup-plain")
                            .debug_selector(|| "store-setup-plain".into())
                            .tab_index(2)
                            .on_key_down(cx.listener(
                                |this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) {
                                        this.plain_setup(cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .on_click(cx.listener(|this, _event, _window, cx| this.plain_setup(cx)))
                            .child(gpui::text!(tr("flows-store-no-password"))),
                    ),
                StoreState::Damaged { .. } => body
                    .when(accepts_unlock(&self.state), |body| {
                        body.child(
                            div()
                                .text_sm()
                                .child(gpui::text!(tr("flows-store-verify-body"))),
                        )
                        .child(password_field(
                            "settings-password-unlock-password",
                            self.password.clone(),
                        ))
                        .child(
                            widgets::primary_button("store-generation-unlock")
                                .debug_selector(|| "store-generation-unlock".into())
                                .tab_index(2)
                                .on_key_down(cx.listener(
                                    |this, event: &gpui::KeyDownEvent, window, cx| {
                                        if password_action_key(event, window, cx) {
                                            this.unlock(cx);
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                                .when(self.busy, |button| button.opacity(0.5))
                                .on_click(cx.listener(|this, _event, _window, cx| this.unlock(cx)))
                                .child(gpui::text!(if self.busy {
                                    tr("flows-store-verifying")
                                } else {
                                    tr("flows-store-verify")
                                })),
                        )
                    })
                    .when_some(self.repair.clone(), |body, repair| body.child(repair))
                    .child(
                        widgets::input_frame()
                            .id("store-gate-confirmation")
                            .debug_selector(|| "store-gate-confirmation".into())
                            .min_h(px(40.))
                            .child(self.confirmation.clone()),
                    )
                    .child(div().text_sm().child(tr("flows-store-start-fresh-body")))
                    .child(
                        widgets::primary_button("store-start-fresh")
                            .debug_selector(|| "store-start-fresh".into())
                            .tab_index(2)
                            .on_key_down(cx.listener(
                                |this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) {
                                        this.reset(true, cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .on_click(cx.listener(|this, _event, _window, cx| this.reset(true, cx)))
                            .child(gpui::text!(tr("flows-store-start-fresh"))),
                    )
                    .child(
                        widgets::ghost_button("store-damaged-erase")
                            .debug_selector(|| "store-damaged-erase".into())
                            .tab_index(3)
                            .on_key_down(cx.listener(
                                |this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) {
                                        this.open_erase(cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .on_click(cx.listener(|this, _event, _window, cx| this.open_erase(cx)))
                            .child(gpui::text!(tr("flows-store-erase-menu"))),
                    ),
                StoreState::Maintenance => body
                    .child(div().text_sm().child(if self.repair_progress.busy {
                        self.messages
                            .get(
                                "flows-store-repair-progress",
                                [(
                                    "phase",
                                    super::store_repair::phase_label(&self.repair_progress.phase)
                                        .to_string()
                                        .into(),
                                )],
                            )
                            .to_string()
                    } else {
                        progress_copy(
                            progress_action(self.progress.operation.as_deref()),
                            &self.progress,
                            &self.messages,
                        )
                    }))
                    .when(
                        if self.repair_progress.busy {
                            self.repair_progress.cancellable
                        } else {
                            self.progress.cancellable
                        },
                        |body| {
                            body.child(
                                widgets::secondary_button("store-gate-operation-cancel")
                                    .debug_selector(|| "store-gate-operation-cancel".into())
                                    .tab_index(1)
                                    .on_key_down(cx.listener(
                                        |this, event: &gpui::KeyDownEvent, window, cx| {
                                            if password_action_key(event, window, cx) {
                                                this.cancel_maintenance(cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.cancel_maintenance(cx)
                                    }))
                                    .child(gpui::text!(tr("flows-store-cancel"))),
                            )
                        },
                    ),
                _ => body,
            };
        }
        if let Some(message) = &self.message {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::status_warning()))
                    .child(message.resolve()),
            );
        }
        div()
            .track_focus(&self.focus)
            .on_key_down(
                cx.listener(|_this, event: &gpui::KeyDownEvent, window, cx| {
                    password_action_key(event, window, cx);
                }),
            )
            .size_full()
            .p_4()
            .flex()
            .justify_center()
            .bg(gpui::rgb(theme::bg_elevated()))
            .child(
                div()
                    .id("store-gate-scroll")
                    .h_full()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

fn refusal_title(reason: RefusalReason) -> gpui::SharedString {
    match reason {
        RefusalReason::NewerVersion | RefusalReason::IncompatibleSchema => {
            tr("flows-store-compatible")
        }
        RefusalReason::UnsupportedFilesystem | RefusalReason::UnsafePath => {
            tr("flows-store-private-path")
        }
        RefusalReason::UnsupportedOperationFormat => tr("flows-store-operation-version"),
        RefusalReason::Unavailable => tr("flows-store-unavailable"),
    }
}

fn refusal_description(reason: RefusalReason) -> gpui::SharedString {
    tr(match reason {
        RefusalReason::NewerVersion => "flows-store-refusal-newer",
        RefusalReason::IncompatibleSchema => "flows-store-refusal-schema",
        RefusalReason::UnsupportedFilesystem => "flows-store-refusal-filesystem",
        RefusalReason::UnsafePath => "flows-store-refusal-unsafe-path",
        RefusalReason::UnsupportedOperationFormat => "flows-store-refusal-operation",
        RefusalReason::Unavailable => "flows-store-refusal-unavailable",
    })
}

fn accepts_unlock(state: &StoreState) -> bool {
    matches!(
        state,
        StoreState::Locked
            | StoreState::Damaged {
                reason: DamageReason::Generation
            }
    )
}

pub(crate) const ERASE_COPY: &str =
    "settings-password-maple-can-t-reset-your-unlock-password-erasing-removes-the-data-s";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasswordAction {
    Set,
    Change,
    Remove,
    Setup,
}

#[derive(Clone)]
pub(crate) struct PasswordFormFinished {
    pub notice: Option<UiText>,
    pub cancelled: bool,
    pub mode_notice: Option<StoreModeNotice>,
    pub completion_operation_id: Option<String>,
}

#[derive(Deserialize)]
struct SuggestedPhrase {
    id: String,
    passphrase: SecretString,
    assessment: PasswordAssessment,
}

struct PasswordAttempt {
    result: Result<Value, super::task::Failure>,
    generated_retry_valid: bool,
}

#[derive(Clone, Copy)]
enum PasswordField {
    Current,
    New,
    Confirmation,
}

pub(crate) struct PasswordForm {
    messages: super::i18n::MessageCache,
    backend: Arc<AgentBackend>,
    action: PasswordAction,
    current: Entity<SecretInput>,
    password: Entity<SecretInput>,
    confirmation: Entity<SecretInput>,
    suggested: bool,
    phrase: Option<SuggestedPhrase>,
    context: Vec<String>,
    assessment: Option<PasswordAssessment>,
    confirmation_matches: bool,
    reencrypt: bool,
    busy: bool,
    generating: bool,
    suggestion_retry_blocked: bool,
    ui_operation_id: Option<String>,
    message: Option<UiText>,
    progress: OperationProgress,
    bridges: RefCell<Vec<gpui::Task<()>>>,
    subscriptions: Vec<Subscription>,
    progress_task: Option<gpui::Task<()>>,
    focus_pending: Option<PasswordField>,
}
impl EventEmitter<PasswordFormFinished> for PasswordForm {}

impl PasswordForm {
    pub(crate) fn show_local_store_notice(
        &mut self,
        message: impl Into<UiText>,
        cx: &mut Context<Self>,
    ) {
        self.message = Some(message.into());
        cx.notify();
    }

    pub(crate) fn new(
        backend: Arc<AgentBackend>,
        action: PasswordAction,
        mut context: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        context.push("Maple".into());
        context.push(
            hostname::get()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        );
        let current = cx.new(|cx| {
            SecretInput::localized("settings-password-current-unlock-password", cx)
                .with_tab_index(1)
        });
        let password = cx.new(|cx| {
            SecretInput::localized("settings-password-enter-a-new-unlock-password", cx)
                .with_tab_index(2)
        });
        let confirmation = cx.new(|cx| {
            SecretInput::localized("settings-password-re-enter-your-new-unlock-password", cx)
                .with_tab_index(3)
        });
        let mut this = Self {
            messages: Default::default(),
            backend,
            action,
            current,
            password,
            confirmation,
            suggested: false,
            phrase: None,
            context,
            assessment: None,
            confirmation_matches: false,
            reencrypt: false,
            busy: false,
            generating: false,
            suggestion_retry_blocked: false,
            ui_operation_id: None,
            message: None,
            progress: OperationProgress::default(),
            bridges: RefCell::new(Vec::new()),
            subscriptions: Vec::new(),
            progress_task: None,
            focus_pending: Some(
                if matches!(action, PasswordAction::Change | PasswordAction::Remove) {
                    PasswordField::Current
                } else {
                    PasswordField::New
                },
            ),
        };
        for input in [&this.current, &this.password, &this.confirmation] {
            this.subscriptions.push(cx.subscribe(
                input,
                |this, _input, _event: &SecretInputChanged, cx| {
                    this.assess(cx);
                    this.message = None;
                    cx.notify();
                },
            ));
        }
        this.subscriptions.push(cx.subscribe(
            &this.confirmation,
            |this, _input, _event: &SecretInputSubmitted, cx| this.submit(cx),
        ));
        this.subscriptions.push(cx.subscribe(
            &this.current,
            |this, _input, _event: &SecretInputSubmitted, cx| {
                if this.action == PasswordAction::Remove {
                    this.submit(cx);
                } else {
                    this.focus_pending = Some(if this.suggested {
                        PasswordField::Confirmation
                    } else {
                        PasswordField::New
                    });
                    cx.notify();
                }
            },
        ));
        this.subscriptions.push(cx.subscribe(
            &this.password,
            |this, _input, _event: &SecretInputSubmitted, cx| {
                this.focus_pending = Some(PasswordField::Confirmation);
                cx.notify();
            },
        ));
        this
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.busy
    }

    /// Correlates display feedback only; the host still validates every effect.
    pub(crate) fn pending_operation_id(&self) -> Option<&str> {
        self.busy
            .then_some(self.ui_operation_id.as_deref())
            .flatten()
    }

    #[cfg(test)]
    pub(crate) fn set_busy_for_test(&mut self, busy: bool) {
        self.busy = busy;
        if busy && self.ui_operation_id.is_none() {
            self.ui_operation_id = Some(uuid::Uuid::new_v4().to_string());
        }
    }

    fn suggest(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.suggested = true;
        self.password.update(cx, |input, cx| input.clear(cx));
        self.generate(cx);
        cx.notify();
    }

    fn use_typed(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.suggested = false;
        self.suggestion_retry_blocked = false;
        self.phrase = None;
        self.confirmation.update(cx, |input, cx| input.clear(cx));
        self.focus_pending = Some(PasswordField::New);
        self.assess(cx);
        self.message = None;
        cx.notify();
    }

    fn copy_phrase(&self, cx: &mut Context<Self>) {
        if let Some(phrase) = &self.phrase {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                phrase.passphrase.expose().to_owned(),
            ));
        }
    }

    /// The heading the form shows for its action; also the dialog's
    /// accessible name in Settings.
    pub(crate) fn title(&self) -> gpui::SharedString {
        match self.action {
            PasswordAction::Set | PasswordAction::Setup => {
                tr("settings-password-enable-encryption")
            }
            PasswordAction::Change => tr("settings-password-change-unlock-password"),
            PasswordAction::Remove => tr("settings-password-remove-encryption"),
        }
    }

    /// Leave the form without doing anything; a no-op while an operation
    /// runs. The Cancel button, and the Settings dialog's Escape and
    /// backdrop, all go through here.
    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.current.update(cx, |input, cx| input.clear(cx));
        self.password.update(cx, |input, cx| input.clear(cx));
        self.confirmation.update(cx, |input, cx| input.clear(cx));
        self.phrase = None;
        cx.emit(PasswordFormFinished {
            notice: None,
            cancelled: true,
            mode_notice: None,
            completion_operation_id: None,
        });
    }
    fn call<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = Result<T, crate::backend::HostFailure>>
        + Send
        + 'static,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<T, super::task::Failure>, &mut Context<Self>) + 'static,
    ) {
        let bridge =
            super::task::call_semantic(&self.backend, future, "flows-task-cancelled", cx, then);
        super::task::retain(&self.bridges, bridge);
    }
    fn generate(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.generating {
            return;
        }
        self.generating = true;
        self.suggestion_retry_blocked = false;
        self.message = None;
        self.phrase = None;
        self.confirmation_matches = false;
        self.confirmation.update(cx, |input, cx| input.clear(cx));
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control_presented::<SuggestedPhrase>("generate_store_passphrase", ())
                    .await
            },
            cx,
            |this, result, cx| {
                this.generating = false;
                if !this.suggested {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(phrase) => {
                        this.phrase = Some(phrase);
                        this.assess(cx);
                        this.focus_pending = Some(PasswordField::Confirmation);
                    }
                    Err(message) => this.message = Some(message.into()),
                }
                cx.notify();
            },
        );
    }
    fn assess(&mut self, cx: &Context<Self>) {
        self.assessment = if self.suggested {
            self.phrase.as_ref().map(|phrase| phrase.assessment)
        } else {
            let context = self.context.iter().map(String::as_str).collect::<Vec<_>>();
            self.password
                .read(cx)
                .with_secret(|value| keys::assess_password(value, &context).ok())
        };
        self.confirmation_matches = self.confirmation.read(cx).with_secret(|confirmation| {
            if self.suggested {
                self.phrase.as_ref().is_some_and(|phrase| {
                    normalized_equal(phrase.passphrase.expose(), confirmation)
                })
            } else {
                self.password
                    .read(cx)
                    .with_secret(|password| normalized_equal(password, confirmation))
            }
        });
    }
    fn validation_message(&self, cx: &Context<Self>) -> Option<UiText> {
        if self.busy {
            return Some(UiText::key(
                "settings-password-wait-for-the-current-change-to-finish",
            ));
        }
        if matches!(self.action, PasswordAction::Change | PasswordAction::Remove)
            && self.current.read(cx).is_empty()
        {
            return Some(UiText::key(
                "settings-password-enter-your-current-unlock-password-to-continue",
            ));
        }
        if self.action == PasswordAction::Remove {
            return None;
        }
        if self.suggested && self.suggestion_retry_blocked {
            return Some(UiText::key(
                "settings-password-keep-this-passphrase-maple-could-not-confirm-the-previous-attempt",
            ));
        }
        if self.suggested && self.generating {
            return Some(UiText::key(
                "settings-password-wait-for-your-passphrase-to-finish-generating",
            ));
        }
        if self.suggested && self.phrase.is_none() {
            return Some(UiText::key(
                "settings-password-generate-a-passphrase-or-enter-your-own-password",
            ));
        }
        if !self.suggested && self.password.read(cx).is_empty() {
            return Some(UiText::key("settings-password-enter-a-new-unlock-password"));
        }
        let Some(assessment) = self.assessment else {
            return Some(UiText::key(
                "settings-password-use-between-15-and-1-024-characters-for-your-new-password",
            ));
        };
        if assessment.reason == keys::StrengthReason::Length {
            return Some(UiText::key(
                "settings-password-use-between-15-and-1-024-characters-for-your-new-password",
            ));
        }
        if !assessment.allowed() {
            return Some(UiText::key(
                "settings-password-choose-a-less-predictable-password-try-a-longer-unique-phrase",
            ));
        }
        if self.confirmation.read(cx).is_empty() {
            return Some(UiText::key(
                "settings-password-re-enter-your-new-unlock-password-in-confirm-password",
            ));
        }
        if !self.confirmation_matches {
            return Some(UiText::key(
                "settings-password-the-passwords-don-t-match-re-enter-your-new-password-in-confirm-p",
            ));
        }
        None
    }
    fn can_submit(&self, cx: &Context<Self>) -> bool {
        self.validation_message(cx).is_none()
    }
    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.can_submit(cx) {
            if !self.busy {
                self.message = self.validation_message(cx);
                cx.notify();
            }
            return;
        }
        let current = self.current.update(cx, |input, cx| input.take_secret(cx));
        let password = if self.suggested {
            Zeroizing::new(
                self.phrase
                    .as_ref()
                    .map(|phrase| phrase.passphrase.expose().to_owned())
                    .unwrap_or_default(),
            )
        } else {
            self.password.update(cx, |input, cx| input.take_secret(cx))
        };
        self.confirmation.update(cx, |input, cx| input.clear(cx));
        let generated_id = if self.suggested {
            self.phrase.as_ref().map(|phrase| phrase.id.clone())
        } else {
            None
        };
        self.busy = true;
        let ui_operation_id = uuid::Uuid::new_v4().to_string();
        self.ui_operation_id = Some(ui_operation_id.clone());
        self.message = None;
        self.progress = OperationProgress::default();
        self.poll_progress(cx);
        let action = self.action;
        let reencrypt = self.reencrypt;
        let backend = self.backend.clone();
        self.call(
            async move {
                let result: Result<Value, crate::backend::HostFailure> = async {
                    if action != PasswordAction::Setup {
                        crate::settings::flush_pending().await?;
                        backend.flush_ui_settings().await?;
                    }
                    match action {
                        PasswordAction::Set => {
                            backend
                                .local_control_presented::<Value>(
                                    "set_store_password",
                                    PasswordRequest {
                                        password: &password,
                                        generated_id: generated_id.as_deref(),
                                        ui_operation_id: Some(&ui_operation_id),
                                    },
                                )
                                .await
                        }
                        PasswordAction::Change => {
                            backend
                                .local_control_presented::<Value>(
                                    "change_store_password",
                                    ChangeRequest {
                                        current_password: &current,
                                        password: &password,
                                        reencrypt,
                                        generated_id: generated_id.as_deref(),
                                        ui_operation_id: Some(&ui_operation_id),
                                    },
                                )
                                .await
                        }
                        PasswordAction::Remove => {
                            backend
                                .local_control_presented::<Value>(
                                    "remove_store_password",
                                    CurrentRequest {
                                        current_password: &current,
                                        ui_operation_id: Some(&ui_operation_id),
                                    },
                                )
                                .await
                        }
                        PasswordAction::Setup => {
                            backend
                                .local_control_presented::<Value>(
                                    "setup_store",
                                    SetupRequest {
                                        password: Some(&password),
                                        generated_id: generated_id.as_deref(),
                                        ui_operation_id: Some(&ui_operation_id),
                                    },
                                )
                                .await
                        }
                    }
                }
                .await;
                // A host-owned receipt survives only a proven precommit
                // rejection. Query it after failure; never replay automatically.
                let generated_retry_valid = if result.is_err() && generated_id.is_some() {
                    backend
                        .local_control_presented::<PasswordAssessment>(
                            "assess_store_password",
                            PasswordRequest {
                                password: &password,
                                generated_id: generated_id.as_deref(),
                                ui_operation_id: None,
                            },
                        )
                        .await
                        .is_ok()
                } else {
                    false
                };
                Ok(PasswordAttempt {
                    result: result.map_err(Into::into),
                    generated_retry_valid,
                })
            },
            cx,
            |this, result, cx| this.submission_completed(result, cx),
        );
    }

    fn submission_completed(
        &mut self,
        attempt: Result<PasswordAttempt, super::task::Failure>,
        cx: &mut Context<Self>,
    ) {
        let PasswordAttempt {
            result,
            generated_retry_valid,
        } = attempt.unwrap_or_else(|message| PasswordAttempt {
            result: Err(message),
            generated_retry_valid: false,
        });
        self.busy = false;
        self.progress_task = None;
        self.progress = OperationProgress::default();
        match result {
            Ok(result) => {
                self.phrase = None;
                cx.emit(PasswordFormFinished {
                    notice: operation_notice(&result),
                    cancelled: false,
                    mode_notice: result
                        .get("mode_change_notice")
                        .cloned()
                        .and_then(|value| serde_json::from_value(value).ok()),
                    completion_operation_id: result
                        .get("completion_operation_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
            Err(message) => {
                self.suggestion_retry_blocked =
                    self.suggested && self.phrase.is_some() && !generated_retry_valid;
                self.assess(cx);
                self.message = Some(if self.suggestion_retry_blocked {
                    UiText::context(
                        "settings-password-unconfirmed-attempt",
                        "message",
                        message.into(),
                    )
                } else {
                    message.into()
                });
            }
        }
        cx.notify();
    }

    fn poll_progress(&mut self, cx: &mut Context<Self>) {
        let operation_id = self.ui_operation_id.clone();
        let action = self.action;
        self.progress_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let Ok(Some(backend)) =
                    this.update(cx, |this, _cx| this.busy.then(|| this.backend.clone()))
                else {
                    break;
                };
                let control = backend.clone();
                let response = backend
                    .spawn(async move {
                        control
                            .local_control_presented::<OperationProgress>("store_progress", ())
                            .await
                    })
                    .await;
                let Ok(Ok(progress)) = response else {
                    continue;
                };
                if this
                    .update(cx, |this, cx| {
                        if this.busy
                            && this.ui_operation_id == operation_id
                            && progress.belongs_to(operation_id.as_deref(), action)
                        {
                            this.progress = progress;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.busy || !self.progress.cancellable {
            return;
        }
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control_presented::<bool>("cancel_store_operation", ())
                    .await
            },
            cx,
            |this, result, cx| {
                this.message = Some(match result {
                    Ok(true) => {
                        UiText::key("settings-password-cancelling-before-the-store-is-replaced")
                    }
                    Ok(false) => UiText::key(
                        "settings-password-the-store-is-committing-wait-for-it-to-finish",
                    ),
                    Err(message) => message.into(),
                });
                cx.notify();
            },
        );
    }
}

impl Render for PasswordForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.busy
            && let Some(field) = self.focus_pending.take()
        {
            let input = match field {
                PasswordField::Current => &self.current,
                PasswordField::New => &self.password,
                PasswordField::Confirmation => &self.confirmation,
            };
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        let (title, description, submit_label) = match self.action {
            PasswordAction::Set | PasswordAction::Setup => (
                self.title(),
                tr(
                    "settings-password-protect-saved-conversation-text-settings-and-sign-in-attached-fil",
                ),
                tr("settings-password-enable-encryption"),
            ),
            PasswordAction::Change => (
                self.title(),
                tr("settings-password-enter-your-current-unlock-password-then-choose-a-new-one"),
                tr("settings-password-save-password"),
            ),
            PasswordAction::Remove => (
                self.title(),
                tr(
                    "settings-password-your-local-maple-data-will-no-longer-be-encrypted-enter-your-curr",
                ),
                tr("settings-password-remove-encryption"),
            ),
        };
        let mut pane = widgets::card_row()
            .w(px(560.))
            .max_w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_5()
            .text_color(gpui::rgb(theme::text_primary()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_secondary()))
                            .child(description),
                    ),
            );
        if self.busy {
            pane = pane.child(div().text_sm().child(progress_copy(
                Some(self.action),
                &self.progress,
                &self.messages,
            )));
            if self.progress.cancellable {
                pane = pane.child(
                    widgets::secondary_button("store-operation-cancel")
                        .tab_index(1)
                        .on_click(cx.listener(|this, _event, _window, cx| this.cancel(cx)))
                        .child(gpui::text!(tr("settings-password-cancel"))),
                );
            }
        } else {
            if matches!(self.action, PasswordAction::Change | PasswordAction::Remove) {
                pane = pane.child(password_field(
                    "settings-password-current-unlock-password",
                    self.current.clone(),
                ));
            }
            if self.action != PasswordAction::Remove {
                let mut new_password = div().flex().flex_col().gap_2().child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(tr("settings-password-new-unlock-password")),
                        )
                        .when(!self.suggested, |row| {
                            row.child(
                                widgets::ghost_button("store-use-suggestion")
                                    .tab_index(7)
                                    .on_click(
                                        cx.listener(|this, _event, _window, cx| this.suggest(cx)),
                                    )
                                    .on_key_down(cx.listener(
                                        |this, event: &gpui::KeyDownEvent, window, cx| {
                                            if password_action_key(event, window, cx) {
                                                this.suggest(cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .child(gpui::text!(tr(
                                        "settings-password-generate-passphrase"
                                    ))),
                            )
                        }),
                );
                if self.suggested {
                    let mut suggestion = div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_3()
                        .rounded(theme::RADIUS_MD)
                        .border_1()
                        .border_color(gpui::rgb(theme::border()))
                        .bg(gpui::rgb(theme::bg_input()));
                    if let Some(phrase) = &self.phrase {
                        suggestion =
                            suggestion.child(suggested_phrase_display(phrase.passphrase.clone()));
                    } else {
                        suggestion = suggestion.child(div().text_sm().child(if self.generating {
                            tr("settings-password-generating-a-random-passphrase")
                        } else {
                            tr("settings-password-couldn-t-generate-a-passphrase-try-again-or-enter-your-own-passwo")
                        }));
                    }
                    suggestion = suggestion
                        .child(div().text_xs().text_color(gpui::rgb(theme::text_secondary()))
                            .child(tr("settings-password-six-random-words-save-them-in-your-password-manager-before-contin")))
                        .child(div().flex().flex_wrap().gap_2()
                            .child(widgets::secondary_button("store-copy-phrase")
                                .tab_index(7)
                                .on_click(cx.listener(|this, _event, _window, cx| this.copy_phrase(cx)))
                                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) { this.copy_phrase(cx); cx.stop_propagation(); }
                                }))
                                .child(gpui::text!(tr("settings-password-copy-passphrase"))))
                            .child(widgets::ghost_button("store-another-phrase")
                                .tab_index(8)
                                .on_click(cx.listener(|this, _event, _window, cx| this.generate(cx)))
                                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) { this.generate(cx); cx.stop_propagation(); }
                                }))
                                .child(gpui::text!(tr("settings-password-generate-another"))))
                            .child(widgets::ghost_button("store-use-typed")
                                .tab_index(9)
                                .on_click(cx.listener(|this, _event, _window, cx| this.use_typed(cx)))
                                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) { this.use_typed(cx); cx.stop_propagation(); }
                                }))
                                .child(gpui::text!(tr("settings-password-use-my-own-password")))));
                    new_password = new_password.child(suggestion);
                } else {
                    new_password = new_password
                        .child(password_input_frame(
                            "password-new-frame",
                            self.password.clone(),
                        ))
                        .child(
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_secondary()))
                                .child(
                                    tr("settings-password-use-at-least-15-characters-a-long-unique-phrase-works-well"),
                                ),
                        );
                }
                if let Some(assessment) = self.assessment {
                    let (label, color) = meter_presentation(Some(assessment.level));
                    new_password =
                        new_password.child(div().text_sm().text_color(gpui::rgb(color)).child(
                            if assessment.reason == keys::StrengthReason::Length {
                                tr("settings-password-use-between-15-and-1-024-characters")
                                    .to_string()
                            } else {
                                self.messages
                                    .get(
                                        "settings-password-strength",
                                        [("strength", label.to_string().into())],
                                    )
                                    .to_string()
                            },
                        ));
                }
                pane = pane.child(new_password).child(password_field(
                    "settings-password-confirm-password",
                    self.confirmation.clone(),
                ));
                if !self.confirmation.read(cx).is_empty() {
                    pane = pane.child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(if self.confirmation_matches {
                                theme::status_success()
                            } else {
                                theme::status_error()
                            }))
                            .child(if self.confirmation_matches {
                                tr("settings-password-passwords-match")
                            } else {
                                tr("settings-password-passwords-don-t-match")
                            }),
                    );
                }
                if self.action == PasswordAction::Change {
                    pane = pane.child(div().flex().flex_col().gap_1()
                        .child(widgets::checkbox("store-reencrypt", self.reencrypt, tr("settings-password-also-create-a-new-encryption-key"))
                            .debug_selector(|| "store-reencrypt".into())
                            .tab_index(4)
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.reencrypt = !this.reencrypt;
                                cx.notify();
                            }))
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                if password_action_key(event, window, cx) {
                                    this.reencrypt = !this.reencrypt;
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                            })))
                        .child(div().pl_7().text_xs().text_color(gpui::rgb(theme::text_secondary()))
                            .child(tr("settings-password-use-a-new-encryption-key-if-your-old-password-and-a-copy-of-this"))));
                }
                pane = pane.child(div().flex().flex_col().gap_1().text_xs()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(tr("settings-password-this-is-separate-from-your-maple-account-password-maple-can-t-res"))
                    .child(tr("settings-password-if-you-forget-it-you-ll-need-to-erase-this-version-s-local-data-a"))
                    .when(matches!(self.action, PasswordAction::Set | PasswordAction::Setup), |note| note.child(
                        tr("settings-password-existing-backups-snapshots-and-previous-app-data-aren-t-re-encryp"),
                    )));
            }
        }
        if let Some(message) = &self.message {
            pane = pane.child(
                div()
                    .id("store-password-feedback")
                    .role(gpui::Role::Alert)
                    .debug_selector(|| "store-password-feedback".into())
                    .text_sm()
                    .text_color(gpui::rgb(theme::status_error()))
                    .child(message.clone()),
            );
        }
        if !self.busy {
            pane = pane.child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        widgets::ghost_button("store-password-close")
                            .tab_index(6)
                            .on_click(cx.listener(|this, _event, _window, cx| this.close(cx)))
                            .on_key_down(cx.listener(
                                |this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) {
                                        this.close(cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .child(gpui::text!(tr("settings-password-cancel"))),
                    )
                    .child(
                        widgets::primary_button("store-password-submit")
                            .debug_selector(|| "store-password-submit".into())
                            .tab_index(5)
                            .on_click(cx.listener(|this, _event, _window, cx| this.submit(cx)))
                            .on_key_down(cx.listener(
                                |this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) {
                                        this.submit(cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .child(gpui::text!(submit_label)),
                    ),
            );
        }
        pane
    }
}

fn password_field(label: &'static str, input: Entity<SecretInput>) -> gpui::Div {
    let selector = match label {
        "settings-password-current-unlock-password" => "password-current-frame",
        "settings-password-confirm-password" => "password-confirmation-frame",
        _ => "password-unlock-frame",
    };
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(tr(label)),
        )
        .child(password_input_frame(selector, input))
}

fn password_input_frame(
    selector: &'static str,
    input: Entity<SecretInput>,
) -> gpui::Stateful<gpui::Div> {
    let focus_input = input.clone();
    widgets::input_frame()
        .id(selector)
        .debug_selector(move || selector.into())
        .w_full()
        .min_h(px(40.))
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            focus_input.update(cx, |input, cx| input.focus(window, cx));
        })
        .child(input)
}

// Standalone form controls share Tab navigation; SecretInput owns its own
// editor key handling and never sends password text through these callbacks.
fn password_action_key(
    event: &gpui::KeyDownEvent,
    window: &mut Window,
    cx: &mut gpui::App,
) -> bool {
    let plain = !event.keystroke.modifiers.control
        && !event.keystroke.modifiers.alt
        && !event.keystroke.modifiers.platform;
    if plain && event.keystroke.key == "tab" {
        if event.keystroke.modifiers.shift {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
        cx.stop_propagation();
        return false;
    }
    plain && matches!(event.keystroke.key.as_str(), "enter" | "space")
}

#[derive(Serialize)]
struct PasswordRequest<'a> {
    password: &'a str,
    generated_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ui_operation_id: Option<&'a str>,
}
#[derive(Serialize)]
struct ChangeRequest<'a> {
    current_password: &'a str,
    password: &'a str,
    reencrypt: bool,
    generated_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ui_operation_id: Option<&'a str>,
}
#[derive(Serialize)]
struct CurrentRequest<'a> {
    current_password: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ui_operation_id: Option<&'a str>,
}
#[derive(Serialize)]
struct SetupRequest<'a> {
    password: Option<&'a str>,
    generated_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ui_operation_id: Option<&'a str>,
}
#[derive(Serialize)]
pub(crate) struct ConfirmationRequest<'a> {
    pub confirmation: &'a str,
}

fn normalized_equal(left: &str, right: &str) -> bool {
    if right.is_empty() {
        return false;
    }
    matches!((Password::normalize(left),Password::normalize(right)),(Ok(left),Ok(right)) if left.as_str()==right.as_str())
}

/// Generated EFF phrases use this small public ASCII alphabet. Shape each
/// public glyph separately, then choose its position while painting. The full
/// phrase and its words never enter SharedString, layout or accessibility text
/// caches. Painting and an explicit Copy still expose the phrase to the OS.
fn suggested_phrase_display(phrase: SecretString) -> impl IntoElement {
    const ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz '-0123456789";
    gpui::canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| {
            let style = window.text_style();
            let font_size = px(16.);
            let color = gpui::rgb(theme::text_primary()).into();
            let glyphs = ALPHABET
                .bytes()
                .map(|byte| {
                    let text = gpui::SharedString::from(char::from(byte).to_string());
                    let run = gpui::TextRun {
                        len: 1,
                        font: style.font(),
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    (
                        byte,
                        window
                            .text_system()
                            .shape_line(text, font_size, &[run], None),
                    )
                })
                .collect::<Vec<_>>();
            let mut x = bounds.left();
            let mut y = bounds.top();
            let line_height = px(24.);
            for word in phrase.expose().split(' ') {
                let width = word
                    .bytes()
                    .filter_map(|byte| glyphs.iter().find(|(glyph, _)| *glyph == byte))
                    .map(|(_, line)| line.width())
                    .fold(px(0.), |sum, width| sum + width);
                if x > bounds.left() && x + width > bounds.right() {
                    x = bounds.left();
                    y += line_height;
                }
                for byte in word.bytes().chain(std::iter::once(b' ')) {
                    if let Some((_, line)) = glyphs.iter().find(|(glyph, _)| *glyph == byte) {
                        let _ = line.paint(
                            gpui::point(x, y),
                            line_height,
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                        x += line.width();
                    }
                }
            }
        },
    )
    .w_full()
    .h(px(72.))
}

pub(crate) fn operation_notice(result: &Value) -> Option<UiText> {
    let flag = |name| result.get(name).and_then(Value::as_bool).unwrap_or(false);
    if flag("synchronization_pending") || flag("sync_pending") {
        Some(UiText::key(
            "settings-password-the-new-unlock-password-took-effect-but-maple-couldn-t-confirm-it",
        ))
    } else if flag("backup_pending") {
        Some(UiText::key(
            "settings-password-the-new-unlock-password-took-effect-maple-still-needs-to-synchron",
        ))
    } else if flag("used_previous_password") {
        Some(UiText::key(
            "settings-password-maple-opened-with-your-previous-unlock-password-keep-your-current",
        ))
    } else if flag("reconciliation_pending") {
        Some(UiText::key(
            "settings-password-maple-is-open-key-file-synchronization-or-its-security-upgrade-st",
        ))
    } else if flag("notice_pending") {
        Some(UiText::key(
            "settings-password-encryption-changed-maple-still-needs-to-save-the-local-mode-chang",
        ))
    } else {
        None
    }
}

fn meter_presentation(level: Option<StrengthLevel>) -> (gpui::SharedString, u32) {
    match level {
        Some(StrengthLevel::TooEasy) => (
            tr("settings-password-too-easy-to-guess"),
            theme::status_error(),
        ),
        Some(StrengthLevel::CouldBeStronger) => (
            tr("settings-password-could-be-stronger"),
            theme::status_warning(),
        ),
        Some(StrengthLevel::Strong) => (tr("settings-password-strong"), theme::status_success()),
        None => (
            tr("settings-password-enter-a-password"),
            theme::text_muted(),
        ),
    }
}
impl OperationProgress {
    fn belongs_to(&self, id: Option<&str>, action: PasswordAction) -> bool {
        self.busy
            && id.is_some()
            && self.ui_operation_id.as_deref() == id
            && progress_action(self.operation.as_deref()) == Some(action)
    }
}

fn progress_action(operation: Option<&str>) -> Option<PasswordAction> {
    match operation {
        Some("set_store_password") => Some(PasswordAction::Set),
        Some("change_store_password") => Some(PasswordAction::Change),
        Some("remove_store_password") => Some(PasswordAction::Remove),
        Some("setup_store") => Some(PasswordAction::Setup),
        _ => None,
    }
}

fn readable_bytes(bytes: u64) -> String {
    for (unit, divisor) in [("GiB", 1_u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if bytes >= divisor {
            return format!("{:.1} {unit}", bytes as f64 / divisor as f64);
        }
    }
    format!("{bytes} B")
}

fn progress_copy(
    action: Option<PasswordAction>,
    progress: &OperationProgress,
    messages: &super::i18n::MessageCache,
) -> String {
    let verb = tr(match action {
        Some(PasswordAction::Remove) => "settings-password-removing-encryption",
        Some(PasswordAction::Change) => "settings-password-updating-your-unlock-password",
        Some(PasswordAction::Set | PasswordAction::Setup) => {
            "settings-password-encrypting-your-local-data"
        }
        None => "settings-password-updating-your-local-store",
    });
    let exporting = progress.busy && progress.phase == "exporting";
    let mut parts = vec![verb.to_string() + "."];
    if let Some(ms) = progress.remaining_ms.filter(|ms| exporting && *ms > 0) {
        let minutes = (ms.saturating_add(59_999) / 60_000).max(1);
        parts.push(
            messages
                .get(
                    "settings-password-progress-estimate",
                    [("count", minutes.into())],
                )
                .to_string(),
        );
    }
    if exporting && progress.total_bytes > 0 {
        parts.push(
            messages
                .get(
                    "settings-password-progress-bytes",
                    [
                        ("done", readable_bytes(progress.completed_bytes).into()),
                        ("total", readable_bytes(progress.total_bytes).into()),
                    ],
                )
                .to_string(),
        );
    }
    if progress.busy && progress.phase == "committing" {
        parts.push(tr("settings-password-progress-committing").to_string());
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice_status() -> StoreStatus {
        serde_json::from_value(serde_json::json!({
            "state": {"state": "open"},
            "mode": "plain",
            "store_id": "current",
            "mode_change_notice": {
                "store_id": "current", "notice_id": "exact", "message": "Encryption changed."
            },
            "completion_notice": {
                "operation_id": "completed", "message": "Maple local data is ready."
            },
            "persistence_status": {
                "store_id": "current", "revision": 3,
                "failure": {"store_id": "current", "revision": 3, "reason": "save_failed"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn routine_completion_does_not_become_a_local_attention_notice() {
        let mut status = notice_status();
        assert!(status.attention_notice().is_none());
        status
            .completion_notice
            .as_mut()
            .unwrap()
            .requires_attention = true;
        assert!(status.attention_notice().is_some());
        status
            .completion_notice
            .as_mut()
            .unwrap()
            .requires_attention = false;
        status.completion_notice.as_mut().unwrap().is_error = true;
        assert!(status.attention_notice().is_some());
        status.store_id = None;
        assert!(status.attention_notice().is_none());
    }

    #[test]
    fn local_store_notices_require_open_matching_generation() {
        let mut status = notice_status();
        assert!(status.current_mode_notice().is_some());
        assert!(status.current_persistence_failure().is_some());
        status.store_id = Some("replacement".into());
        assert!(status.current_mode_notice().is_none());
        assert!(status.current_persistence_failure().is_none());
        status.store_id = Some("current".into());
        status.state = StoreState::Locked;
        assert!(status.current_mode_notice().is_none());
        assert!(status.current_persistence_failure().is_none());
        status.state = StoreState::Open;
        status.persistence_status.failure.as_mut().unwrap().store_id = Some("old".into());
        assert!(status.current_persistence_failure().is_none());
    }

    #[gpui::test]
    fn refusal_ignores_stale_setup_and_reset_actions(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let state = StoreState::Refused {
            reason: RefusalReason::NewerVersion,
        };
        let gate = cx.new(|cx| StoreGate::new_mounted(test_backend(), state.clone(), cx));
        gate.update(cx, |gate, cx| {
            let mut status = notice_status();
            status.state = state;
            status.password_settings_available = true;
            gate.status = Some(status);
            gate.open_erase(cx);
            gate.setup_password(cx);
            gate.plain_setup(cx);
            gate.reset(true, cx);
            gate.reset_with_confirmation(true, "START FRESH".into(), cx);
            gate.reset_with_confirmation(false, "ERASE".into(), cx);
            assert!(!gate.erase);
            assert!(gate.form.is_none());
            assert!(!gate.busy);
            assert!(gate.message.is_none());
            assert!(!accepts_unlock(&gate.state));
        });
    }

    #[gpui::test]
    fn external_gate_notice_is_shown_inside_its_live_password_form(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let gate = cx.new(|cx| StoreGate::new_mounted(test_backend(), StoreState::Locked, cx));
        let form =
            cx.new(|cx| PasswordForm::new(test_backend(), PasswordAction::Remove, Vec::new(), cx));
        gate.update(cx, |gate, cx| {
            gate.form = Some(form.clone());
            gate.show_local_store_notice("Local storage needs attention.", cx);
            assert!(gate.message.is_none());
            assert_eq!(
                form.read(cx)
                    .message
                    .as_ref()
                    .map(UiText::resolve)
                    .as_deref(),
                Some("Local storage needs attention.")
            );
            gate.form = None;
            gate.show_local_store_notice("Retry unlocking.", cx);
            assert_eq!(
                gate.message.as_ref().map(UiText::resolve).as_deref(),
                Some("Retry unlocking.")
            );
        });
    }

    #[test]
    fn meter_states_at_the_boundaries() {
        assert_eq!(
            meter_presentation(Some(StrengthLevel::TooEasy)).0,
            "Too easy to guess"
        );
        assert_eq!(
            meter_presentation(Some(StrengthLevel::CouldBeStronger)).0,
            "Could be stronger"
        );
        assert_eq!(meter_presentation(Some(StrengthLevel::Strong)).0, "Strong");
    }
    #[test]
    fn only_locked_and_generation_damage_offer_authenticated_unlock() {
        assert!(accepts_unlock(&StoreState::Locked));
        assert!(accepts_unlock(&StoreState::Damaged {
            reason: DamageReason::Generation,
        }));
        for state in [
            StoreState::Open,
            StoreState::Maintenance,
            StoreState::NeedsSetup,
            StoreState::Damaged {
                reason: DamageReason::Database,
            },
        ] {
            assert!(!accepts_unlock(&state));
        }
    }
    #[test]
    fn confirmation_compares_nfc_and_rejects_empty() {
        assert!(normalized_equal("Cafe\u{301}", "Café"));
        assert!(!normalized_equal("", ""));
    }
    #[test]
    fn nested_password_failure_reformats_both_context_and_host_reason() {
        let _locale = super::super::i18n::TestLocalizationGuard::enable();
        let failure: super::super::task::Failure = crate::backend::HostFailure::new(
            "key-wrong-password",
            "That password didn't unlock Maple.",
        )
        .into();
        let notice = UiText::context(
            "settings-password-unconfirmed-attempt",
            "message",
            failure.into(),
        );
        super::super::i18n::apply_preference(Some("en"));
        let english = notice.resolve();
        assert!(english.contains("That password didn't unlock Maple."));
        super::super::i18n::apply_preference(Some("es"));
        assert_ne!(notice.resolve(), english);
        assert!(
            !notice
                .resolve()
                .contains("That password didn't unlock Maple.")
        );
        super::super::i18n::apply_preference(Some("en"));
        assert_eq!(notice.resolve(), english);
    }

    #[test]
    fn completed_password_notices_retain_semantics_across_language_changes() {
        let _locale = crate::ui::i18n::TestLocalizationGuard::enable();
        for flag in [
            "synchronization_pending",
            "sync_pending",
            "backup_pending",
            "used_previous_password",
            "reconciliation_pending",
            "notice_pending",
        ] {
            let event = PasswordFormFinished {
                notice: operation_notice(&serde_json::json!({flag:true})),
                cancelled: false,
                mode_notice: None,
                completion_operation_id: None,
            };
            let notice = event.notice.unwrap();
            crate::ui::i18n::apply_preference(Some("en"));
            let english = notice.resolve();
            crate::ui::i18n::apply_preference(Some("es"));
            assert_ne!(notice.resolve(), english);
            crate::ui::i18n::apply_preference(Some("en"));
            assert_eq!(notice.resolve(), english);
        }
    }

    #[test]
    fn committed_notice_does_not_claim_rollback() {
        let notice = operation_notice(
            &serde_json::json!({"synchronization_pending":true,"backup_pending":true}),
        )
        .unwrap();
        assert!(notice.resolve().contains("took effect"));
        assert!(notice.resolve().contains("both passwords"));
        assert!(!notice.resolve().contains("rolled back"));
    }
    #[test]
    fn progress_uses_readable_copy_units_and_singular_estimates_only_while_exporting() {
        let mut progress = OperationProgress {
            busy: true,
            phase: "exporting".into(),
            completed_bytes: 1024 * 1024,
            total_bytes: 2 * 1024 * 1024,
            remaining_ms: Some(60_000),
            ..Default::default()
        };
        let text = progress_copy(Some(PasswordAction::Remove), &progress, &Default::default())
            .replace(['\u{2068}', '\u{2069}'], "");
        assert!(text.starts_with("Removing encryption."));
        assert!(text.contains("1.0 MiB of 2.0 MiB"));
        assert!(text.contains("1 minute left"));
        assert!(!text.contains("1 minutes"));
        progress.remaining_ms = Some(60_001);
        assert!(
            progress_copy(None, &progress, &Default::default())
                .replace(['\u{2068}', '\u{2069}'], "")
                .contains("2 minutes left")
        );
        progress.remaining_ms = Some(0);
        assert!(
            !progress_copy(None, &progress, &Default::default())
                .replace(['\u{2068}', '\u{2069}'], "")
                .contains("left")
        );
        for phase in ["verifying", "committing", "complete"] {
            progress.phase = phase.into();
            let text = progress_copy(None, &progress, &Default::default())
                .replace(['\u{2068}', '\u{2069}'], "");
            assert!(!text.contains("left"));
            assert!(!text.contains("Copied"));
            assert!(!text.contains("committed"));
        }
        assert!(
            progress_copy(None, &progress, &Default::default())
                .replace(['\u{2068}', '\u{2069}'], "")
                .starts_with("Updating your local store.")
        );
        assert_eq!(readable_bytes(512), "512 B");
        assert_eq!(readable_bytes(1024), "1.0 KiB");
        assert_eq!(readable_bytes(1 << 30), "1.0 GiB");
    }

    #[test]
    fn form_progress_requires_the_current_active_operation_and_action() {
        let mut progress = OperationProgress {
            busy: true,
            operation: Some("change_store_password".into()),
            ui_operation_id: Some("current-attempt".into()),
            ..Default::default()
        };
        assert!(progress.belongs_to(Some("current-attempt"), PasswordAction::Change));
        assert!(!progress.belongs_to(Some("previous-attempt"), PasswordAction::Change));
        assert!(!progress.belongs_to(Some("current-attempt"), PasswordAction::Remove));
        assert!(!progress.belongs_to(None, PasswordAction::Change));
        progress.busy = false;
        assert!(!progress.belongs_to(Some("current-attempt"), PasswordAction::Change));
    }
    fn test_backend() -> Arc<AgentBackend> {
        Arc::new(AgentBackend::for_test("http://127.0.0.1:1".into(), String::new()).unwrap())
    }
    #[gpui::test]
    fn red_password_disables_set(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let form =
            cx.new(|cx| PasswordForm::new(test_backend(), PasswordAction::Remove, Vec::new(), cx));
        form.update(cx, |form, cx| {
            form.action = PasswordAction::Set;
            form.suggested = false;
            form.password.update(cx, |input, cx| {
                input.replace_secret(Zeroizing::new("password".into()), cx)
            });
            form.confirmation.update(cx, |input, cx| {
                input.replace_secret(Zeroizing::new("password".into()), cx)
            });
            form.assess(cx);
            assert_eq!(form.assessment.unwrap().level, StrengthLevel::TooEasy);
            assert!(!form.can_submit(cx));
            form.password.update(cx, |input, cx| {
                input.replace_secret(Zeroizing::new("Z7jv!9qP#3nMx@8Ks^2dH".into()), cx)
            });
            form.confirmation.update(cx, |input, cx| {
                input.replace_secret(Zeroizing::new("Z7jv!9qP#3nMx@8Ks^2dH".into()), cx)
            });
            form.assess(cx);
            assert!(form.can_submit(cx));
        });
    }
    #[gpui::test]
    fn typed_password_is_default_and_generated_confirmation_is_required(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let form =
            cx.new(|cx| PasswordForm::new(test_backend(), PasswordAction::Remove, Vec::new(), cx));
        form.update(cx, |form, cx| {
            assert!(!form.suggested);
            assert!(!form.generating);
            assert!(!form.reencrypt);
            form.suggested = true;
            form.action = PasswordAction::Set;
            let phrase = "synthetic fixture amber basket orbit sample";
            let assessment = PasswordAssessment {
                level: StrengthLevel::Strong,
                reason: keys::StrengthReason::Accepted,
                guesses_log10: 20.,
            };
            form.phrase = Some(SuggestedPhrase {
                id: uuid::Uuid::new_v4().to_string(),
                passphrase: SecretString::new(phrase.into()),
                assessment,
            });
            form.assessment = Some(assessment);
            assert!(!form.can_submit(cx));
            form.confirmation.update(cx, |input, cx| {
                input.replace_secret(Zeroizing::new(phrase.into()), cx)
            });
            form.assess(cx);
            assert!(form.can_submit(cx));
        });
    }
    #[test]
    fn wrapper_upgrade_notice_keeps_existing_password_valid() {
        let notice = operation_notice(
            &serde_json::json!({"reconciliation_pending":true,"used_previous_password":false}),
        )
        .unwrap();
        assert!(
            notice
                .resolve()
                .contains("existing unlock password remains valid")
        );
        assert!(!notice.resolve().contains("previous unlock password"));
    }
    #[gpui::test]
    fn forgot_password_opens_erase_and_clears_hidden_input(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let gate = cx.new(|cx| StoreGate::new_mounted(test_backend(), StoreState::Locked, cx));
        gate.update(cx, |gate, cx| {
            gate.password.update(cx, |input, cx| {
                input.replace_secret(Zeroizing::new("synthetic attempted unlock".into()), cx)
            });
            gate.open_erase(cx);
            assert!(gate.erase);
            assert!(gate.password.read(cx).is_empty());
            assert_eq!(gate.state, StoreState::Locked);
        });
    }
    #[gpui::test]
    fn erase_requires_exact_typed_confirmation_before_any_request(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let gate = cx.new(|cx| StoreGate::new_mounted(test_backend(), StoreState::Locked, cx));
        gate.update(cx, |gate, cx| {
            gate.confirmation
                .update(cx, |input, cx| input.set_text("erase", cx));
            gate.reset(false, cx);
            assert!(!gate.busy);
            assert!(
                gate.message
                    .as_ref()
                    .unwrap()
                    .resolve()
                    .contains("Type ERASE exactly")
            );
            assert_eq!(gate.state, StoreState::Locked);
            gate.confirmation
                .update(cx, |input, cx| input.set_text("ERASE", cx));
            gate.reset(true, cx);
            assert!(!gate.busy);
            assert!(
                gate.message
                    .as_ref()
                    .unwrap()
                    .resolve()
                    .contains("Type START FRESH exactly")
            );
        });
    }
}

#[cfg(test)]
mod password_form_tests;
