//! This computer's optional HTTP inference listener. Account keys are created
//! in the existing API keys pane and supplied by each calling application.

use gpui::{Context, Div, Entity, Focusable, SharedString, Window, div, prelude::*};

use super::{Section, SettingsScreen, SettingsTarget, section_title, setting_copy};
use crate::local_api::{LocalApiConfig, LocalApiState, LocalApiStatus};
use crate::ui::{text_input::TextInput, theme, widgets};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LocalApiTarget {
    Enabled,
    Retry,
    Port,
    SavePort,
    CopyUrl,
    ApiKeys,
    Refresh,
}

pub(super) struct LocalApiUiState {
    status: Option<LocalApiStatus>,
    pub(super) port: Entity<TextInput>,
    base_url: SharedString,
    request_count: SharedString,
    saving: bool,
    refreshing: bool,
    host_epoch: u64,
    refresh_generation: u64,
    notice: Option<&'static str>,
}

impl LocalApiUiState {
    pub(super) fn new(
        application_vim_enabled: bool,
        application_focus: gpui::FocusHandle,
        cx: &mut Context<SettingsScreen>,
    ) -> Self {
        Self {
            status: None,
            port: cx.new(move |cx| {
                TextInput::new("Port (1–65535)", cx)
                    .with_tab_index(19)
                    .application_vim(application_vim_enabled)
                    .on_application_escape(move |window, cx| window.focus(&application_focus, cx))
            }),
            base_url: SharedString::default(),
            request_count: SharedString::default(),
            saving: false,
            refreshing: false,
            host_epoch: 0,
            refresh_generation: 0,
            notice: None,
        }
    }

    fn can_configure(&self) -> bool {
        self.status.is_some() && !self.saving
    }

    fn can_edit_port(&self) -> bool {
        self.can_configure()
            && self.status.as_ref().is_some_and(|status| {
                matches!(status.state, LocalApiState::Stopped | LocalApiState::Failed)
            })
    }

    fn can_retry(&self) -> bool {
        self.can_configure()
            && self.status.as_ref().is_some_and(|status| {
                status.config.enabled
                    && matches!(status.state, LocalApiState::Stopped | LocalApiState::Failed)
            })
    }
}

impl SettingsScreen {
    #[cfg(test)]
    pub(crate) fn local_api_status_for_test(&self) -> Option<LocalApiStatus> {
        self.local_api.status.clone()
    }

    /// Called only with this computer's host status. A selected remote agent
    /// never changes the backend used by this pane.
    pub(crate) fn set_local_api_status(&mut self, status: LocalApiStatus, cx: &mut Context<Self>) {
        if self
            .local_api
            .status
            .as_ref()
            .is_some_and(|current| status.revision < current.revision)
        {
            return;
        }
        let port_changed = self
            .local_api
            .status
            .as_ref()
            .is_none_or(|current| current.config.port != status.config.port);
        self.local_api.base_url = format!("http://127.0.0.1:{}/v1", status.config.port).into();
        self.local_api.request_count = format!("{} active requests", status.active_requests).into();
        self.local_api.status = Some(status);
        if port_changed {
            let port = self
                .local_api
                .status
                .as_ref()
                .unwrap()
                .config
                .port
                .to_string();
            self.local_api
                .port
                .update(cx, |input, cx| input.set_text(&port, cx));
        }
        self.reconcile_application_vim_target();
        cx.notify();
    }

    /// Refresh on pane entry and after reconnecting to the local host.
    pub(crate) fn refresh_local_api_status(&mut self, cx: &mut Context<Self>) {
        self.local_api.refreshing = true;
        self.local_api.notice = None;
        self.local_api.refresh_generation = self.local_api.refresh_generation.wrapping_add(1);
        let generation = self.local_api.refresh_generation;
        let host_epoch = self.local_api.host_epoch;
        let backend = self.backend.clone();
        self.call(
            async move { backend.get_local_api_status().await },
            cx,
            move |this, result, cx| {
                if host_epoch != this.local_api.host_epoch
                    || generation != this.local_api.refresh_generation
                {
                    return;
                }
                this.local_api.refreshing = false;
                match result {
                    Ok(status) => {
                        this.set_local_api_status(status, cx);
                    }
                    Err(_) => {
                        this.local_api.notice = Some(
                            "Could not read the local API status. Reconnect to this computer and try again.",
                        );
                    }
                }
                this.reconcile_application_vim_target();
                cx.notify();
            },
        );
        self.reconcile_application_vim_target();
        cx.notify();
    }

