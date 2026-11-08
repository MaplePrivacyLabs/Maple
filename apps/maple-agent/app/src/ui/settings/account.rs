//! The Account section: who is signed in, email verification, and
//! sign-out. Later sections (password, deletion) build on this state.

use crate::ui::i18n::{UiText, tr};
use gpui::{Context, Div, Entity, Focusable, div, prelude::*, px};

use super::{
    AccountDeleted, SettingsScreen, SettingsTarget, SignOutRequested, info_row_content,
    page_header, subsection_title,
};
use crate::backend::{MapleAccount, MapleLoginMethod};
use crate::ui::icons::icon;
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::widgets;

/// Rows in the Account pane that Application Vim can land on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AccountTarget {
    ResendVerification,
    PasswordCurrent,
    PasswordNew,
    PasswordConfirm,
    PasswordSave,
    SignOut,
    DeleteStart,
    DeleteAcknowledge,
    DeleteRequestCode,
    DeleteCode,
    DeleteConfirm,
    DeleteCancel,
}

pub(super) struct AccountState {
    /// The profile; None until loaded or when loading failed.
    pub(super) info: Option<MapleAccount>,
    pub(super) load_error: Option<UiText>,
    pub(super) verification_notice: Option<UiText>,
    pub(super) verification_busy: bool,
    pub(super) password: PasswordForm,
    pub(super) delete: DeleteFlow,
    /// Typed acknowledgement ("DELETE") and the emailed code.
    pub(super) delete_ack: Entity<TextInput>,
    pub(super) delete_code: Entity<TextInput>,
    pub(super) delete_busy: bool,
    /// Focus the dialog's current field the next time it draws.
    pub(super) delete_focus_pending: bool,
    pub(super) delete_error: Option<UiText>,
}

/// Where the two-step account deletion stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DeleteFlow {
    Idle,
    /// The user opened the danger zone and must type DELETE.
    Acknowledging,
    /// A code was emailed; `secret` is the client half the server checks.
    CodeSent {
        secret: String,
    },
}

/// The word the user types to unlock the deletion request.
pub(super) const DELETE_ACKNOWLEDGEMENT: &str = "DELETE";

/// The change-password form. Fields are cleared after a successful change.
pub(super) struct PasswordForm {
    pub(super) current: Entity<TextInput>,
    pub(super) new: Entity<TextInput>,
    pub(super) confirm: Entity<TextInput>,
    pub(super) busy: bool,
    /// (succeeded, message) after the last attempt.
    pub(super) notice: Option<(bool, UiText)>,
}

impl PasswordForm {
    pub(super) fn inputs(&self) -> [Entity<TextInput>; 3] {
        [self.current.clone(), self.new.clone(), self.confirm.clone()]
    }
}

impl AccountState {
    /// Every text input this section owns, for Application Vim wiring.
    pub(super) fn inputs(&self) -> Vec<Entity<TextInput>> {
        let mut inputs = self.password.inputs().to_vec();
        inputs.push(self.delete_ack.clone());
        inputs.push(self.delete_code.clone());
        inputs
    }
}

impl AccountState {
    pub(super) fn new(
        application_vim_enabled: bool,
        application_focus: gpui::FocusHandle,
        cx: &mut Context<SettingsScreen>,
    ) -> Self {
        let field =
            |placeholder: &'static str, tab_index: isize, cx: &mut Context<SettingsScreen>| {
                let focus = application_focus.clone();
                cx.new(move |cx| {
                    TextInput::localized(placeholder, cx)
                        .masked()
                        .with_tab_index(tab_index)
                        .application_vim(application_vim_enabled)
                        .on_application_escape(move |window, cx| window.focus(&focus, cx))
                })
            };
        let password = PasswordForm {
            current: field("settings-account-current-password", 10, cx),
            new: field("settings-account-new-password", 11, cx),
            confirm: field("settings-account-confirm-new-password", 12, cx),
            busy: false,
            notice: None,
        };
        let plain =
            |placeholder: &'static str, tab_index: isize, cx: &mut Context<SettingsScreen>| {
                let focus = application_focus.clone();
                cx.new(move |cx| {
                    TextInput::localized(placeholder, cx)
                        .with_tab_index(tab_index)
                        .application_vim(application_vim_enabled)
                        .on_application_escape(move |window, cx| window.focus(&focus, cx))
                })
            };
        Self {
            info: None,
            load_error: None,
            verification_notice: None,
            verification_busy: false,
            password,
            delete: DeleteFlow::Idle,
            delete_ack: plain("settings-account-type-delete-to-continue", 13, cx),
            delete_code: plain("settings-account-confirmation-code", 14, cx),
            delete_busy: false,
            delete_focus_pending: false,
            delete_error: None,
        }
    }

    /// Whether the password form is on screen: only accounts that sign in
    /// with a password can change one.
    fn has_password(&self) -> bool {
        self.info
            .as_ref()
            .is_some_and(|info| info.login_method.has_password())
    }

    /// Whether the Resend control is on screen.
    fn can_resend_verification(&self) -> bool {
        self.info
            .as_ref()
            .is_some_and(|info| info.email.is_some() && !info.email_verified)
    }
}

