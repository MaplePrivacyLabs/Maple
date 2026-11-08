//! Local advanced preferences and capability-gated encryption controls. A
//! remote task selection never redirects these settings to another computer.

use crate::ui::i18n::{MessageCache, UiText, tr, tr_args};
use gpui::{App, Context, Div, Entity, EventEmitter, Focusable, div, prelude::*, px};

use crate::ui::icons::icon;
use maple_agent::store::{
    StoreState,
    lifecycle::{StoreMode, StoreModeNotice},
};
use serde_json::Value;

use super::{Section, SettingsScreen, TextInput, subsection_title, theme, widgets};
use crate::ui::store_gate::{
    ConfirmationRequest, ERASE_COPY, PasswordAction, PasswordForm, PasswordFormFinished,
    RetryStorePersistence, StoreStatus,
};

pub(super) struct AdvancedState {
    pub status: Option<StoreStatus>,
    pub form: Option<Entity<PasswordForm>>,
    pub confirmation: Entity<TextInput>,
    pub erase: bool,
    /// Focus the erase dialog's field the next time it draws.
    pub erase_focus_pending: bool,
    pub busy: bool,
    pub notice: Option<UiText>,
    success: Option<&'static str>,
    mode_notice_acknowledging: Option<StoreModeNotice>,
    mode_notice_acknowledged: Option<StoreModeNotice>,
    pub remote_enabled: bool,
}

impl AdvancedState {
    pub fn new(
        application_vim_enabled: bool,
        application_focus: gpui::FocusHandle,
        cx: &mut Context<SettingsScreen>,
    ) -> Self {
        Self {
            status: None,
            form: None,
            confirmation: {
                let screen = cx.entity().downgrade();
                cx.new(move |cx| {
                    TextInput::localized("settings-advanced-type-erase", cx)
                        .application_vim(application_vim_enabled)
                        .on_application_escape(move |window, cx| {
                            window.focus(&application_focus, cx)
                        })
                        // Enter submits once the word is right; a wrong word
                        // shows the notice, as the button does.
                        .on_enter(move |_text, _window, cx| {
                            let _ = screen.update(cx, |screen, cx| screen.erase_local_store(cx));
                        })
                })
            },
            erase: false,
            erase_focus_pending: false,
            busy: false,
            notice: None,
            success: None,
            mode_notice_acknowledging: None,
            mode_notice_acknowledged: None,
            remote_enabled: false,
        }
    }
    pub fn available(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|status| status.password_settings_available)
    }
}

impl SettingsScreen {
    pub(crate) fn record_mode_notice_ack(
        &mut self,
        notice: &StoreModeNotice,
        cx: &mut Context<Self>,
    ) {
        self.advanced.mode_notice_acknowledged = Some(notice.clone());
        if let Some(status) = &mut self.advanced.status {
            apply_mode_notice_ack(status, notice, true);
        }
        cx.notify();
    }

    /// Store feedback stays beside the control that initiated the operation.
    /// Other Settings sections are not a recipient for encryption dialogs.
    pub(crate) fn show_local_store_notice(
        &mut self,
        message: impl Into<UiText>,
        cx: &mut Context<Self>,
    ) -> bool {
        let message = message.into();
        if self.section != Section::Account {
            return false;
        }
        if let Some(form) = &self.advanced.form {
            form.update(cx, |form, cx| form.show_local_store_notice(message, cx));
        } else {
            self.advanced.notice = Some(message);
            cx.notify();
        }
        true
    }

    pub(crate) fn password_operation_busy(&self, cx: &App) -> bool {
        self.advanced
            .form
            .as_ref()
            .is_some_and(|form| form.read(cx).is_busy())
    }