    /// A replacement host starts a new revision sequence. Old requests must
    /// neither install a snapshot nor clear a newer host's pending operation.
    pub(crate) fn local_api_host_reconnected(&mut self, cx: &mut Context<Self>) {
        self.local_api.host_epoch = self.local_api.host_epoch.wrapping_add(1);
        self.local_api.status = None;
        self.local_api.saving = false;
        self.refresh_local_api_status(cx);
    }

    fn toggle_local_api(&mut self, cx: &mut Context<Self>) {
        if !self.local_api.can_configure() {
            return;
        }
        let Some(status) = &self.local_api.status else {
            return;
        };
        let enabled = !status.config.enabled;
        let port = if enabled {
            match parse_port(self.local_api.port.read(cx).text_ref()) {
                Ok(port) => port,
                Err(message) => {
                    self.local_api.notice = Some(message);
                    cx.notify();
                    return;
                }
            }
        } else {
            status.config.port
        };
        self.save_local_api_config(LocalApiConfig { enabled, port }, cx);
    }

    fn save_local_api_port(&mut self, cx: &mut Context<Self>) {
        if !self.local_api.can_edit_port() {
            return;
        }
        let port = match parse_port(self.local_api.port.read(cx).text_ref()) {
            Ok(port) => port,
            Err(message) => {
                self.local_api.notice = Some(message);
                cx.notify();
                return;
            }
        };
        let enabled = self.local_api.status.as_ref().unwrap().config.enabled;
        self.save_local_api_config(LocalApiConfig { enabled, port }, cx);
    }

    fn retry_local_api(&mut self, cx: &mut Context<Self>) {
        if self.local_api.can_retry() {
            let config = self.local_api.status.as_ref().unwrap().config.clone();
            self.save_local_api_config(config, cx);
        }
    }

    fn save_local_api_config(&mut self, config: LocalApiConfig, cx: &mut Context<Self>) {
        self.local_api.saving = true;
        self.local_api.notice = None;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let host_epoch = self.local_api.host_epoch;
        self.call(
            async move { backend.set_local_api_config(&user_id, config).await },
            cx,
            move |this, result, cx| {
                if host_epoch != this.local_api.host_epoch {
                    return;
                }
                this.local_api.saving = false;
                match result {
                    Ok(status) => this.set_local_api_status(status, cx),
                    Err(_) => {
                        this.refresh_local_api_status(cx);
                        this.local_api.notice = Some(
                            "Could not update the local API. Refresh its status and try again.",
                        );
                    }
                }
                this.reconcile_application_vim_target();
                cx.notify();
            },
        );
        self.reconcile_application_vim_target();
        cx.notify();
    }

    pub(super) fn local_api_targets(&self) -> Vec<SettingsTarget> {
        let target = SettingsTarget::LocalApi;
        let mut targets = Vec::new();
        if self.local_api.can_configure() {
            targets.push(target(LocalApiTarget::Enabled));
        }
        if self.local_api.can_retry() {
            targets.push(target(LocalApiTarget::Retry));
        }
        if self.local_api.can_edit_port() {
            targets.extend([
                target(LocalApiTarget::Port),
                target(LocalApiTarget::SavePort),
            ]);
        }
        if self.local_api.status.is_some() {
            targets.push(target(LocalApiTarget::CopyUrl));
        }
        targets.push(target(LocalApiTarget::ApiKeys));
        if !self.local_api.refreshing && !self.local_api.saving {
            targets.push(target(LocalApiTarget::Refresh));
        }
        targets
    }

    pub(super) fn activate_local_api_target(
        &mut self,
        target: LocalApiTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            LocalApiTarget::Enabled => self.toggle_local_api(cx),
            LocalApiTarget::Retry => self.retry_local_api(cx),
            LocalApiTarget::Port if self.local_api.can_edit_port() => {
                let focus = self.local_api.port.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
            LocalApiTarget::Port => {}
            LocalApiTarget::SavePort => self.save_local_api_port(cx),
            LocalApiTarget::CopyUrl => self.copy_local_api_url(cx),
            LocalApiTarget::ApiKeys => self.select_section(Section::ApiKeys, cx),
            LocalApiTarget::Refresh => self.refresh_local_api_status(cx),
        }
    }