impl SettingsScreen {
    /// Preserve typed backend failures and semantic cancellation until the UI thread.
    pub(super) fn call_presented<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = Result<T, crate::backend::HostFailure>>
        + Send
        + 'static,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<T, crate::ui::task::Failure>, &mut Context<Self>) + 'static,
    ) {
        let bridge =
            crate::ui::task::call_semantic(&self.backend, future, "flows-task-cancelled", cx, then);
        crate::ui::task::retain(&self.bridged_tasks, bridge);
    }

    pub(super) fn load_account(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move { backend.call_presented("account", (&user_id,)).await },
            cx,
            |this, result, cx| {
                match result {
                    Ok(info) => {
                        this.account.info = Some(info);
                        this.account.load_error = None;
                    }
                    Err(message) => this.account.load_error = Some(message.into()),
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    pub(super) fn account_targets(&self) -> Vec<SettingsTarget> {
        let mut targets = Vec::new();
        if self.account.can_resend_verification() {
            targets.push(SettingsTarget::Account(AccountTarget::ResendVerification));
        }
        if self.account.has_password() {
            targets.extend(
                [
                    AccountTarget::PasswordCurrent,
                    AccountTarget::PasswordNew,
                    AccountTarget::PasswordConfirm,
                    AccountTarget::PasswordSave,
                ]
                .map(SettingsTarget::Account),
            );
        }
        targets.push(SettingsTarget::Account(AccountTarget::SignOut));
        if self.account.info.is_some() {
            targets.push(SettingsTarget::Account(AccountTarget::DeleteStart));
        }
        targets
    }

    pub(super) fn activate_account_target(
        &mut self,
        target: AccountTarget,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let focus_input =
            |input: &Entity<TextInput>, window: &mut gpui::Window, cx: &mut Context<Self>| {
                let handle = input.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            };
        match target {
            AccountTarget::ResendVerification => self.resend_verification(cx),
            AccountTarget::PasswordCurrent => {
                focus_input(&self.account.password.current, window, cx)
            }
            AccountTarget::PasswordNew => focus_input(&self.account.password.new, window, cx),
            AccountTarget::PasswordConfirm => {
                focus_input(&self.account.password.confirm, window, cx)
            }
            AccountTarget::PasswordSave => self.submit_password_change(cx),
            AccountTarget::SignOut => cx.emit(SignOutRequested),
            AccountTarget::DeleteStart => self.begin_account_deletion(cx),
            AccountTarget::DeleteAcknowledge => focus_input(&self.account.delete_ack, window, cx),
            AccountTarget::DeleteRequestCode => self.request_account_deletion(cx),
            AccountTarget::DeleteCode => focus_input(&self.account.delete_code, window, cx),
            AccountTarget::DeleteConfirm => self.confirm_account_deletion(cx),
            AccountTarget::DeleteCancel => self.cancel_account_deletion(cx),
        }
    }

    pub(super) fn begin_account_deletion(&mut self, cx: &mut Context<Self>) {
        if self.account.delete != DeleteFlow::Idle {
            return;
        }
        self.account.delete = DeleteFlow::Acknowledging;
        self.account.delete_error = None;
        self.account.delete_focus_pending = true;
        self.reconcile_application_vim_target();
        cx.notify();
    }

    pub(super) fn cancel_account_deletion(&mut self, cx: &mut Context<Self>) {
        if self.account.delete_busy {
            return;
        }
        self.account.delete = DeleteFlow::Idle;
        self.account.delete_error = None;
        self.account
            .delete_ack
            .update(cx, |input, cx| input.clear(cx));
        self.account
            .delete_code
            .update(cx, |input, cx| input.clear(cx));
        self.reconcile_application_vim_target();
        cx.notify();
    }

    /// The deletion dialog's controls, in display order; the only targets
    /// while it is open.
    pub(super) fn delete_dialog_targets(&self) -> Vec<SettingsTarget> {
        match self.account.delete {
            DeleteFlow::Idle => Vec::new(),
            DeleteFlow::Acknowledging => vec![
                AccountTarget::DeleteAcknowledge,
                AccountTarget::DeleteCancel,
                AccountTarget::DeleteRequestCode,
            ],
            DeleteFlow::CodeSent { .. } => vec![
                AccountTarget::DeleteCode,
                AccountTarget::DeleteCancel,
                AccountTarget::DeleteConfirm,
            ],
        }
        .into_iter()
        .map(SettingsTarget::Account)
        .collect()
    }

    /// Escape or a backdrop click. Before a code is emailed this is a plain
    /// cancel; after it, the code is real and a stray click must not throw
    /// it away, so only the explicit Cancel button ends the flow.
    pub(super) fn dismiss_delete_dialog(&mut self, cx: &mut Context<Self>) {
        if self.account.delete == DeleteFlow::Acknowledging {
            self.cancel_account_deletion(cx);
        }
    }

    /// Step one: the user typed DELETE; ask the server to email a code.
    pub(super) fn request_account_deletion(&mut self, cx: &mut Context<Self>) {
        if self.account.delete_busy || self.account.delete != DeleteFlow::Acknowledging {
            return;
        }
        let typed = self.account.delete_ack.read(cx).text();
        if typed.trim() != DELETE_ACKNOWLEDGEMENT {
            self.account.delete_error = Some(UiText::message(
                "settings-delete-confirmation",
                [(
                    "delete-acknowledgement",
                    (DELETE_ACKNOWLEDGEMENT).to_string().into(),
                )],
            ));
            cx.notify();
            return;
        }
        self.account.delete_busy = true;
        self.account.delete_error = None;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented("request_account_deletion", (&user_id,))
                    .await
            },
            cx,
            |this, result, cx| {
                this.account.delete_busy = false;
                match result {
                    Ok(secret) => {
                        this.account.delete = DeleteFlow::CodeSent { secret };
                        this.account.delete_focus_pending = true;
                    }
                    Err(message) => this.account.delete_error = Some(message.into()),
                }
                this.reconcile_application_vim_target();
                cx.notify();
            },
        );
    }

    /// Step two: the emailed code plus the client secret delete the
    /// account. On success the screen reports it and the app signs out.
    pub(super) fn confirm_account_deletion(&mut self, cx: &mut Context<Self>) {
        let DeleteFlow::CodeSent { secret } = &self.account.delete else {
            return;
        };
        if self.account.delete_busy {
            return;
        }
        let secret = secret.clone();
        let code = self.account.delete_code.read(cx).text();
        if code.trim().is_empty() {
            self.account.delete_error = Some(UiText::key(
                "settings-account-enter-the-confirmation-code-from-the-email",
            ));
            cx.notify();
            return;
        }
        self.account.delete_busy = true;
        self.account.delete_error = None;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented::<()>("confirm_account_deletion", (&user_id, code, secret))
                    .await
            },
            cx,
            |this, result, cx| {
                this.account.delete_busy = false;
                match result {
                    Ok(()) => cx.emit(AccountDeleted),
                    Err(message) => this.account.delete_error = Some(message.into()),
                }
                cx.notify();
            },
        );
    }

    pub(super) fn submit_password_change(&mut self, cx: &mut Context<Self>) {
        if self.account.password.busy {
            return;
        }
        let current = self.account.password.current.read(cx).text();
        let new = self.account.password.new.read(cx).text();
        let confirm = self.account.password.confirm.read(cx).text();
        if let Err(message) = password_form_check(&current, &new, &confirm) {
            self.account.password.notice = Some((false, message));
            cx.notify();
            return;
        }
        self.account.password.busy = true;
        self.account.password.notice = None;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented::<()>("change_password", (&user_id, current, new))
                    .await
            },
            cx,
            |this, result, cx| {
                this.account.password.busy = false;
                this.account.password.notice = Some(match result {
                    Ok(()) => {
                        for input in this.account.password.inputs() {
                            input.update(cx, |input, cx| input.clear(cx));
                        }
                        (true, UiText::key("settings-account-password-changed"))
                    }
                    Err(message) => (false, message.into()),
                });
                cx.notify();
            },
        );
    }

    pub(super) fn resend_verification(&mut self, cx: &mut Context<Self>) {
        if self.account.verification_busy || !self.account.can_resend_verification() {
            return;
        }
        self.account.verification_busy = true;
        self.account.verification_notice = None;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented::<()>("request_verification_email", (&user_id,))
                    .await
            },
            cx,
            |this, result, cx| {
                this.account.verification_busy = false;
                this.account.verification_notice = Some(match result {
                    Ok(()) => {
                        UiText::key("settings-account-verification-email-sent-check-your-inbox")
                    }
                    Err(message) => message.into(),
                });
                cx.notify();
            },
        );
    }

    pub(super) fn render_account_pane(&self, cx: &mut Context<Self>) -> Div {
        let mut pane = div().flex().flex_col().gap_4().child(page_header(
            &tr("settings-section-account"),
            &tr("settings-account-subtitle"),
        ));
        let Some(info) = self.account.info.as_ref() else {
            pane = pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_faint()))
                    .child(match &self.account.load_error {
                        Some(message) => message.resolve(),
                        None => tr("settings-account-loading-account"),
                    }),
            );
            // Sign out and the local encryption controls do not depend on the
            // profile fetch; an expired or offline session still needs them.
            pane = pane
                .child(subsection_title(&tr("settings-account-security-group")))
                .child(self.render_sign_out_row(cx));
            if self.advanced_has_content() {
                pane = pane.child(self.render_advanced(cx));
            }
            return pane;
        };

        let is_guest = info.login_method == MapleLoginMethod::Guest;
        let profile_rows = vec![
            match &info.email {
                Some(email) => self.render_email_row(email, info.email_verified, cx),
                None if is_guest => guest_id_row(&info.user_id, cx),
                None => info_row_content(
                    &tr("settings-account-email"),
                    tr("settings-account-no-email-on-this-account").to_string(),
                )
                .into_any_element(),
            },
            info_row_content(
                &tr("settings-account-sign-in-method"),
                super::presentation::login_method(info.login_method),
            )
            .into_any_element(),
            info_row_content(
                &tr("settings-account-member-since"),
                member_since(&info.created_at, &self.messages),
            )
            .into_any_element(),
        ];
        pane = pane
            .child(subsection_title(&tr("settings-account-profile")))
            .child(widgets::card_stack(profile_rows));
        if let Some(notice) = &self.account.verification_notice {
            pane = pane.child(
                widgets::banner(theme::bg_sidebar_pill())
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(notice.clone()),
            );
        }
        pane = pane.child(subsection_title(&tr("settings-account-security-group")));
        if info.login_method.has_password() {
            pane = pane.child(self.render_password_form(cx));
        }
        pane = pane.child(self.render_sign_out_row(cx));
        if self.advanced_has_content() {
            pane = pane.child(self.render_advanced(cx));
        }
        pane.child(subsection_title(&tr("settings-account-danger-zone")))
            .child(self.render_delete_account(cx))
    }

    fn render_sign_out_row(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        self.application_target(
            || SettingsTarget::Account(AccountTarget::SignOut),
            widgets::card_row()
                .flex()
                .items_center()
                .justify_between()
                .gap_4()
                .child(super::setting_copy(
                    tr("settings-account-sign-out"),
                    tr("settings-account-ends-this-session-on-this-device-your-tasks-stay-in-your-account"),
                ))
                .child(
                    widgets::secondary_button("account-sign-out")
                        .debug_selector(|| "account-sign-out".into())
                        .flex_none()
                        .py_1p5()
                        .on_click(cx.listener(|_this, _event, _window, cx| {
                            cx.emit(SignOutRequested);
                        }))
                        .child(tr("settings-account-sign-out")),
                ),
        )
    }

    /// The row that starts account deletion: quiet at rest, outlined in the
    /// error color. The weight lives in the dialog it opens.
    fn render_delete_account(&self, cx: &mut Context<Self>) -> Div {
        let busy = self.account.delete_busy;
        widgets::card_row()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(super::setting_copy(
                tr("settings-account-delete-account"),
                tr("settings-account-permanently-deletes-your-account-tasks-and-encrypted-data-this-ca"),
            ))
            .child(
                self.application_target(
                    || SettingsTarget::Account(AccountTarget::DeleteStart),
                    widgets::danger_outline_button("account-delete-start")
                        .flex_none()
                        .py_1p5()
                        .when(busy, |el| el.opacity(0.6))
                        .when(!busy, |el| {
                            el.on_click(cx.listener(|this, _event, _window, cx| {
                                this.begin_account_deletion(cx);
                            }))
                        })
                        .child(icon("circle-alert", widgets::ROW_ICON, theme::status_error()))
                        .child(tr("settings-account-delete-account-more")),
                ),
            )
    }

    /// Focus the deletion dialog's current field the first time it draws.
    pub(super) fn focus_delete_dialog(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if !std::mem::take(&mut self.account.delete_focus_pending) {
            return;
        }
        let input = match &self.account.delete {
            DeleteFlow::Idle => return,
            DeleteFlow::Acknowledging => &self.account.delete_ack,
            DeleteFlow::CodeSent { .. } => &self.account.delete_code,
        };
        window.focus(&input.read(cx).focus_handle(cx), cx);
    }

    /// Account deletion as a dialog: step one types the acknowledgement,
    /// step two enters the emailed code. The destructive button stays
    /// inert until the field is filled in.
    pub(super) fn render_delete_dialog(
        &self,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let busy = self.account.delete_busy;
        let (title, body, input, target, input_target, action_label, action_target, ready) =
            match &self.account.delete {
                DeleteFlow::Idle => return None,
                DeleteFlow::Acknowledging => (
                    tr("settings-account-delete-dialog-title"),
                    tr(
                        "settings-account-this-permanently-deletes-your-account-tasks-and-encrypted-data-ty",
                    ),
                    self.account.delete_ack.clone(),
                    "account-delete-ack",
                    AccountTarget::DeleteAcknowledge,
                    if busy {
                        tr("settings-account-sending")
                    } else {
                        tr("settings-account-send-confirmation-code")
                    },
                    AccountTarget::DeleteRequestCode,
                    self.account.delete_ack.read(cx).text().trim() == DELETE_ACKNOWLEDGEMENT,
                ),
                DeleteFlow::CodeSent { .. } => (
                    tr("settings-account-confirm-deletion"),
                    tr(
                        "settings-account-we-emailed-you-a-confirmation-code-enter-it-to-delete-the-account",
                    ),
                    self.account.delete_code.clone(),
                    "account-delete-code",
                    AccountTarget::DeleteCode,
                    if busy {
                        tr("settings-account-deleting")
                    } else {
                        tr("settings-account-delete-my-account")
                    },
                    AccountTarget::DeleteConfirm,
                    !self.account.delete_code.read(cx).text().trim().is_empty(),
                ),
            };
        let _ = target;
        let width = (window.viewport_size().width - px(48.)).clamp(px(0.), px(480.));
        Some(
            div()
                .id("account-delete-backdrop")
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
                    cx.listener(|this, _event, _window, cx| this.dismiss_delete_dialog(cx)),
                )
                .child(
                    div()
                        .id("account-delete-dialog")
                        .debug_selector(|| "account-delete-dialog".to_string())
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
                                    this.dismiss_delete_dialog(cx);
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
                                .child(gpui::text!(body)),
                        )
                        .child(self.application_target(
                            move || SettingsTarget::Account(input_target),
                            widgets::input_frame().text_sm().child(input),
                        ))
                        .when_some(self.account.delete_error.clone(), |dialog, message| {
                            dialog.child(
                                div()
                                    .text_sm()
                                    .text_color(gpui::rgb(theme::status_error()))
                                    .child(message),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap_2()
                                .child(
                                    self.application_target(
                                        || SettingsTarget::Account(AccountTarget::DeleteCancel),
                                        widgets::ghost_button("account-delete-cancel")
                                            .py_1p5()
                                            .when(!busy, |el| {
                                                el.on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.cancel_account_deletion(cx);
                                                    },
                                                ))
                                            })
                                            .child(tr("settings-account-cancel")),
                                    ),
                                )
                                .child(
                                    self.application_target(
                                        move || SettingsTarget::Account(action_target),
                                        widgets::danger_button("account-delete-action")
                                            .py_1p5()
                                            .when(busy || !ready, |el| el.opacity(0.5))
                                            .when(!busy && ready, |el| {
                                                el.on_click(cx.listener(
                                                    move |this, _event, window, cx| {
                                                        this.activate_account_target(
                                                            action_target,
                                                            window,
                                                            cx,
                                                        );
                                                    },
                                                ))
                                            })
                                            .child(action_label),
                                    ),
                                ),
                        ),
                ),
        )
    }

    fn render_password_form(&self, cx: &mut Context<Self>) -> Div {
        let form = &self.account.password;
        let busy = form.busy;
        let field =
            |target: AccountTarget, label: gpui::SharedString, input: &Entity<TextInput>| {
                self.application_target(
                    move || SettingsTarget::Account(target),
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .text_color(gpui::rgb(theme::text_secondary()))
                                .child(label.to_string()),
                        )
                        .child(widgets::input_frame().child(input.clone())),
                )
            };
        let mut card = widgets::card_row()
            .flex()
            .flex_col()
            .gap_3()
            .child(super::setting_copy(
                tr("settings-account-change-password"),
                tr("settings-account-use-at-least-8-characters-other-devices-stay-signed-in"),
            ))
            .child(field(
                AccountTarget::PasswordCurrent,
                tr("settings-account-current-password"),
                &form.current,
            ))
            .child(field(
                AccountTarget::PasswordNew,
                tr("settings-account-new-password"),
                &form.new,
            ))
            .child(field(
                AccountTarget::PasswordConfirm,
                tr("settings-account-confirm-new-password"),
                &form.confirm,
            ));
        if let Some((ok, message)) = &form.notice {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(if *ok {
                        theme::status_success()
                    } else {
                        theme::status_error()
                    }))
                    .child(message.clone()),
            );
        }
        card.child(
            div().flex().justify_end().child(
                self.application_target(
                    || SettingsTarget::Account(AccountTarget::PasswordSave),
                    widgets::default_button("account-password-save")
                        .py_1p5()
                        .when(busy, |el| el.opacity(0.6))
                        .when(!busy, |el| {
                            el.on_click(cx.listener(|this, _event, _window, cx| {
                                this.submit_password_change(cx);
                            }))
                        })
                        .child(if busy {
                            tr("settings-account-changing")
                        } else {
                            tr("settings-account-change-password")
                        }),
                ),
            ),
        )
    }

    fn render_email_row(
        &self,
        email: &str,
        verified: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let busy = self.account.verification_busy;
        let mut value = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(email.to_string()),
            )
            .child(if verified {
                status_badge(tr("settings-account-verified"), theme::status_success())
            } else {
                status_badge(tr("settings-account-unverified"), theme::status_warning())
            });
        if !verified {
            value = value.child(
                widgets::secondary_button("account-resend-verification")
                    .flex_none()
                    .py_1p5()
                    .when(busy, |el| el.opacity(0.6))
                    .when(!busy, |el| {
                        el.on_click(cx.listener(|this, _event, _window, cx| {
                            this.resend_verification(cx);
                        }))
                    })
                    .child(if busy {
                        tr("settings-account-sending")
                    } else {
                        tr("settings-account-resend-email")
                    }),
            );
        }
        let row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(tr("settings-account-email")),
            )
            .child(value);
        if verified {
            row.into_any_element()
        } else {
            self.application_target(
                || SettingsTarget::Account(AccountTarget::ResendVerification),
                row,
            )
        }
    }
}