    pub(crate) fn pending_password_operation_id<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        self.advanced
            .form
            .as_ref()
            .and_then(|form| form.read(cx).pending_operation_id())
    }

    #[cfg(test)]
    pub(crate) fn set_password_form_for_test(&mut self, form: Option<Entity<PasswordForm>>) {
        self.advanced.form = form;
    }

    /// Release builds have no password controls, so this section has nothing
    /// to show unless the local store reports a notice.
    pub(super) fn advanced_has_content(&self) -> bool {
        self.advanced.available()
            || self.advanced.notice.is_some()
            || self.advanced.status.as_ref().is_some_and(|status| {
                status.attention_notice().is_some()
                    || status.current_mode_notice().is_some()
                    || status.current_persistence_failure().is_some()
            })
    }

    pub(crate) fn refresh_encryption_status(&self, cx: &mut Context<Self>) {
        self.load_advanced_status(cx);
    }

    pub(super) fn load_advanced_status(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        self.call_presented(
            async move {
                backend
                    .local_control_presented::<StoreStatus>("store_status", ())
                    .await
            },
            cx,
            |this, result, cx| {
                match result {
                    Ok(mut status) => {
                        filter_acknowledged_mode_notice(
                            &mut status,
                            this.advanced.mode_notice_acknowledged.as_ref(),
                        );
                        this.advanced.status = Some(status);
                    }
                    Err(message) => this.advanced.notice = Some(message.into()),
                }
                cx.notify();
            },
        );
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .remote_setting::<crate::remote::manager::Status>("remote_status", ())
                    .await
            },
            cx,
            |this, result, cx| {
                if let Ok(status) = result {
                    this.advanced.remote_enabled = status.enabled;
                }
                cx.notify();
            },
        );
    }

    pub(super) fn open_password_form(&mut self, action: PasswordAction, cx: &mut Context<Self>) {
        if !self.advanced.available()
            || self.advanced.busy
            || !self
                .advanced
                .status
                .as_ref()
                .is_some_and(|status| status.state == StoreState::Open)
        {
            return;
        }
        let backend = self.backend.clone();
        let mut context = vec![self.user_id.clone()];
        if let Some(email) = self
            .account
            .info
            .as_ref()
            .and_then(|account| account.email.as_ref())
        {
            context.push(email.clone());
        }
        self.advanced_subscriptions.clear();
        let form = cx.new(|cx| PasswordForm::new(backend, action, context, cx));
        self.advanced_subscriptions.push(cx.subscribe(
            &form,
            move |this, _form, event: &PasswordFormFinished, cx| {
                this.advanced.form = None;
                this.application_focus_pending = true;
                this.reconcile_application_vim_target();
                if event.cancelled {
                    cx.notify();
                    return;
                }
                this.advanced.success = Some(password_success_message(action));
                this.advanced.notice = event.notice.clone();
                this.load_advanced_status(cx);
                // Desktop may acknowledge this exact receipt only after this
                // live Settings recipient has installed its inline result.
                cx.emit(event.clone());
                cx.notify();
            },
        ));
        self.advanced.form = Some(form);
        self.advanced.notice = None;
        self.advanced.success = None;
        cx.notify();
    }

    /// Escape or the backdrop on the password dialog. A running operation
    /// keeps the dialog: its own Cancel knows whether it can stop.
    pub(super) fn dismiss_password_form(&mut self, cx: &mut Context<Self>) {
        // The form's own close: a no-op while busy, otherwise it emits the
        // cancelled finish that the subscription turns into `form = None`.
        if let Some(form) = self.advanced.form.clone() {
            form.update(cx, |form, cx| form.close(cx));
        }
    }

    /// Set / change / remove the unlock password, as a dialog. The form is
    /// its own card; this adds the scrim and keeps the encryption context
    /// lines under it.
    pub(super) fn render_password_dialog(
        &self,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let form = self.advanced.form.clone()?;
        let title = form.read(cx).title();
        let width = (window.viewport_size().width - px(48.)).clamp(px(0.), px(560.));
        let max_height = (window.viewport_size().height - px(96.)).max(px(240.));
        // The notes sit on their own surface: the scrim is no background for
        // body text in either theme.
        let notes = self.encryption_notes(true);
        let body = div()
            .id("advanced-password-body")
            .flex()
            .flex_col()
            .gap_3()
            .w(width)
            .max_h(max_height)
            .overflow_y_scroll()
            .child(form)
            .when(!notes.is_empty(), |body| {
                body.child(
                    widgets::card_row()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .text_color(gpui::rgb(theme::text_secondary()))
                        .children(notes),
                )
            });
        Some(
            div()
                .id("advanced-password-backdrop")
                .absolute()
                .inset_0()
                .size_full()
                .occlude()
                .bg(theme::scrim())
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| this.dismiss_password_form(cx)),
                )
                .child(
                    div()
                        .id("advanced-password-dialog")
                        .debug_selector(|| "advanced-password-dialog".to_string())
                        .role(gpui::Role::Dialog)
                        .aria_label(title)
                        .key_context("Dialog")
                        .rounded(theme::RADIUS_XL)
                        .shadow_lg()
                        .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                            cx.stop_propagation();
                        })
                        .on_key_down(cx.listener(
                            |this, event: &gpui::KeyDownEvent, _window, cx| {
                                if event.keystroke.key == "escape"
                                    && !event.keystroke.modifiers.modified()
                                {
                                    cx.stop_propagation();
                                    this.dismiss_password_form(cx);
                                }
                            },
                        ))
                        .child(body),
                ),
        )
    }

    pub(super) fn open_erase_dialog(&mut self, cx: &mut Context<Self>) {
        self.advanced.erase = true;
        self.advanced.erase_focus_pending = true;
        self.advanced.notice = None;
        self.advanced.success = None;
        cx.notify();
    }

    /// Cancel, Escape, or the backdrop. Nothing is lost by closing: the
    /// typed word is the only state.
    pub(super) fn close_erase_dialog(&mut self, cx: &mut Context<Self>) {
        if self.advanced.busy {
            return;
        }
        self.advanced.erase = false;
        self.advanced.notice = None;
        self.advanced
            .confirmation
            .update(cx, |input, cx| input.clear(cx));
        self.application_focus_pending = true;
        self.reconcile_application_vim_target();
        cx.notify();
    }

    pub(super) fn focus_erase_dialog(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.advanced.erase && std::mem::take(&mut self.advanced.erase_focus_pending) {
            window.focus(&self.advanced.confirmation.read(cx).focus_handle(cx), cx);
        }
    }

    /// Erasing this version's local data, as a dialog: the consequences,
    /// the typed word, Cancel, and a destructive button inert until the
    /// word is right.
    pub(super) fn render_erase_dialog(
        &self,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        if !self.advanced.erase {
            return None;
        }
        let busy = self.advanced.busy;
        let ready = self.advanced.confirmation.read(cx).text() == "ERASE";
        let title = tr("settings-advanced-erase-dialog-title");
        let width = (window.viewport_size().width - px(48.)).clamp(px(0.), px(520.));
        Some(
            div()
                .id("advanced-erase-backdrop")
                .absolute()
                .inset_0()
                .size_full()
                .occlude()
                .bg(theme::scrim())
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| this.close_erase_dialog(cx)),
                )
                .child(
                    div()
                        .id("advanced-erase-dialog")
                        .debug_selector(|| "advanced-erase-dialog".to_string())
                        .role(gpui::Role::Dialog)
                        .aria_label(title.clone())
                        .key_context("Dialog")
                        .w(width)
                        .p_5()
                        .rounded(theme::RADIUS_XL)
                        .shadow_lg()
                        .bg(gpui::rgb(theme::bg_elevated()))
                        .border_1()
                        .border_color(gpui::rgb(theme::border()))
                        .flex()
                        .flex_col()
                        .gap_4()
                        .text_color(gpui::rgb(theme::text_primary()))
                        .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                            cx.stop_propagation();
                        })
                        .on_key_down(cx.listener(
                            |this, event: &gpui::KeyDownEvent, _window, cx| {
                                if event.keystroke.key == "escape"
                                    && !event.keystroke.modifiers.modified()
                                {
                                    cx.stop_propagation();
                                    this.close_erase_dialog(cx);
                                }
                            },
                        ))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(icon("circle-alert", px(18.), theme::status_error()))
                                .child(
                                    div()
                                        .text_lg()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(title),
                                ),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(gpui::rgb(theme::text_secondary()))
                                .child(gpui::text!(tr(ERASE_COPY))),
                        )
                        .child(
                            widgets::input_frame()
                                .text_sm()
                                .child(self.advanced.confirmation.clone()),
                        )
                        .when_some(self.advanced.notice.clone(), |dialog, notice| {
                            dialog.child(
                                div()
                                    .text_sm()
                                    .text_color(gpui::rgb(theme::status_error()))
                                    .child(notice),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap_2()
                                .child(
                                    widgets::ghost_button("advanced-erase-cancel")
                                        .py_1p5()
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.close_erase_dialog(cx);
                                        }))
                                        .child(tr("settings-advanced-cancel")),
                                )
                                .child(
                                    widgets::danger_button("advanced-erase-confirm")
                                        .py_1p5()
                                        .when(busy || !ready, |el| el.opacity(0.5))
                                        .when(!busy && ready, |el| {
                                            el.on_click(cx.listener(|this, _event, _window, cx| {
                                                this.erase_local_store(cx)
                                            }))
                                        })
                                        .child(tr(
                                            "settings-advanced-erase-this-version-s-local-data",
                                        )),
                                ),
                        ),
                ),
        )
    }

    pub(super) fn erase_local_store(&mut self, cx: &mut Context<Self>) {
        if self.advanced.busy || !self.advanced.available() {
            return;
        }
        let confirmation = self.advanced.confirmation.read(cx).text();
        if confirmation != "ERASE" {
            self.advanced.notice = Some(UiText::key(
                "settings-advanced-type-erase-exactly-to-continue",
            ));
            cx.notify();
            return;
        }
        // Erase intentionally discards pending edits. A new generation must
        // never receive an old generation's delayed settings or dictionary.
        crate::settings::discard_decrypted_cache();
        crate::ui::spell::clear_user_words();
        self.advanced.busy = true;
        self.advanced.notice = None;
        self.advanced.success = None;
        let backend = self.backend.clone();
        self.call_presented(
            async move {
                backend
                    .local_control_presented::<Value>(
                        "erase_local_data",
                        ConfirmationRequest {
                            confirmation: &confirmation,
                        },
                    )
                    .await
            },
            cx,
            |this, result, cx| {
                this.advanced.busy = false;
                match result {
                    Ok(_) => {
                        this.advanced.erase = false;
                        this.advanced.form = None;
                    }
                    Err(message) => this.advanced.notice = Some(message.into()),
                }
                this.load_advanced_status(cx);
                cx.notify();
            },
        );
    }

    fn dismiss_local_mode_notice(&mut self, cx: &mut Context<Self>) {
        if self.advanced.mode_notice_acknowledging.is_some() {
            return;
        }
        let Some(notice) = self
            .advanced
            .status
            .as_ref()
            .and_then(StoreStatus::current_mode_notice)
            .cloned()
        else {
            return;
        };
        self.advanced.mode_notice_acknowledging = Some(notice.clone());
        let backend = self.backend.clone();
        let requested = notice.clone();
        self.call_presented(
            async move {
                backend
                    .local_control_presented::<bool>(
                        "acknowledge_mode_change_notice",
                        (&requested.store_id, &requested.notice_id),
                    )
                    .await
            },
            cx,
            move |this, result, cx| {
                if this.advanced.mode_notice_acknowledging.as_ref() == Some(&notice) {
                    this.advanced.mode_notice_acknowledging = None;
                }
                // A replacement generation or newer notice must never be
                // cleared (or receive an old request's error) by this reply.
                let acknowledged = matches!(result, Ok(true));
                if acknowledged {
                    this.advanced.mode_notice_acknowledged = Some(notice.clone());
                }
                if this.backend.store_state() == StoreState::Open
                    && let Some(status) = &mut this.advanced.status
                    && apply_mode_notice_ack(status, &notice, acknowledged)
                    && !acknowledged
                {
                    this.advanced.notice = Some(UiText::key(
                        "settings-advanced-this-notice-could-not-be-dismissed-try-again",
                    ));
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn render_local_store_notices(&self, mut pane: Div, cx: &mut Context<Self>) -> Div {
        let status = self.advanced.status.as_ref();
        let form_open = self.advanced.form.is_some();
        let attention = status
            .filter(|_| !form_open)
            .and_then(StoreStatus::attention_notice);
        let mode_notice = status
            .filter(|_| !form_open)
            .and_then(StoreStatus::current_mode_notice);
        let persistence = status.and_then(StoreStatus::current_persistence_failure);
        if let Some(failure) = persistence {
            pane = pane.child(
                local_store_notice_card()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(tr("settings-advanced-changes-need-saving")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .child(crate::ui::host_errors::persistence_text(failure).resolve()),
                    )
                    .child(
                        widgets::secondary_button("advanced-retry-store-persistence")
                            .on_click(cx.listener(|_this, _, _, cx| cx.emit(RetryStorePersistence)))
                            .child(tr("settings-advanced-retry-saving")),
                    ),
            );
        }
        if let Some(notice) = attention
            && self
                .advanced
                .notice
                .as_ref()
                .map(UiText::resolve)
                .as_deref()
                != Some(notice.presentation().resolve().as_ref())
            && persistence.is_none_or(|failure| failure.message() != notice.message.as_str())
        {
            pane = pane.child(
                local_store_notice_card()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(tr("settings-advanced-local-storage-needs-attention")),
                    )
                    .child(div().text_sm().child(notice.presentation().resolve())),
            );
        }
        if let Some(notice) = mode_notice {
            let acknowledging = self.advanced.mode_notice_acknowledging.as_ref() == Some(notice);
            pane = pane.child(
                local_store_notice_card()
                    .child(
                        div()
                            .text_sm()
                            .child(mode_notice_text(notice, &self.messages)),
                    )
                    .child(
                        widgets::ghost_button("advanced-dismiss-mode-notice")
                            .when(acknowledging, |button| button.opacity(0.5))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.dismiss_local_mode_notice(cx)),
                            )
                            .child(if acknowledging {
                                tr("settings-advanced-dismissing")
                            } else {
                                tr("settings-advanced-dismiss")
                            }),
                    ),
            );
        }
        if !form_open
            && let Some(notice) = &self.advanced.notice
            && mode_notice
                .is_none_or(|mode| mode_notice_text(mode, &self.messages) != notice.resolve())
            && persistence.is_none_or(|failure| failure.message() != notice.resolve().as_ref())
        {
            pane = pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::status_warning()))
                    .child(notice.clone()),
            );
        }
        pane
    }

    pub(super) fn render_advanced(&self, cx: &mut Context<Self>) -> Div {
        let mut pane = div().flex().flex_col().gap_3();
        if !self.advanced.available() {
            pane = self.render_local_store_notices(
                pane.child(subsection_title(&tr("settings-advanced-local-data"))),
                cx,
            );
            if !self.advanced_has_content() {
                pane = pane.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_secondary()))
                        .child(tr("settings-advanced-no-attention")),
                );
            }
            return pane;
        }
        pane = pane.child(subsection_title(&tr(
            "settings-advanced-encryption-on-this-computer",
        )));
        let mut card = widgets::card_row().flex().flex_col().gap_3();
        if let Some(success) = self.advanced.success {
            card = card.child(
                div()
                    .rounded_md()
                    .p_3()
                    .bg(gpui::rgb(theme::bg_sidebar()))
                    .text_color(gpui::rgb(theme::status_success()))
                    .child(tr(success)),
            );
        }
        let password = self
            .advanced
            .status
            .as_ref()
            .is_some_and(|status| status.mode == StoreMode::Password);
        card = card.child(
            div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(gpui::rgb(if password {
                    theme::status_success()
                } else {
                    theme::text_primary()
                }))
                .child(if password {
                    tr("settings-advanced-encryption-is-on")
                } else {
                    tr("settings-advanced-encryption-is-off")
                }),
        );
        card = card.child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(if password {
                    tr("settings-advanced-maple-s-copy-of-your-conversation-text-settings-and-sign-in-on-th")
                } else {
                    tr("settings-advanced-your-conversation-text-settings-and-sign-in-are-stored-on-this-co")
                }),
        );
        {
            card = card.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        widgets::default_button("advanced-password")
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.open_password_form(
                                    if password {
                                        PasswordAction::Change
                                    } else {
                                        PasswordAction::Set
                                    },
                                    cx,
                                )
                            }))
                            .child(if password {
                                tr("settings-advanced-change-password")
                            } else {
                                tr("settings-advanced-set-unlock-password")
                            }),
                    )
                    .when(password, |row| {
                        row.child(
                            widgets::secondary_button("advanced-remove-password")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.open_password_form(PasswordAction::Remove, cx)
                                }))
                                .child(tr("settings-advanced-remove-password")),
                        )
                    })
                    .child(
                        widgets::danger_outline_button("advanced-erase")
                            .debug_selector(|| "advanced-erase".to_string())
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.open_erase_dialog(cx);
                            }))
                            .child(icon(
                                "circle-alert",
                                widgets::ROW_ICON,
                                theme::status_error(),
                            ))
                            .child(tr("settings-advanced-erase-local-data")),
                    ),
            );
        }
        let card = self.encryption_context(card, password);
        self.render_local_store_notices(pane.child(card), cx)
    }

    fn encryption_context(&self, pane: Div, password: bool) -> Div {
        pane.children(self.encryption_notes(password))
    }

    /// The warnings that belong next to any encryption control: remote
    /// access while locked, and the coding agents that keep their own copy.
    fn encryption_notes(&self, password: bool) -> Vec<Div> {
        let mut notes = Vec::new();
        if self.advanced.remote_enabled {
            notes.push(div().text_sm().child(tr("settings-advanced-while-maple-is-locked-your-other-computers-can-t-reach-this-one-a")));
        }
        if password {
            for integration in self
                .integrations
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|integration| integration.enabled_for_new_tasks)
            {
                let copy = match integration.id.as_str() {
                    "claude" => Some(tr(
                        "settings-advanced-claude-code-keeps-its-own-copy-of-the-work-maple-hands-it-in-its",
                    )),
                    "codex" => Some(tr(
                        "settings-advanced-codex-keeps-its-own-copy-of-the-work-maple-hands-it-in-its-own-fo",
                    )),
                    _ => None,
                };
                if let Some(copy) = copy {
                    notes.push(div().text_sm().child(copy));
                }
            }
        }
        notes
    }
}