    fn copy_local_api_url(&self, cx: &mut Context<Self>) {
        if self.local_api.status.is_some() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                self.local_api.base_url.to_string(),
            ));
        }
    }

    pub(super) fn render_local_api_pane(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.local_api;
        let status = state.status.as_ref();
        let enabled = status.is_some_and(|status| status.config.enabled);
        let label = if state.refreshing && status.is_none() {
            "Checking status…"
        } else {
            status.map_or("Status unavailable", state_label)
        };
        let mut pane = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title("Local API"))
            .child(setting_copy(
                "Connect AI applications on this computer",
                "Expose Maple's inference APIs at a local address. Requests use Maple's secure backend and your API credits.",
            ))
            .child(
                widgets::card_row()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(setting_copy(
                        "Enable local API",
                        "Applies to this computer, including while you work with a remote agent.",
                    ))
                    .child(self.application_target(
                        || SettingsTarget::LocalApi(LocalApiTarget::Enabled),
                        widgets::switch("local-api-enabled", enabled)
                            .debug_selector(|| "local-api-enabled".to_string())
                            .aria_label("Enable local API on this computer")
                            .when(!state.can_configure(), |switch| {
                                switch.opacity(0.5).aria_description(
                                    "Unavailable while local API settings are loading or being updated.",
                                )
                            })
                            .when(state.can_configure(), |switch| {
                                switch
                                    .tab_index(18)
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_local_api(cx)))
                                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                        if activates(event) {
                                            cx.stop_propagation();
                                            this.toggle_local_api(cx);
                                        }
                                    }))
                            }),
                    )),
            )
            .child(
                div()
                    .id("local-api-status")
                    .role(gpui::Role::Status)
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(label)
                    .when(state.saving, |row| row.child("Updating local API settings…"))
                    .when(status.is_some_and(|status| status.state == LocalApiState::Running), |row| {
                        row.child(state.request_count.clone())
                    })
                    .when(status.is_some_and(|status| status.state == LocalApiState::Stopped && status.config.enabled), |row| {
                        row.child("The local API is enabled but paused. Select Resume to accept requests on this computer.")
                    }),
            );
        if let Some(message) = state.notice {
            pane = pane.child(widgets::banner(theme::status_error()).child(message));
        }
        if let Some(error) = status.and_then(|status| status.error.as_ref()) {
            pane = pane.child(widgets::banner(theme::status_error()).child(error.clone()));
        }
        if state.can_retry() {
            pane = pane.child(
                self.application_target(
                    || SettingsTarget::LocalApi(LocalApiTarget::Retry),
                    widgets::secondary_button("local-api-retry")
                        .tab_index(19)
                        .on_click(cx.listener(|this, _, _, cx| this.retry_local_api(cx)))
                        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                            if activates(event) {
                                cx.stop_propagation();
                                this.retry_local_api(cx);
                            }
                        }))
                        .child(retry_label(status.expect("retry requires a status"))),
                ),
            );
        }
        if let Some(status) = status {
            let port_control = if state.can_edit_port() {
                self.application_target(
                    || SettingsTarget::LocalApi(LocalApiTarget::Port),
                    widgets::input_frame()
                        .w(gpui::px(120.))
                        .child(state.port.clone()),
                )
            } else {
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(status.config.port.to_string())
                    .into_any_element()
            };
            pane = pane
                .child(
                    widgets::card_row()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(setting_copy("Port", "The local address always uses 127.0.0.1. Stop the API to change its port."))
                        .child(div().flex().items_center().gap_2().child(port_control).when(state.can_edit_port(), |row| {
                            row.child(self.application_target(
                                || SettingsTarget::LocalApi(LocalApiTarget::SavePort),
                                widgets::secondary_button("local-api-save-port")
                                    .tab_index(20)
                                    .on_click(cx.listener(|this, _, _, cx| this.save_local_api_port(cx)))
                                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                        if activates(event) {
                                            cx.stop_propagation();
                                            this.save_local_api_port(cx);
                                        }
                                    }))
                                    .child(if !enabled {
                                        "Save port"
                                    } else if status.state == LocalApiState::Stopped {
                                        "Save and resume"
                                    } else {
                                        "Save and retry"
                                    }),
                            ))
                        })),
                )
                .child(
                    widgets::card_row()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(
                            div().flex().flex_col().gap_1()
                                .child(setting_copy("OpenAI-compatible base URL", "Paste this address into your AI application's provider settings."))
                                .child(div().font_family(crate::assets::FONT_MONO).text_sm().child(state.base_url.clone())),
                        )
                        .child(self.application_target(
                            || SettingsTarget::LocalApi(LocalApiTarget::CopyUrl),
                            widgets::copy_button("local-api-copy-url", state.base_url.clone(), None, Some(cx.entity_id()))
                                .aria_label("Copy local API base URL")
                                .tab_index(21)
                                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                    if activates(event) {
                                        cx.stop_propagation();
                                        this.copy_local_api_url(cx);
                                    }
                                })),
                        )),
                );
        }
        pane.child(
            div().flex().flex_col().gap_2().text_sm().text_color(gpui::rgb(theme::text_secondary()))
                .child("When enabled, the API resumes when this computer's Maple host starts and its store is unlocked. Signing out, locking, or quitting the host stops the listener.")
                .child("Create a dedicated Maple API key, copy it once, and enter it in your AI application's API key field. Every request must supply that key as Authorization: Bearer <key>.")
                .child(self.application_target(
                    || SettingsTarget::LocalApi(LocalApiTarget::ApiKeys),
                    widgets::secondary_button("local-api-open-keys")
                        .tab_index(22)
                        .on_click(cx.listener(|this, _, _, cx| this.select_section(Section::ApiKeys, cx)))
                        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                            if activates(event) {
                                cx.stop_propagation();
                                this.select_section(Section::ApiKeys, cx);
                            }
                        }))
                        .child("Manage API keys"),
                )),
        ).child(
            widgets::card_row().flex().flex_col().gap_2()
                .child(section_title("Available endpoints"))
                .children([
                    "GET /v1/models",
                    "POST /v1/chat/completions",
                    "POST /v1/embeddings",
                    "POST /v1/systemone",
                ].into_iter().map(|route| div().font_family(crate::assets::FONT_MONO).text_xs().child(route)))
                .child(div().text_xs().text_color(gpui::rgb(theme::text_muted())).child("Browser-origin requests are blocked. Use a desktop application, CLI, or server client.")),
        ).when(!state.refreshing && !state.saving, |pane| {
            pane.child(self.application_target(
                || SettingsTarget::LocalApi(LocalApiTarget::Refresh),
                widgets::ghost_button("local-api-refresh")
                    .tab_index(23)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_local_api_status(cx)))
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                        if activates(event) {
                            cx.stop_propagation();
                            this.refresh_local_api_status(cx);
                        }
                    }))
                    .child("Refresh status"),
            ))
        })
    }
}