/// Guests sign in with their account id, so it is the thing to show and
/// copy where an email would be.
fn guest_id_row(user_id: &str, cx: &mut Context<SettingsScreen>) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .child(super::setting_copy(
            tr("settings-account-anonymous-account"),
            tr("settings-account-this-account-has-no-email-keep-the-account-id-and-password-somewh"),
        ))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .font_family(crate::assets::FONT_MONO)
                        .text_color(gpui::rgb(theme::text_primary()))
                        .child(user_id.to_string()),
                )
                .child(widgets::copy_button(
                    "account-copy-id",
                    gpui::SharedString::from(user_id.to_string()),
                    None,
                    Some(cx.entity_id()),
                )),
        )
        .into_any_element()
}

/// Client-side checks before the backend call; the messages match the
/// web app's form validation.
pub(super) fn password_form_check(current: &str, new: &str, confirm: &str) -> Result<(), UiText> {
    if current.is_empty() {
        return Err(UiText::key("settings-account-enter-your-current-password"));
    }
    crate::backend::validate_new_password_presented(new)
        .map_err(|failure| crate::ui::host_errors::failure_text(&failure))?;
    if new != confirm {
        return Err(UiText::key(
            "settings-account-the-new-passwords-do-not-match",
        ));
    }
    if new == current {
        return Err(UiText::key(
            "settings-account-the-new-password-must-differ-from-the-current-one",
        ));
    }
    Ok(())
}