impl EventEmitter<PasswordFormFinished> for SettingsScreen {}
impl EventEmitter<RetryStorePersistence> for SettingsScreen {}

fn local_store_notice_card() -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .p_3()
        .bg(gpui::rgb(theme::bg_elevated()))
        .text_color(gpui::rgb(theme::status_warning()))
}

/// Only presentation uses the additive metadata; acknowledgement retains the
/// original store and notice identities, including for legacy hosts.
fn mode_notice_text(notice: &StoreModeNotice, messages: &MessageCache) -> gpui::SharedString {
    let key = match notice.kind.as_deref() {
        Some("password_set") => "flows-mode-notice-set",
        Some("password_changed") => "flows-mode-notice-changed",
        Some("password_rotated") => "flows-mode-notice-rotated",
        Some("password_removed") => "flows-mode-notice-removed",
        _ => return notice.message.clone().into(),
    };
    let Some(at_ms) = notice.at_ms else {
        return notice.message.clone().into();
    };
    let Some(date) = chrono::DateTime::from_timestamp_millis(at_ms) else {
        return notice.message.clone().into();
    };
    messages.computed(key, &at_ms.to_string(), || {
        // Preserve the source record's UTC time and minute precision.
        tr_args(
            key,
            [("date", date.format("%Y-%m-%d %H:%M UTC").to_string().into())],
        )
    })
}

