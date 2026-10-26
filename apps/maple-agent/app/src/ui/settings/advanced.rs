//! Local advanced preferences and capability-gated encryption controls. A
//! remote task selection never redirects these settings to another computer.

use gpui::{App, Context, Div, Entity, EventEmitter, div, prelude::*};
#[cfg(feature = "acp")]
use maple_agent::acp::AgentAcpConfig;
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
    #[cfg(feature = "acp")]
    acp: AcpSettings,
}

#[cfg(feature = "acp")]
struct AcpSettings {
    config: Option<AgentAcpConfig>,
    roots: Entity<TextInput>,
    connection_limit: Entity<TextInput>,
    saving: bool,
    notice: Option<String>,
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
            #[cfg(feature = "acp")]
            acp: AcpSettings {
                config: None,
                roots: cx.new(|cx| {
                    TextInput::new("One absolute folder path per line", cx)
                        .multiline(6)
                        .with_tab_index(30)
                }),
                connection_limit: cx.new(|cx| {
                    TextInput::new(
                        &format!("Default ({})", AgentAcpConfig::default().max_connections),
                        cx,
                    )
                    .with_tab_index(31)
                }),
                saving: false,
                notice: None,
            },
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

    pub(crate) fn refresh_encryption_status(&self, cx: &mut Context<Self>) {
        self.load_advanced_status(cx);
    }