fn parse_port(value: &str) -> Result<u16, &'static str> {
    value
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or("Enter a port between 1 and 65535.")
}

fn state_label(status: &LocalApiStatus) -> &'static str {
    match status.state {
        LocalApiState::Stopped if status.config.enabled => "Paused",
        LocalApiState::Stopped => "Stopped",
        LocalApiState::Starting => "Starting…",
        LocalApiState::Running => "Running on this computer",
        LocalApiState::Stopping => "Stopping…",
        LocalApiState::Failed => "Could not start",
    }
}

fn retry_label(status: &LocalApiStatus) -> &'static str {
    if status.state == LocalApiState::Stopped {
        "Resume"
    } else {
        "Retry start"
    }
}

fn activates(event: &gpui::KeyDownEvent) -> bool {
    !event.keystroke.modifiers.modified()
        && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    fn screen(cx: &mut TestAppContext) -> Entity<SettingsScreen> {
        cx.executor().allow_parking();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                .expect("test backend"),
        );
        cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
                backend,
                "user".into(),
                crate::settings::AppSettings {
                    application_vim_enabled: true,
                    ..Default::default()
                },
                crate::shortcuts::ShortcutSnapshot {
                    generation: 1,
                    rows: Vec::new(),
                    last_error: None,
                    compatibility_warning: None,
                },
                Section::LocalApi,
                cx,
            )
        })
    }

    fn status(revision: u64, state: LocalApiState, enabled: bool, port: u16) -> LocalApiStatus {
        LocalApiStatus {
            config: LocalApiConfig { enabled, port },
            state,
            owner: Some("user".into()),
            revision,
            ..Default::default()
        }
    }

    #[test]
    fn invalid_ports_cannot_enable_an_ephemeral_or_wrapped_listener() {
        for value in ["", "0", "65536", "-1", "1.5", "localhost", "8080/tcp"] {
            assert!(parse_port(value).is_err(), "{value:?}");
        }
        assert_eq!(parse_port("1"), Ok(1));
        assert_eq!(parse_port(" 65535 "), Ok(65535));
    }

    #[gpui::test]
    fn pushed_status_rejects_stale_replies_and_preserves_unsaved_port(cx: &mut TestAppContext) {
        let settings = screen(cx);
        settings.update(cx, |this, cx| {
            this.local_api.refreshing = false;
            this.set_local_api_status(status(100, LocalApiState::Stopped, false, 8123), cx);
            this.local_api
                .port
                .update(cx, |input, cx| input.set_text("9123", cx));
            this.set_local_api_status(status(101, LocalApiState::Running, true, 8123), cx);
            assert_eq!(this.local_api.port.read(cx).text_ref(), "9123");
            assert_eq!(this.local_api.base_url.as_ref(), "http://127.0.0.1:8123/v1");

            // A late stopped snapshot must not resurrect old configuration or
            // re-enable port editing after the listener started.
            this.set_local_api_status(status(99, LocalApiState::Stopped, false, 7123), cx);
            assert_eq!(
                this.local_api.status.as_ref().unwrap().state,
                LocalApiState::Running
            );
            assert!(!this.local_api.can_edit_port());
            assert_eq!(this.local_api.base_url.as_ref(), "http://127.0.0.1:8123/v1");

            this.set_local_api_status(status(102, LocalApiState::Stopped, false, 9123), cx);
            assert_eq!(this.local_api.port.read(cx).text_ref(), "9123");
            assert_eq!(this.local_api.base_url.as_ref(), "http://127.0.0.1:9123/v1");
            assert!(this.local_api.can_edit_port());
        });
    }

    #[gpui::test]
    fn keyboard_targets_follow_listener_state_and_retry_availability(cx: &mut TestAppContext) {
        let settings = screen(cx);
        settings.update(cx, |this, cx| {
            this.local_api.refreshing = false;
            this.set_local_api_status(status(100, LocalApiState::Stopped, false, 8123), cx);
            let target = SettingsTarget::LocalApi;
            assert!(
                this.local_api_targets()
                    .contains(&target(LocalApiTarget::Port))
            );
            assert!(
                !this
                    .local_api_targets()
                    .contains(&target(LocalApiTarget::Retry))
            );
            for (revision, state) in [
                LocalApiState::Starting,
                LocalApiState::Running,
                LocalApiState::Stopping,
            ]
            .into_iter()
            .enumerate()
            {
                this.set_local_api_status(status(101 + revision as u64, state, true, 8123), cx);
                assert!(
                    !this
                        .local_api_targets()
                        .contains(&target(LocalApiTarget::Port))
                );
                assert!(
                    !this
                        .local_api_targets()
                        .contains(&target(LocalApiTarget::SavePort))
                );
            }
            this.set_local_api_status(status(104, LocalApiState::Failed, true, 8123), cx);
            assert!(
                this.local_api_targets()
                    .contains(&target(LocalApiTarget::Retry))
            );
            this.local_api.saving = true;
            assert_eq!(
                this.local_api_targets(),
                vec![
                    target(LocalApiTarget::CopyUrl),
                    target(LocalApiTarget::ApiKeys),
                ]
            );
        });
    }

    #[gpui::test]
    fn invalid_typed_port_is_rejected_before_changing_enabled_state(cx: &mut TestAppContext) {
        let settings = screen(cx);
        settings.update(cx, |this, cx| {
            this.local_api.refreshing = false;
            this.set_local_api_status(status(100, LocalApiState::Stopped, false, 8123), cx);
            this.local_api
                .port
                .update(cx, |input, cx| input.set_text("0", cx));
            this.toggle_local_api(cx);
            assert!(!this.local_api.saving);
            assert!(!this.local_api.status.as_ref().unwrap().config.enabled);
            assert_eq!(
                this.local_api.notice,
                Some("Enter a port between 1 and 65535.")
            );
        });
    }

    #[gpui::test]
    fn enabled_but_stopped_api_offers_explicit_resume(cx: &mut TestAppContext) {
        let settings = screen(cx);
        settings.update(cx, |this, cx| {
            this.local_api.refreshing = false;
            this.set_local_api_status(status(100, LocalApiState::Stopped, true, 8123), cx);
            let snapshot = this.local_api.status.as_ref().unwrap();
            assert_eq!(state_label(snapshot), "Paused");
            assert_eq!(retry_label(snapshot), "Resume");
            assert!(
                this.local_api_targets()
                    .contains(&SettingsTarget::LocalApi(LocalApiTarget::Retry))
            );

            this.set_local_api_status(status(101, LocalApiState::Failed, true, 8123), cx);
            assert_eq!(
                retry_label(this.local_api.status.as_ref().unwrap()),
                "Retry start"
            );

            this.set_local_api_status(status(102, LocalApiState::Stopped, false, 8123), cx);
            assert_eq!(
                state_label(this.local_api.status.as_ref().unwrap()),
                "Stopped"
            );
            assert!(!this.local_api.can_retry());
        });
    }
}
