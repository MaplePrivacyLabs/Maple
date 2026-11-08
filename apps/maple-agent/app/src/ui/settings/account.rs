//! The Account section: who is signed in, email verification, and
//! sign-out. Later sections (password, deletion) build on this state.

use crate::ui::i18n::{UiText, tr};
use gpui::{Context, Div, Entity, Focusable, div, prelude::*, px};

use super::{
    AccountDeleted, SettingsScreen, SettingsTarget, SignOutRequested, info_row, section_title,
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
            let delete = match self.account.delete {
                DeleteFlow::Idle => vec![AccountTarget::DeleteStart],
                DeleteFlow::Acknowledging => vec![
                    AccountTarget::DeleteAcknowledge,
                    AccountTarget::DeleteRequestCode,
                    AccountTarget::DeleteCancel,
                ],
                DeleteFlow::CodeSent { .. } => vec![
                    AccountTarget::DeleteCode,
                    AccountTarget::DeleteConfirm,
                    AccountTarget::DeleteCancel,
                ],
            };
            targets.extend(delete.into_iter().map(SettingsTarget::Account));
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
                    Ok(secret) => this.account.delete = DeleteFlow::CodeSent { secret },
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
        let mut pane = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title(&tr("settings-account-account")));
        let Some(info) = self.account.info.as_ref() else {
            return pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_faint()))
                    .child(match &self.account.load_error {
                        Some(message) => message.resolve(),
                        None => tr("settings-account-loading-account"),
                    }),
            );
        };

        let is_guest = info.login_method == MapleLoginMethod::Guest;
        pane = pane.child(match &info.email {
            Some(email) => self.render_email_row(email, info.email_verified, cx),
            None if is_guest => guest_id_row(&info.user_id, cx),
            None => info_row(
                &tr("settings-account-email"),
                tr("settings-account-no-email-on-this-account").to_string(),
            )
            .into_any_element(),
        });
        if let Some(notice) = &self.account.verification_notice {
            pane = pane.child(widgets::banner(widgets::Tone::Info, notice.clone()));
        }
        pane = pane
            .child(info_row(
                &tr("settings-account-sign-in-method"),
                super::presentation::login_method(info.login_method),
            ))
            .child(info_row(
                &tr("settings-account-member-since"),
                member_since(&info.created_at, &self.messages),
            ));
        if info.login_method.has_password() {
            pane = pane.child(self.render_password_form(cx));
        }
        pane = pane.child(
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
                            .flex_none()
                            .py_1p5()
                            .on_click(cx.listener(|_this, _event, _window, cx| {
                                cx.emit(SignOutRequested);
                            }))
                            .child(tr("settings-account-sign-out")),
                    ),
            ),
        );
        pane.child(section_title(&tr("settings-account-danger-zone")))
            .child(self.render_delete_account(cx))
    }

    fn render_delete_account(&self, cx: &mut Context<Self>) -> Div {
        let busy = self.account.delete_busy;
        let mut card = widgets::card_row()
            .flex()
            .flex_col()
            .gap_3()
            .border_color(gpui::rgb(theme::status_error()));
        let field = |target: AccountTarget, input: &Entity<TextInput>| {
            self.application_target(
                move || SettingsTarget::Account(target),
                widgets::input_frame().child(input.clone()),
            )
        };
        let button = |id: &'static str, label: String, danger: bool, target: AccountTarget| {
            let base = if danger {
                widgets::danger_button(id)
            } else {
                widgets::secondary_button(id)
            };
            self.application_target(
                move || SettingsTarget::Account(target),
                base.py_1p5()
                    .when(busy, |el| el.opacity(0.6))
                    .when(!busy, |el| {
                        el.on_click(cx.listener(move |this, _event, window, cx| {
                            this.activate_account_target(target, window, cx);
                        }))
                    })
                    .child(label),
            )
        };
        match &self.account.delete {
            DeleteFlow::Idle => {
                card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(super::setting_copy(
                            tr("settings-account-delete-account"),
                            tr("settings-account-permanently-deletes-your-account-tasks-and-encrypted-data-this-ca"),
                        ))
                        .child(button(
                            "account-delete-start",
                            tr("settings-account-delete-account-more").to_string(),
                            true,
                            AccountTarget::DeleteStart,
                        )),
                );
            }
            DeleteFlow::Acknowledging => {
                card = card
                    .child(super::setting_copy(
                        tr("settings-account-delete-account"),
                        tr("settings-account-this-permanently-deletes-your-account-tasks-and-encrypted-data-ty"),
                    ))
                    .child(field(
                        AccountTarget::DeleteAcknowledge,
                        &self.account.delete_ack,
                    ))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button(
                                "account-delete-cancel",
                                tr("settings-account-cancel").to_string(),
                                false,
                                AccountTarget::DeleteCancel,
                            ))
                            .child(button(
                                "account-delete-request",
                                if busy {
                                    tr("settings-account-sending")
                                } else {
                                    tr("settings-account-send-confirmation-code")
                                }
                                .to_string(),
                                true,
                                AccountTarget::DeleteRequestCode,
                            )),
                    );
            }
            DeleteFlow::CodeSent { .. } => {
                card = card
                    .child(super::setting_copy(
                        tr("settings-account-confirm-deletion"),
                        tr("settings-account-we-emailed-you-a-confirmation-code-enter-it-to-delete-the-account"),
                    ))
                    .child(field(AccountTarget::DeleteCode, &self.account.delete_code))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button(
                                "account-delete-cancel",
                                tr("settings-account-cancel").to_string(),
                                false,
                                AccountTarget::DeleteCancel,
                            ))
                            .child(button(
                                "account-delete-confirm",
                                if busy {
                                    tr("settings-account-deleting")
                                } else {
                                    tr("settings-account-delete-my-account")
                                }
                                .to_string(),
                                true,
                                AccountTarget::DeleteConfirm,
                            )),
                    );
            }
        }
        if let Some(message) = &self.account.delete_error {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::status_error()))
                    .child(message.clone()),
            );
        }
        card
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
                    widgets::primary_button("account-password-save")
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
        let row = widgets::card_row()
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
    widgets::card_row()
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
