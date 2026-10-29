//! Local-only store entry and password forms. These screens ask the host for
//! lifecycle status before loading any account-owned application data.

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

#[derive(Clone, Deserialize)]
pub(crate) struct StoreOperationNotice {
    pub operation_id: String,
    pub message: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub requires_attention: bool,
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
    message: Option<String>,
    progress: OperationProgress,
    repair_progress: RepairProgress,
    progress_task: Option<gpui::Task<()>>,
    focus_pending: bool,
}

impl EventEmitter<StoreGateChanged> for StoreGate {}

impl StoreGate {
    pub(crate) fn show_local_store_notice(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(form) = &self.form {
            form.update(cx, |form, cx| form.show_local_store_notice(message, cx));
        } else {
            self.message = Some(message);
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
            focus: cx.focus_handle(),
            backend,
            state,
            status: None,
            password: cx.new(|cx| SecretInput::new("Unlock password", cx).with_tab_index(1)),
            confirmation: cx.new(|cx| TextInput::new("Type START FRESH", cx).with_tab_index(1)),
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
                            control.local_control::<OperationProgress>("store_progress", ()),
                            control.local_control::<RepairProgress>("repair_progress", ())
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
                    .local_control::<bool>(
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
                    Ok(true) => "Cancelling before the store is replaced…".into(),
                    Ok(false) => "The store is committing. Wait for it to finish.".into(),
                    Err(message) => message,
                });
                cx.notify();
            },
        );
    }

    fn call<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    ) {
        let bridge = super::task::call(&self.backend, future, cx, then);
        super::task::retain(&self.bridges, bridge);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control::<StoreStatus>("store_status", ())
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
                    Err(message) => this.message = Some(message),
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
            self.message = Some("Enter your unlock password to continue.".into());
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
                    .local_control::<Value>(
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
                        this.message = Some(message);
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
            input.set_placeholder("Type ERASE", cx);
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
            input.set_placeholder("Type START FRESH", cx);
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
                    .local_control::<Value>(
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
                    Err(message) => this.message = Some(message),
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
            self.message = Some(format!("Type {required} exactly to continue."));
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
                    .local_control::<Value>(
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
                    Err(message) => this.message = Some(message),
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
            StoreState::Locked => (
                "Maple is locked.",
                "Enter your unlock password to open your tasks.",
            ),
            StoreState::NeedsSetup => (
                "Start fresh on this computer",
                "Choose a password for this version's new store, or continue without a password.",
            ),
            StoreState::Damaged { .. } => (
                "Maple's local store needs attention.",
                "You can permanently delete this computer's current Maple local data and create a new store.",
            ),
            StoreState::Refused { reason } => (refusal_title(*reason), reason.message()),
            StoreState::Maintenance => (
                "Updating your local store…",
                "Wait for the local store operation to finish.",
            ),
            StoreState::Open => ("Opening Maple…", "Your local store is ready."),
        };
        body = body
            .child(div().text_xl().child(title))
            .child(div().text_sm().child(description));
        if self.erase {
            body = body
                .child(div().text_sm().child(ERASE_COPY))
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
                        .child("Erase this version's local data"),
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
                        .child("Cancel"),
                );
        } else {
            body = match self.state {
                StoreState::Locked => body
                    .child(password_field("Unlock password", self.password.clone()))
                    .child(
                        widgets::primary_button("store-unlock")
                            .debug_selector(|| "store-unlock".into())
                            .tab_index(2)
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                if password_action_key(event, window, cx) {
                                    this.unlock(cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .when(self.busy, |button| button.opacity(0.5))
                            .on_click(cx.listener(|this, _event, _window, cx| this.unlock(cx)))
                            .child(if self.busy { "Unlocking…" } else { "Unlock" }),
                    )
                    .child(
                        widgets::ghost_button("store-forgot")
                            .debug_selector(|| "store-forgot".into())
                            .tab_index(3)
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                if password_action_key(event, window, cx) {
                                    this.open_erase(cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .on_click(cx.listener(|this, _event, _window, cx| this.open_erase(cx)))
                            .child("Forgot password?"),
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
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                if password_action_key(event, window, cx) {
                                    this.setup_password(cx);
                                    cx.stop_propagation();
                                }
                            }))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.setup_password(cx)
                                    }))
                                    .child("Set an unlock password"),
                            )
                        },
                    )
                    .child(
                        widgets::secondary_button("store-setup-plain")
                            .debug_selector(|| "store-setup-plain".into())
                            .tab_index(2)
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                if password_action_key(event, window, cx) {
                                    this.plain_setup(cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .on_click(cx.listener(|this, _event, _window, cx| this.plain_setup(cx)))
                            .child("Continue without a password"),
                    ),
                StoreState::Damaged { .. } => body
                    .when(accepts_unlock(&self.state), |body| {
                        body.child(div().text_sm().child(
                            "If this store uses a password, enter its current unlock password to verify and repair its generation metadata.",
                        ))
                        .child(password_field("Unlock password", self.password.clone()))
                        .child(
                            widgets::primary_button("store-generation-unlock")
                            .debug_selector(|| "store-generation-unlock".into())
                            .tab_index(2)
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                if password_action_key(event, window, cx) {
                                    this.unlock(cx);
                                    cx.stop_propagation();
                                }
                            }))
                                .when(self.busy, |button| button.opacity(0.5))
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.unlock(cx)
                                }))
                                .child(if self.busy {
                                    "Verifying…"
                                } else {
                                    "Verify and unlock"
                                }),
                        )
                    })
                    .when_some(self.repair.clone(), |body, repair| body.child(repair))
                    .child(widgets::input_frame().id("store-gate-confirmation").debug_selector(|| "store-gate-confirmation".into()).min_h(px(40.)).child(self.confirmation.clone()))
                    .child(div().text_sm().child(
                        "Type START FRESH to permanently delete this computer's current Maple local data and create a new store.",
                    ))
                    .child(
                        widgets::primary_button("store-start-fresh")
                            .debug_selector(|| "store-start-fresh".into())
                            .tab_index(2)
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                if password_action_key(event, window, cx) {
                                    this.reset(true, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .on_click(cx.listener(|this, _event, _window, cx| this.reset(true, cx)))
                            .child("Delete current data and start fresh"),
                    )
                    .child(
                        widgets::ghost_button("store-damaged-erase")
                            .debug_selector(|| "store-damaged-erase".into())
                            .tab_index(3)
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                if password_action_key(event, window, cx) {
                                    this.open_erase(cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .on_click(cx.listener(|this, _event, _window, cx| this.open_erase(cx)))
                            .child("Erase local data…"),
                    ),
                StoreState::Maintenance => body
                    .child(div().text_sm().child(if self.repair_progress.busy {
                        format!(
                            "Local repair: {}. Wait for its verified result.",
                            self.repair_progress.phase
                        )
                    } else {
                        progress_copy(progress_action(self.progress.operation.as_deref()), &self.progress)
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
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                if password_action_key(event, window, cx) {
                                    this.cancel_maintenance(cx);
                                    cx.stop_propagation();
                                }
                            }))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.cancel_maintenance(cx)
                                    }))
                                    .child("Cancel"),
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
                    .child(message.clone()),
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

fn refusal_title(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::NewerVersion | RefusalReason::IncompatibleSchema => {
            "Use a compatible Maple version"
        }
        RefusalReason::UnsupportedFilesystem | RefusalReason::UnsafePath => {
            "Choose a private local data location"
        }
        RefusalReason::UnsupportedOperationFormat => {
            "This local operation needs a compatible Maple version"
        }
        RefusalReason::Unavailable => "Maple could not open local data",
    }
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

pub(crate) const ERASE_COPY: &str = "Maple can't reset your unlock password. Erasing removes the data saved in this version's store: tasks and history, attachments, settings, sign-in, MCP secrets and this store's remote pairings. On your other computers, remove this computer from Remote computers. Backups, snapshots and previous app data remain untouched.";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasswordAction {
    Set,
    Change,
    Remove,
    Setup,
}

#[derive(Clone)]
pub(crate) struct PasswordFormFinished {
    pub notice: Option<String>,
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
    result: Result<Value, String>,
    generated_retry_valid: bool,
}

#[derive(Clone, Copy)]
enum PasswordField {
    Current,
    New,
    Confirmation,
}

pub(crate) struct PasswordForm {
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
    message: Option<String>,
    progress: OperationProgress,
    bridges: RefCell<Vec<gpui::Task<()>>>,
    subscriptions: Vec<Subscription>,
    progress_task: Option<gpui::Task<()>>,
    focus_pending: Option<PasswordField>,
}
impl EventEmitter<PasswordFormFinished> for PasswordForm {}

impl PasswordForm {
    pub(crate) fn show_local_store_notice(&mut self, message: String, cx: &mut Context<Self>) {
        self.message = Some(message);
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
        let current =
            cx.new(|cx| SecretInput::new("Current unlock password", cx).with_tab_index(1));
        let password =
            cx.new(|cx| SecretInput::new("Enter a new unlock password", cx).with_tab_index(2));
        let confirmation = cx
            .new(|cx| SecretInput::new("Re-enter your new unlock password", cx).with_tab_index(3));
        let mut this = Self {
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

    fn close(&mut self, cx: &mut Context<Self>) {
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
        future: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    ) {
        let bridge = super::task::call(&self.backend, future, cx, then);
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
                    .local_control::<SuggestedPhrase>("generate_store_passphrase", ())
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
                    Err(message) => this.message = Some(message),
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
    fn validation_message(&self, cx: &Context<Self>) -> Option<&'static str> {
        if self.busy {
            return Some("Wait for the current change to finish.");
        }
        if matches!(self.action, PasswordAction::Change | PasswordAction::Remove)
            && self.current.read(cx).is_empty()
        {
            return Some("Enter your current unlock password to continue.");
        }
        if self.action == PasswordAction::Remove {
            return None;
        }
        if self.suggested && self.suggestion_retry_blocked {
            return Some(
                "Keep this passphrase. Maple could not confirm the previous attempt. Reopen Maple to check the store before trying again.",
            );
        }
        if self.suggested && self.generating {
            return Some("Wait for your passphrase to finish generating.");
        }
        if self.suggested && self.phrase.is_none() {
            return Some("Generate a passphrase or enter your own password.");
        }
        if !self.suggested && self.password.read(cx).is_empty() {
            return Some("Enter a new unlock password.");
        }
        let Some(assessment) = self.assessment else {
            return Some("Use between 15 and 1,024 characters for your new password.");
        };
        if assessment.reason == keys::StrengthReason::Length {
            return Some("Use between 15 and 1,024 characters for your new password.");
        }
        if !assessment.allowed() {
            return Some("Choose a less predictable password. Try a longer, unique phrase.");
        }
        if self.confirmation.read(cx).is_empty() {
            return Some("Re-enter your new unlock password in Confirm password.");
        }
        if !self.confirmation_matches {
            return Some(
                "The passwords don't match. Re-enter your new password in Confirm password.",
            );
        }
        None
    }
    fn can_submit(&self, cx: &Context<Self>) -> bool {
        self.validation_message(cx).is_none()
    }
    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.can_submit(cx) {
            if !self.busy {
                self.message = self.validation_message(cx).map(str::to_owned);
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
                let result = async {
                    if action != PasswordAction::Setup {
                        crate::settings::flush_pending().await?;
                        backend.flush_ui_settings().await?;
                    }
                    match action {
                        PasswordAction::Set => {
                            backend
                                .local_control::<Value>(
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
                                .local_control::<Value>(
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
                                .local_control::<Value>(
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
                                .local_control::<Value>(
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
                        .local_control::<PasswordAssessment>(
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
                    result,
                    generated_retry_valid,
                })
            },
            cx,
            |this, result, cx| this.submission_completed(result, cx),
        );
    }

    fn submission_completed(
        &mut self,
        attempt: Result<PasswordAttempt, String>,
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
                    format!(
                        "{message} Keep this passphrase. Maple could not confirm the previous attempt. Reopen Maple to check the store before trying again."
                    )
                } else {
                    message
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
                            .local_control::<OperationProgress>("store_progress", ())
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
                    .local_control::<bool>("cancel_store_operation", ())
                    .await
            },
            cx,
            |this, result, cx| {
                this.message = Some(match result {
                    Ok(true) => "Cancelling before the store is replaced…".into(),
                    Ok(false) => "The store is committing. Wait for it to finish.".into(),
                    Err(message) => message,
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
                "Enable encryption",
                "Protect saved conversation text, settings and sign-in. Attached files and images remain unencrypted on this computer.",
                "Enable encryption",
            ),
            PasswordAction::Change => (
                "Change unlock password",
                "Enter your current unlock password, then choose a new one.",
                "Save password",
            ),
            PasswordAction::Remove => (
                "Remove encryption",
                "Your local Maple data will no longer be encrypted. Enter your current password to continue.",
                "Remove encryption",
            ),
        };
        let mut pane = widgets::card_row()
            .w_full()
            .max_w(px(560.))
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
            pane = pane.child(
                div()
                    .text_sm()
                    .child(progress_copy(Some(self.action), &self.progress)),
            );
            if self.progress.cancellable {
                pane = pane.child(
                    widgets::secondary_button("store-operation-cancel")
                        .tab_index(1)
                        .on_click(cx.listener(|this, _event, _window, cx| this.cancel(cx)))
                        .child("Cancel"),
                );
            }
        } else {
            if matches!(self.action, PasswordAction::Change | PasswordAction::Remove) {
                pane = pane.child(password_field(
                    "Current unlock password",
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
                                .child("New unlock password"),
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
                                    .child("Generate passphrase"),
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
                            "Generating a random passphrase…"
                        } else {
                            "Couldn't generate a passphrase. Try again or enter your own password."
                        }));
                    }
                    suggestion = suggestion
                        .child(div().text_xs().text_color(gpui::rgb(theme::text_secondary()))
                            .child("Six random words. Save them in your password manager before continuing."))
                        .child(div().flex().flex_wrap().gap_2()
                            .child(widgets::secondary_button("store-copy-phrase")
                                .tab_index(7)
                                .on_click(cx.listener(|this, _event, _window, cx| this.copy_phrase(cx)))
                                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) { this.copy_phrase(cx); cx.stop_propagation(); }
                                }))
                                .child("Copy passphrase"))
                            .child(widgets::ghost_button("store-another-phrase")
                                .tab_index(8)
                                .on_click(cx.listener(|this, _event, _window, cx| this.generate(cx)))
                                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) { this.generate(cx); cx.stop_propagation(); }
                                }))
                                .child("Generate another"))
                            .child(widgets::ghost_button("store-use-typed")
                                .tab_index(9)
                                .on_click(cx.listener(|this, _event, _window, cx| this.use_typed(cx)))
                                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                    if password_action_key(event, window, cx) { this.use_typed(cx); cx.stop_propagation(); }
                                }))
                                .child("Use my own password")));
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
                                    "Use at least 15 characters. A long, unique phrase works well.",
                                ),
                        );
                }
                if let Some(assessment) = self.assessment {
                    let (label, color) = meter_presentation(Some(assessment.level));
                    new_password =
                        new_password.child(div().text_sm().text_color(gpui::rgb(color)).child(
                            if assessment.reason == keys::StrengthReason::Length {
                                "Use between 15 and 1,024 characters.".to_owned()
                            } else {
                                format!("Password strength: {label}")
                            },
                        ));
                }
                pane = pane.child(new_password).child(password_field(
                    "Confirm password",
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
                                "Passwords match"
                            } else {
                                "Passwords don't match"
                            }),
                    );
                }
                if self.action == PasswordAction::Change {
                    pane = pane.child(div().flex().flex_col().gap_1()
                        .child(widgets::checkbox("store-reencrypt", self.reencrypt, "Also create a new encryption key")
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
                            .child("Use a new encryption key if your old password and a copy of this store may have been exposed. This protects this store and future writes; it cannot revoke existing copies.")));
                }
                pane = pane.child(div().flex().flex_col().gap_1().text_xs()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child("This is separate from your Maple account password. Maple can't reset it.")
                    .child("If you forget it, you'll need to erase this version's local data and pair your other computers again.")
                    .when(matches!(self.action, PasswordAction::Set | PasswordAction::Setup), |note| note.child(
                        "Existing backups, snapshots and previous app data aren't re-encrypted.",
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
                            .child("Cancel"),
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
                            .child(submit_label),
                    ),
            );
        }
        pane
    }
}