/// Return whether this reply still belongs to the visible authenticated notice.
/// Failed acknowledgements leave it intact so the user can retry.
fn apply_mode_notice_ack(
    status: &mut StoreStatus,
    expected: &StoreModeNotice,
    acknowledged: bool,
) -> bool {
    if status.current_mode_notice() != Some(expected) {
        return false;
    }
    if acknowledged {
        status.mode_change_notice = None;
    }
    true
}

fn filter_acknowledged_mode_notice(
    status: &mut StoreStatus,
    acknowledged: Option<&StoreModeNotice>,
) {
    if acknowledged.is_some() && status.mode_change_notice.as_ref() == acknowledged {
        // A read begun before the exact ACK can arrive afterwards. Its stale
        // receipt cannot resurrect a notice the host already acknowledged.
        status.mode_change_notice = None;
    }
}

fn password_success_message(action: PasswordAction) -> &'static str {
    match action {
        PasswordAction::Set | PasswordAction::Setup => {
            "settings-advanced-encryption-enabled-on-this-computer"
        }
        PasswordAction::Change => "settings-advanced-your-unlock-password-was-changed",
        PasswordAction::Remove => "settings-advanced-your-unlock-password-was-removed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_mode_notice_switches_language_without_changing_acknowledgement() {
        let _locale = crate::ui::i18n::TestLocalizationGuard::enable();
        let messages = MessageCache::default();
        for kind in [
            "password_set",
            "password_changed",
            "password_rotated",
            "password_removed",
        ] {
            let notice: StoreModeNotice = serde_json::from_value(serde_json::json!({
                "store_id": "current", "notice_id": "exact", "message": "Legacy fallback",
                "kind": kind, "at_ms": 0
            }))
            .unwrap();
            let original = notice.clone();
            crate::ui::i18n::apply_preference(Some("en"));
            let english = mode_notice_text(&notice, &messages);
            assert!(english.contains("1970-01-01 00:00 UTC"));
            assert!(english.contains("on this computer"));
            crate::ui::i18n::apply_preference(Some("es"));
            let spanish = mode_notice_text(&notice, &messages);
            assert!(spanish.contains("1970-01-01 00:00 UTC"));
            assert!(spanish.contains("en esta computadora"));
            assert_ne!(english, spanish);
            crate::ui::i18n::apply_preference(Some("en"));
            assert_eq!(mode_notice_text(&notice, &messages), english);
            assert_eq!(notice, original);
        }
    }

    #[test]
    fn retained_mode_notice_preserves_legacy_unknown_and_invalid_metadata() {
        let _locale = crate::ui::i18n::TestLocalizationGuard::enable();
        let messages = MessageCache::default();
        for metadata in [
            serde_json::json!({}),
            serde_json::json!({"kind":"future_kind", "at_ms":0}),
            serde_json::json!({"kind":"password_set"}),
            serde_json::json!({"kind":"password_set", "at_ms":i64::MAX}),
        ] {
            let mut value = serde_json::json!({"store_id":"current", "notice_id":"exact", "message":"Original host message"});
            value
                .as_object_mut()
                .unwrap()
                .extend(metadata.as_object().unwrap().clone());
            let notice: StoreModeNotice = serde_json::from_value(value).unwrap();
            for locale in ["en", "es", "en"] {
                crate::ui::i18n::apply_preference(Some(locale));
                assert_eq!(
                    mode_notice_text(&notice, &messages).as_ref(),
                    "Original host message"
                );
            }
        }
    }

    #[test]
    fn password_ui_requires_explicit_host_capability() {
        let status: StoreStatus =
            serde_json::from_value(serde_json::json!({"state":{"state":"open"},"mode":"plain"}))
                .unwrap();
        assert!(!status.password_settings_available);
        assert!(status.store_id.is_none());
        assert!(status.completion_notice.is_none());
        assert!(status.mode_change_notice.is_none());
        assert_eq!(status.persistence_status, Default::default());
    }
    #[test]
    fn mode_notice_dismissal_preserves_failures_and_fences_replacement_notices() {
        let mut status: StoreStatus = serde_json::from_value(serde_json::json!({
            "state": {"state":"open"}, "mode":"password", "store_id":"current",
            "mode_change_notice": {
                "store_id":"current", "notice_id":"first", "message":"Password changed."
            }
        }))
        .unwrap();
        let first = status.current_mode_notice().unwrap().clone();
        assert!(apply_mode_notice_ack(&mut status, &first, false));
        assert_eq!(status.current_mode_notice(), Some(&first));
        status.mode_change_notice.as_mut().unwrap().notice_id = "newer".into();
        let newer = status.current_mode_notice().unwrap().clone();
        assert!(!apply_mode_notice_ack(&mut status, &first, true));
        assert_eq!(status.current_mode_notice(), Some(&newer));
        status.store_id = Some("replacement".into());
        assert!(!apply_mode_notice_ack(&mut status, &newer, true));
        assert_eq!(status.mode_change_notice, Some(newer.clone()));
        status.store_id = Some("current".into());
        status.state = StoreState::Locked;
        assert!(!apply_mode_notice_ack(&mut status, &newer, true));
        status.state = StoreState::Open;
        assert!(apply_mode_notice_ack(&mut status, &newer, true));
        assert!(status.mode_change_notice.is_none());
    }
    #[test]
    fn delayed_status_cannot_resurrect_an_exact_acknowledged_notice() {
        let mut status: StoreStatus = serde_json::from_value(serde_json::json!({
            "state": {"state":"open"}, "mode":"password", "store_id":"current",
            "mode_change_notice": {
                "store_id":"current", "notice_id":"first", "message":"Password changed."
            }
        }))
        .unwrap();
        let acknowledged = status.current_mode_notice().unwrap().clone();
        filter_acknowledged_mode_notice(&mut status, None);
        assert_eq!(status.current_mode_notice(), Some(&acknowledged));
        filter_acknowledged_mode_notice(&mut status, Some(&acknowledged));
        assert!(status.mode_change_notice.is_none());
        let newer = StoreModeNotice {
            notice_id: "newer".into(),
            ..acknowledged.clone()
        };
        status.mode_change_notice = Some(newer.clone());
        filter_acknowledged_mode_notice(&mut status, Some(&acknowledged));
        assert_eq!(status.mode_change_notice, Some(newer));
        let replacement = StoreModeNotice {
            store_id: "replacement".into(),
            ..acknowledged.clone()
        };
        status.mode_change_notice = Some(replacement.clone());
        filter_acknowledged_mode_notice(&mut status, Some(&acknowledged));
        assert_eq!(status.mode_change_notice, Some(replacement));
    }
    #[gpui::test]
    fn encryption_feedback_only_claims_the_privacy_and_security_section(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:1".into(), String::new())
                .unwrap(),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
                backend,
                "user".into(),
                crate::settings::AppSettings::default(),
                crate::shortcuts::ShortcutSnapshot {
                    generation: 1,
                    rows: Vec::new(),
                    last_error: None,
                    compatibility_warning: None,
                },
                Section::General,
                cx,
            )
        });
        settings.update(cx, |settings, cx| {
            assert!(!settings.show_local_store_notice("Needs attention.", cx));
            assert!(settings.advanced.notice.is_none());
            settings.section = Section::Account;
            assert!(settings.show_local_store_notice("Needs attention.", cx));
            assert_eq!(
                settings
                    .advanced
                    .notice
                    .as_ref()
                    .map(UiText::resolve)
                    .as_deref(),
                Some("Needs attention.")
            );
        });
    }
    #[gpui::test]
    fn local_store_content_is_shown_in_account_only_when_available_or_noticing(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:1".into(), String::new())
                .unwrap(),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
                backend,
                "user".into(),
                crate::settings::AppSettings::default(),
                crate::shortcuts::ShortcutSnapshot {
                    generation: 1,
                    rows: Vec::new(),
                    last_error: None,
                    compatibility_warning: None,
                },
                Section::General,
                cx,
            )
        });
        settings.update(cx, |settings, _| {
            assert!(!settings.advanced_has_content());
            settings.advanced.notice = Some("Needs attention.".into());
            assert!(settings.advanced_has_content());
            settings.advanced.notice = None;
            assert!(!settings.advanced_has_content());
            settings.advanced.status = Some(
                serde_json::from_value(serde_json::json!({
                    "state": {"state": "open"},
                    "mode": "plain",
                    "password_settings_available": true,
                }))
                .unwrap(),
            );
            assert!(settings.advanced_has_content());
            assert!(settings.visible_sections().contains(&Section::Account));
        });
    }
    #[test]
    fn erase_copy_names_only_this_version_and_preserves_previous_data() {
        assert!(tr(ERASE_COPY).contains("this version's store"));
        assert!(tr(ERASE_COPY).contains("previous app data remain untouched"));
        assert!(tr(ERASE_COPY).contains("remote pairings"));
    }
}