    pub(super) fn load_advanced_status(&self, cx: &mut Context<Self>) {
        #[cfg(feature = "acp")]
        if self.advanced.acp.config.is_none() {
            self.load_acp_settings(cx);
        }
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

    #[cfg(feature = "acp")]
    fn load_acp_settings(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.get_acp_config(&user_id).await },
            cx,
            |this, result, cx| {
                // Repeated status/retry reads may overlap. Once the editor is
                // available, a late read must not overwrite an in-progress draft.
                if this.advanced.acp.config.is_some() {
                    return;
                }
                match result {
                    Ok(config) => {
                        this.install_acp_fields(&config, cx);
                        this.advanced.acp.config = Some(config);
                        this.advanced.acp.notice = None;
                    }
                    Err(message) => this.advanced.acp.notice = Some(message),
                }
                cx.notify();
            },
        );
    }

    #[cfg(feature = "acp")]
    fn install_acp_fields(&mut self, config: &AgentAcpConfig, cx: &mut Context<Self>) {
        self.advanced.acp.roots.update(cx, |input, cx| {
            input.set_text(&config.allowed_project_roots.join("\n"), cx);
        });
        let limit = if config.max_connections == AgentAcpConfig::default().max_connections {
            String::new()
        } else {
            config.max_connections.to_string()
        };
        self.advanced
            .acp
            .connection_limit
            .update(cx, |input, cx| input.set_text(&limit, cx));
    }

    #[cfg(feature = "acp")]
    fn save_acp_settings(&mut self, cx: &mut Context<Self>) {
        if self.advanced.acp.saving || self.advanced.acp.config.is_none() {
            return;
        }
        let config = match acp_form_config(
            &self.advanced.acp.roots.read(cx).text(),
            &self.advanced.acp.connection_limit.read(cx).text(),
        ) {
            Ok(config) => config,
            Err(message) => {
                self.advanced.acp.notice = Some(message);
                cx.notify();
                return;
            }
        };
        self.advanced.acp.saving = true;
        self.advanced.acp.notice = None;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.set_acp_config(&user_id, config).await },
            cx,
            |this, result, cx| {
                this.advanced.acp.saving = false;
                match result {
                    Ok(config) => {
                        // The host validates again and returns the stored values.
                        this.install_acp_fields(&config, cx);
                        this.advanced.acp.config = Some(config);
                        this.advanced.acp.notice =
                            Some("ACP preferences saved. They apply to new connections.".into());
                    }
                    Err(message) => this.advanced.acp.notice = Some(message),
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    #[cfg(feature = "acp")]
    fn reset_acp_fields(&mut self, cx: &mut Context<Self>) {
        if self.advanced.acp.saving || self.advanced.acp.config.is_none() {
            return;
        }
        self.install_acp_fields(&AgentAcpConfig::default(), cx);
        self.advanced.acp.notice = Some("Default values are ready. Save to apply them.".into());
        cx.notify();
    }

    #[cfg(feature = "acp")]
    fn render_acp_settings(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.advanced.acp;
        let mut pane = div().flex().flex_col().gap_3()
            .child(section_title("Agent Client Protocol (ACP)"))
            .child(div().text_sm().text_color(gpui::rgb(theme::text_secondary()))
                .child("Choose which folders local ACP clients can open and how many connections can run at once. These preferences apply to this account on this computer."));
        if state.saving {
            return pane.child(div().text_sm().child("Saving ACP preferences…"));
        }
        if state.config.is_some() {
            pane = pane
                .child(div().text_sm().child("Allowed project folders"))
                .child(widgets::input_frame().child(state.roots.clone()))
                .child(div().text_sm().text_color(gpui::rgb(theme::text_secondary()))
                    .child("Enter one absolute folder path per line. Leave the list empty to allow any folder."))
                .child(div().text_sm().child("Maximum connections"))
                .child(widgets::input_frame().child(state.connection_limit.clone()))
                .child(div().text_sm().text_color(gpui::rgb(theme::text_secondary()))
                    .child(format!("Choose 1–{} simultaneous connections. Leave blank for the default ({}).", AgentAcpConfig::default().max_connections, AgentAcpConfig::default().max_connections)))
                .child(div().flex().gap_2()
                    .child(widgets::primary_button("advanced-acp-save")
                        .on_click(cx.listener(|this, _, _, cx| this.save_acp_settings(cx))).child("Save ACP preferences"))
                    .child(widgets::secondary_button("advanced-acp-defaults")
                        .on_click(cx.listener(|this, _, _, cx| this.reset_acp_fields(cx))).child("Use defaults")));
        } else if state.notice.is_some() {
            pane = pane.child(
                widgets::secondary_button("advanced-acp-retry")
                    .on_click(cx.listener(|this, _, _, cx| this.load_acp_settings(cx)))
                    .child("Retry"),
            );
        } else {
            pane = pane.child(div().text_sm().child("Loading ACP preferences…"));
        }
        if let Some(notice) = &state.notice {
            pane = pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::status_warning()))
                    .child(notice.clone()),
            );
        }
        pane
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
            #[cfg(feature = "acp")]
            {
                pane = pane.child(self.render_acp_settings(cx));
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
        pane = self.render_local_store_notices(pane, cx);
        #[cfg(feature = "acp")]
        {
            pane = pane.child(self.render_acp_settings(cx));
        }
        pane
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

#[cfg(feature = "acp")]
fn acp_form_config(roots: &str, connection_limit: &str) -> Result<AgentAcpConfig, String> {
    let defaults = AgentAcpConfig::default();
    let limit = connection_limit.trim();
    let max_connections = if limit.is_empty() {
        defaults.max_connections
    } else {
        limit
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=defaults.max_connections).contains(limit))
            .ok_or_else(|| {
                format!(
                    "Choose a connection limit from 1 to {}.",
                    defaults.max_connections
                )
            })?
    };
    let mut allowed_project_roots = Vec::new();
    for root in roots.lines().map(str::trim).filter(|root| !root.is_empty()) {
        if !std::path::Path::new(root).is_absolute() {
            return Err("Allowed ACP folders must use absolute paths.".into());
        }
        if !allowed_project_roots.iter().any(|saved| saved == root) {
            allowed_project_roots.push(root.to_owned());
        }
    }
    Ok(AgentAcpConfig {
        allowed_project_roots,
        max_connections,
    })
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
    #[test]
    fn erase_copy_names_only_this_version_and_preserves_previous_data() {
        assert!(ERASE_COPY.contains("this version's store"));
        assert!(ERASE_COPY.contains("previous app data remain untouched"));
        assert!(ERASE_COPY.contains("remote pairings"));
    }
    #[cfg(feature = "acp")]
    #[test]
    fn empty_acp_fields_preserve_default_semantics() {
        assert_eq!(
            acp_form_config("\n  \n", " ").unwrap(),
            AgentAcpConfig::default()
        );
    }
    #[cfg(feature = "acp")]
    #[test]
    fn acp_fields_validate_roots_and_limits_before_submitting() {
        let first = std::env::temp_dir()
            .join("maple-acp-ui-fixture-one")
            .to_string_lossy()
            .into_owned();
        let second = std::env::temp_dir()
            .join("maple-acp-ui-fixture-two")
            .to_string_lossy()
            .into_owned();
        let config = acp_form_config(&format!(" {first} \n{second}\n{first}\n"), "1").unwrap();
        assert_eq!(config.allowed_project_roots, vec![first, second]);
        assert_eq!(config.max_connections, 1);
        assert!(acp_form_config("relative/folder", "8").is_err());
        for limit in ["0", "9", "-1", "one"] {
            assert!(acp_form_config("", limit).is_err());
        }
    }
}