fn password_field(label: &'static str, input: Entity<SecretInput>) -> gpui::Div {
    let selector = match label {
        "Current unlock password" => "password-current-frame",
        "Confirm password" => "password-confirmation-frame",
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
                .child(label),
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

pub(crate) fn operation_notice(result: &Value) -> Option<String> {
    let flag = |name| result.get(name).and_then(Value::as_bool).unwrap_or(false);
    if flag("synchronization_pending") || flag("sync_pending") {
        Some("The new unlock password took effect, but Maple couldn't confirm its key file was synchronized. Keep both passwords until synchronization finishes.".into())
    } else if flag("backup_pending") {
        Some("The new unlock password took effect. Maple still needs to synchronize its backup key file.".into())
    } else if flag("used_previous_password") {
        Some("Maple opened with your previous unlock password. Keep your current and previous unlock passwords until the key files are synchronized.".into())
    } else if flag("reconciliation_pending") {
        Some("Maple is open. Key-file synchronization or its security upgrade still needs attention. Your existing unlock password remains valid.".into())
    } else if flag("notice_pending") {
        Some("Encryption changed. Maple still needs to save the local mode-change notice.".into())
    } else {
        None
    }
}

fn meter_presentation(level: Option<StrengthLevel>) -> (&'static str, u32) {
    match level {
        Some(StrengthLevel::TooEasy) => ("Too easy to guess", theme::status_error()),
        Some(StrengthLevel::CouldBeStronger) => ("Could be stronger", theme::status_warning()),
        Some(StrengthLevel::Strong) => ("Strong", theme::status_success()),
        None => ("Enter a password", theme::text_muted()),
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

fn progress_copy(action: Option<PasswordAction>, progress: &OperationProgress) -> String {
    let verb = match action {
        Some(PasswordAction::Remove) => "Removing encryption",
        Some(PasswordAction::Change) => "Updating your unlock password",
        Some(PasswordAction::Set | PasswordAction::Setup) => "Encrypting your local data",
        None => "Updating your local store",
    };
    let exporting = progress.busy && progress.phase == "exporting";
    let estimate = progress
        .remaining_ms
        .filter(|ms| exporting && *ms > 0)
        .map(|ms| {
            let minutes = (ms.saturating_add(59_999) / 60_000).max(1);
            format!(
                " About {minutes} {} left.",
                if minutes == 1 { "minute" } else { "minutes" }
            )
        })
        .unwrap_or_default();
    let bytes = if exporting && progress.total_bytes > 0 {
        format!(
            " Copied about {} of {}.",
            readable_bytes(progress.completed_bytes),
            readable_bytes(progress.total_bytes)
        )
    } else {
        String::new()
    };
    let phase = if progress.busy && progress.phase == "committing" {
        " Finishing the change. Keep Maple open."
    } else {
        ""
    };
    format!("{verb}.{estimate}{bytes}{phase}")
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
            gate.show_local_store_notice("Local storage needs attention.".into(), cx);
            assert!(gate.message.is_none());
            assert_eq!(
                form.read(cx).message.as_deref(),
                Some("Local storage needs attention.")
            );
            gate.form = None;
            gate.show_local_store_notice("Retry unlocking.".into(), cx);
            assert_eq!(gate.message.as_deref(), Some("Retry unlocking."));
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
    fn committed_notice_does_not_claim_rollback() {
        let notice = operation_notice(
            &serde_json::json!({"synchronization_pending":true,"backup_pending":true}),
        )
        .unwrap();
        assert!(notice.contains("took effect"));
        assert!(notice.contains("both passwords"));
        assert!(!notice.contains("rolled back"));
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
        let text = progress_copy(Some(PasswordAction::Remove), &progress);
        assert!(text.starts_with("Removing encryption."));
        assert!(text.contains("1.0 MiB of 2.0 MiB"));
        assert!(text.contains("1 minute left"));
        assert!(!text.contains("1 minutes"));
        progress.remaining_ms = Some(60_001);
        assert!(progress_copy(None, &progress).contains("2 minutes left"));
        progress.remaining_ms = Some(0);
        assert!(!progress_copy(None, &progress).contains("left"));
        for phase in ["verifying", "committing", "complete"] {
            progress.phase = phase.into();
            let text = progress_copy(None, &progress);
            assert!(!text.contains("left"));
            assert!(!text.contains("Copied"));
            assert!(!text.contains("committed"));
        }
        assert!(progress_copy(None, &progress).starts_with("Updating your local store."));
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
        assert!(notice.contains("existing unlock password remains valid"));
        assert!(!notice.contains("previous unlock password"));
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
                    .contains("Type START FRESH exactly")
            );
        });
    }
}

#[cfg(test)]
mod password_form_tests;
