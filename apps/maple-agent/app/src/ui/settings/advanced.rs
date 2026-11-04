//! Local advanced preferences and capability-gated encryption controls. A
//! remote task selection never redirects these settings to another computer.

use gpui::{App, Context, Div, Entity, EventEmitter, div, prelude::*};
use maple_agent::store::{
    StoreState,
    lifecycle::{StoreMode, StoreModeNotice},
};
use serde_json::Value;

use super::{Section, SettingsScreen, TextInput, section_title, theme, widgets};
use crate::ui::store_gate::{
    ConfirmationRequest, ERASE_COPY, PasswordAction, PasswordForm, PasswordFormFinished,
    RetryStorePersistence, StoreStatus,
};

pub(super) struct AdvancedState {
    pub status: Option<StoreStatus>,
    pub form: Option<Entity<PasswordForm>>,
    pub confirmation: Entity<TextInput>,
    pub erase: bool,
    pub busy: bool,
    pub notice: Option<String>,
    success: Option<&'static str>,
    mode_notice_acknowledging: Option<StoreModeNotice>,
    mode_notice_acknowledged: Option<StoreModeNotice>,
    pub remote_enabled: bool,
}

impl AdvancedState {
    pub fn new(cx: &mut Context<SettingsScreen>) -> Self {
        Self {
            status: None,
            form: None,
            confirmation: cx.new(|cx| TextInput::new("Type ERASE", cx)),
            erase: false,
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
        message: String,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.section != Section::Advanced {
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
        self.call(
            async move {
                backend
                    .local_control::<StoreStatus>("store_status", ())
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
                    Err(message) => this.advanced.notice = Some(message),
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

    fn open_password_form(&mut self, action: PasswordAction, cx: &mut Context<Self>) {
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

    fn erase_local_store(&mut self, cx: &mut Context<Self>) {
        if self.advanced.busy || !self.advanced.available() {
            return;
        }
        let confirmation = self.advanced.confirmation.read(cx).text();
        if confirmation != "ERASE" {
            self.advanced.notice = Some("Type ERASE exactly to continue.".into());
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
        self.call(
            async move {
                backend
                    .local_control::<Value>(
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
                    Err(message) => this.advanced.notice = Some(message),
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
        self.call(
            async move {
                backend
                    .local_control::<bool>(
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
                    this.advanced.notice =
                        Some("This notice could not be dismissed. Try again.".into());
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
                            .child("Changes need saving"),
                    )
                    .child(div().text_sm().child(failure.message()))
                    .child(
                        widgets::secondary_button("advanced-retry-store-persistence")
                            .on_click(cx.listener(|_this, _, _, cx| cx.emit(RetryStorePersistence)))
                            .child("Retry saving"),
                    ),
            );
        }
        if let Some(notice) = attention
            && self.advanced.notice.as_deref() != Some(notice.message.as_str())
            && persistence.is_none_or(|failure| failure.message() != notice.message.as_str())
        {
            pane = pane.child(
                local_store_notice_card()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Local storage needs attention"),
                    )
                    .child(div().text_sm().child(notice.message.clone())),
            );
        }
        if let Some(notice) = mode_notice {
            let acknowledging = self.advanced.mode_notice_acknowledging.as_ref() == Some(notice);
            pane = pane.child(
                local_store_notice_card()
                    .child(div().text_sm().child(notice.message.clone()))
                    .child(
                        widgets::ghost_button("advanced-dismiss-mode-notice")
                            .when(acknowledging, |button| button.opacity(0.5))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.dismiss_local_mode_notice(cx)),
                            )
                            .child(if acknowledging {
                                "Dismissing…"
                            } else {
                                "Dismiss"
                            }),
                    ),
            );
        }
        if !form_open
            && let Some(notice) = &self.advanced.notice
            && mode_notice.is_none_or(|mode| mode.message.as_str() != notice.as_str())
            && persistence.is_none_or(|failure| failure.message() != notice.as_str())
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
        let mut pane = div().flex().flex_col().gap_4();
        if !self.advanced.available() {
            pane = self.render_local_store_notices(pane.child(section_title("Local data")), cx);
            if !self.advanced_has_content() {
                pane = pane.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_secondary()))
                        .child("Your local Maple data doesn't need attention."),
                );
            }
            return pane;
        }
        pane = pane.child(section_title("Encryption on this computer"));
        if let Some(form) = &self.advanced.form {
            let pane = self.encryption_context(pane.child(form.clone()), true);
            return self.render_local_store_notices(pane, cx);
        }
        if let Some(success) = self.advanced.success {
            pane = pane.child(
                div()
                    .rounded_md()
                    .p_3()
                    .bg(gpui::rgb(theme::bg_elevated()))
                    .text_color(gpui::rgb(theme::status_success()))
                    .child(success),
            );
        }
        let password = self
            .advanced
            .status
            .as_ref()
            .is_some_and(|status| status.mode == StoreMode::Password);
        pane = pane.child(
            div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(gpui::rgb(if password {
                    theme::status_success()
                } else {
                    theme::text_primary()
                }))
                .child(if password {
                    "Encryption is on"
                } else {
                    "Encryption is off"
                }),
        );
        pane=pane.child(div().text_sm().text_color(gpui::rgb(theme::text_secondary())).child(if password {
            "Maple's copy of your conversation text, settings and sign-in on this computer is encrypted with your unlock password. Maple asks for it each time it starts. Attached files and images remain unencrypted."
        } else {"Your conversation text, settings and sign-in are stored on this computer, readable only by your user account. Set an unlock password to encrypt them. Attached files and images remain unencrypted."}));
        if self.advanced.erase {
            pane = pane
                .child(div().text_sm().child(ERASE_COPY))
                .child(self.advanced.confirmation.clone())
                .child(
                    widgets::primary_button("advanced-erase-confirm")
                        .when(self.advanced.busy, |button| button.opacity(0.5))
                        .on_click(
                            cx.listener(|this, _event, _window, cx| this.erase_local_store(cx)),
                        )
                        .child("Erase this version's local data"),
                )
                .child(
                    widgets::ghost_button("advanced-erase-cancel")
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.advanced.erase = false;
                            cx.notify();
                        }))
                        .child("Cancel"),
                );
        } else {
            pane = pane
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            widgets::primary_button("advanced-password")
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
                                    "Change password…"
                                } else {
                                    "Set unlock password…"
                                }),
                        )
                        .when(password, |row| {
                            row.child(
                                widgets::secondary_button("advanced-remove-password")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.open_password_form(PasswordAction::Remove, cx)
                                    }))
                                    .child("Remove password…"),
                            )
                        }),
                )
                .child(
                    widgets::ghost_button("advanced-erase")
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.advanced.erase = true;
                            this.advanced.notice = None;
                            this.advanced.success = None;
                            cx.notify();
                        }))
                        .child("Erase local data…"),
                );
        }
        pane = self.encryption_context(pane, password);
        self.render_local_store_notices(pane, cx)
    }

    fn encryption_context(&self, mut pane: Div, password: bool) -> Div {
        if self.advanced.remote_enabled {
            pane=pane.child(div().text_sm().child("While Maple is locked, your other computers can't reach this one. After a restart, open Maple and unlock it to turn remote access back on."));
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
                    "claude" => Some(
                        "Claude Code keeps its own copy of the work Maple hands it, in its own folder, and Maple's password doesn't encrypt it.",
                    ),
                    "codex" => Some(
                        "Codex keeps its own copy of the work Maple hands it, in its own folder, and Maple's password doesn't encrypt it.",
                    ),
                    _ => None,
                };
                if let Some(copy) = copy {
                    pane = pane.child(div().text_sm().child(copy));
                }
            }
        }
        pane
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
        PasswordAction::Set | PasswordAction::Setup => "Encryption enabled on this computer.",
        PasswordAction::Change => "Your unlock password was changed.",
        PasswordAction::Remove => "Your unlock password was removed.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            assert!(!settings.show_local_store_notice("Needs attention.".into(), cx));
            assert!(settings.advanced.notice.is_none());
            settings.section = Section::Advanced;
            assert!(settings.show_local_store_notice("Needs attention.".into(), cx));
            assert_eq!(
                settings.advanced.notice.as_deref(),
                Some("Needs attention.")
            );
        });
    }
    #[gpui::test]
    fn privacy_and_security_is_listed_only_with_something_to_show(cx: &mut gpui::TestAppContext) {
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
            assert!(!settings.visible_sections().contains(&Section::Advanced));
            settings.advanced.notice = Some("Needs attention.".into());
            assert!(settings.visible_sections().contains(&Section::Advanced));
            settings.advanced.notice = None;
            settings.section = Section::Advanced;
            assert!(settings.visible_sections().contains(&Section::Advanced));
            settings.section = Section::General;
            settings.advanced.status = Some(
                serde_json::from_value(serde_json::json!({
                    "state": {"state": "open"},
                    "mode": "plain",
                    "password_settings_available": true,
                }))
                .unwrap(),
            );
            assert!(settings.visible_sections().contains(&Section::Advanced));
        });
    }
    #[test]
    fn erase_copy_names_only_this_version_and_preserves_previous_data() {
        assert!(ERASE_COPY.contains("this version's store"));
        assert!(ERASE_COPY.contains("previous app data remain untouched"));
        assert!(ERASE_COPY.contains("remote pairings"));
    }
}