fn status_badge(label: gpui::SharedString, color: u32) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded(theme::RADIUS_SM)
        .text_xs()
        .text_color(gpui::rgb(color))
        .bg(gpui::rgb(theme::bg_sidebar_pill()))
        .child(icon(
            if color == theme::status_success() {
                "check"
            } else {
                "x"
            },
            px(11.),
            color,
        ))
        .child(label)
}

/// "September 6, 2026" from an RFC 3339 timestamp; the raw text when it
/// does not parse.
pub(super) fn member_since(created_at: &str, messages: &crate::ui::i18n::MessageCache) -> String {
    messages
        .computed(
            "settings-member-since",
            created_at,
            || match chrono::DateTime::parse_from_rfc3339(created_at) {
                Ok(time) => super::presentation::date(
                    time.with_timezone(&chrono::Local).date_naive(),
                    messages,
                ),
                Err(_) => created_at.to_string().into(),
            },
        )
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_since_formats_a_date_and_keeps_unparseable_text() {
        let formatted = member_since("2024-03-05T12:00:00Z", &Default::default());
        assert!(formatted.ends_with(", 2024"), "{formatted}");
        assert_eq!(member_since("soon", &Default::default()), "soon");
    }

    #[test]
    fn password_form_checks_mirror_the_web_rules() {
        assert!(password_form_check("", "longenough", "longenough").is_err());
        assert!(password_form_check("old", "short", "short").is_err());
        assert!(password_form_check("old", "longenough", "different1").is_err());
        assert!(password_form_check("longenough", "longenough", "longenough").is_err());
        assert!(password_form_check("old", "longenough", "longenough").is_ok());
    }

    #[test]
    fn resend_is_offered_only_for_an_unverified_email() {
        fn resendable(info: &MapleAccount) -> bool {
            info.email.is_some() && !info.email_verified
        }
        let mut info = MapleAccount {
            user_id: "u".into(),
            email: Some("a@b.c".into()),
            name: None,
            email_verified: false,
            login_method: MapleLoginMethod::Email,
            created_at: String::new(),
        };
        assert!(resendable(&info));
        info.email_verified = true;
        assert!(!resendable(&info));
        info.email = None;
        info.email_verified = false;
        assert!(!resendable(&info));
    }
}
