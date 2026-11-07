//! This computer's optional HTTP inference listener, as a card on the API
//! pane. Each calling application supplies one of the account keys listed
//! below it.

use crate::ui::i18n::{UiText, tr};
use gpui::{Context, Div, Entity, Focusable, SharedString, Window, div, prelude::*};

use super::api_keys::tab_order;
use super::{SettingsScreen, SettingsTarget, setting_copy};
use crate::local_api::{LocalApiConfig, LocalApiState, LocalApiStatus};
use crate::ui::{text_input::TextInput, theme, widgets};

const INVALID_PORT: &str = "settings-local-api-enter-a-port-between-1-and-65535";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LocalApiTarget {
    Enabled,
    Retry,
    Port,
    SavePort,
    CopyUrl,
    Refresh,
}

pub(super) struct LocalApiUiState {
    status: Option<LocalApiStatus>,
    pub(super) port: Entity<TextInput>,
    /// The port field, parsed when it changes.
    typed_port: Result<u16, &'static str>,
    base_url: SharedString,
    /// The line under the card's title for the latest status.
    detail: UiText,
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
        let port = cx.new(move |cx| {
            TextInput::localized("settings-local-api-port-1-65535", cx)
                .with_tab_index(tab_order::LOCAL_PORT)
                .application_vim(application_vim_enabled)
                .on_application_escape(move |window, cx| window.focus(&application_focus, cx))
        });
        cx.observe(&port, |this, input, cx| {
            let typed = parse_port(input.read(cx).text_ref());
            if this.local_api.typed_port != typed {
                this.local_api.typed_port = typed;
                this.reconcile_application_vim_target();
                cx.notify();
            }
        })
        .detach();
        Self {
            status: None,
            port,
            typed_port: Err(INVALID_PORT),
            base_url: SharedString::default(),
            detail: UiText::key(OFF_DETAIL),
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

    /// Save appears once the field holds a different valid port.
    fn can_save_port(&self) -> bool {
        self.can_edit_port()
            && self.typed_port.is_ok_and(|port| {
                self.status
                    .as_ref()
                    .is_some_and(|status| status.config.port != port)
            })
    }

    fn can_retry(&self) -> bool {
        self.can_configure()
            && self.status.as_ref().is_some_and(|status| {
                status.config.enabled
                    && matches!(status.state, LocalApiState::Stopped | LocalApiState::Failed)
            })
    }

    /// Status is pushed, so refreshing by hand is only for recovering from a
    /// read or update that failed.
    fn can_refresh(&self) -> bool {
        !self.refreshing && !self.saving && (self.status.is_none() || self.notice.is_some())
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
        self.local_api.detail = state_detail(&status);
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
            self.local_api.typed_port = parse_port(&port);
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
                            "settings-local-api-could-not-read-the-local-api-status-reconnect-to-this-computer-an",
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
                    self.local_api.typed_port = Err(message);
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
                self.local_api.typed_port = Err(message);
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
                            "settings-local-api-could-not-update-the-local-api-refresh-its-status-and-try-again",
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

    /// The card's controls, in the order they are drawn.
    pub(super) fn local_api_targets(&self) -> Vec<SettingsTarget> {
        let target = SettingsTarget::LocalApi;
        let mut targets = Vec::new();
        if self.local_api.can_retry() {
            targets.push(target(LocalApiTarget::Retry));
        }
        if self.local_api.can_configure() {
            targets.push(target(LocalApiTarget::Enabled));
        }
        if self.local_api.can_refresh() {
            targets.push(target(LocalApiTarget::Refresh));
        }
        if self.local_api.status.is_some() {
            targets.push(target(LocalApiTarget::CopyUrl));
        }
        if self.local_api.can_save_port() {
            targets.push(target(LocalApiTarget::SavePort));
        }
        if self.local_api.can_edit_port() {
            targets.push(target(LocalApiTarget::Port));
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
            LocalApiTarget::Refresh => self.refresh_local_api_status(cx),
        }
    }

    fn copy_local_api_url(&mut self, cx: &mut Context<Self>) {
        if self.local_api.status.is_some() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                self.local_api.base_url.to_string(),
            ));
        }
    }

    pub(super) fn render_local_api_card(&self, cx: &mut Context<Self>) -> Div {
        let mut rows = vec![self.render_local_api_header(cx).into_any_element()];
        if let Some(status) = &self.local_api.status {
            rows.push(self.render_local_api_url(cx).into_any_element());
            rows.push(self.render_local_api_port(status, cx).into_any_element());
        }
        rows.push(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(tr("settings-local-api-connect-help"))
                .child(tr("settings-local-api-routes-help"))
                .child(tr("settings-local-api-lifecycle-help"))
                .into_any_element(),
        );
        widgets::card_stack(rows)
    }

    /// The switch, what the listener is doing, and whatever went wrong.
    fn render_local_api_header(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.local_api;
        let status = state.status.as_ref();
        let enabled = status.is_some_and(|status| status.config.enabled);
        let (label, color) = match status {
            Some(status) => (state_label(status), state_color(status)),
            None if state.refreshing => (tr("settings-local-api-checking"), theme::text_muted()),
            None => (tr("settings-local-api-unavailable"), theme::status_error()),
        };
        let detail = if state.saving {
            tr("settings-local-api-updating-local-api-settings")
        } else if status.is_some() {
            state.detail.resolve()
        } else {
            tr(OFF_DETAIL)
        };
        let copy = div()
            .id("local-api-status")
            .role(gpui::Role::Status)
            .flex()
            .flex_col()
            .gap_1()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(row_title(tr("settings-local-api-local-api")))
                    .child(widgets::status_badge(label, color)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(detail),
            );
        let controls = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_3()
            .when(state.can_refresh(), |row| {
                row.child(
                    self.application_target(
                        || SettingsTarget::LocalApi(LocalApiTarget::Refresh),
                        widgets::ghost_button("local-api-refresh")
                            .py_1p5()
                            .tab_index(tab_order::LOCAL_REFRESH)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.refresh_local_api_status(cx)),
                            )
                            .on_key_down(Self::on_press_key(Self::refresh_local_api_status, cx))
                            .child(tr("settings-local-api-refresh-status")),
                    ),
                )
            })
            .when_some(status.filter(|_| state.can_retry()), |row, status| {
                row.child(
                    self.application_target(
                        || SettingsTarget::LocalApi(LocalApiTarget::Retry),
                        widgets::secondary_button("local-api-retry")
                            .py_1p5()
                            .tab_index(tab_order::LOCAL_RETRY)
                            .on_click(cx.listener(|this, _, _, cx| this.retry_local_api(cx)))
                            .on_key_down(Self::on_press_key(Self::retry_local_api, cx))
                            .child(retry_label(status)),
                    ),
                )
            })
            .child(
                self.application_target(
                    || SettingsTarget::LocalApi(LocalApiTarget::Enabled),
                    widgets::switch("local-api-enabled", enabled)
                        .debug_selector(|| "local-api-enabled".to_string())
                        .aria_label(tr("settings-local-api-enable-local-api-on-this-computer"))
                        .when(!state.can_configure(), |switch| {
                            switch.opacity(0.5).aria_description(
                            tr("settings-local-api-unavailable-while-local-api-settings-are-loading-or-being-updated"),
                        )
                        })
                        .when(state.can_configure(), |switch| {
                            switch
                                .tab_index(tab_order::LOCAL_ENABLED)
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_local_api(cx)))
                                .on_key_down(Self::on_press_key(Self::toggle_local_api, cx))
                        }),
                ),
            );
        let banner = |message: SharedString| widgets::banner(theme::status_error()).child(message);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(copy)
                    .child(controls),
            )
            .when_some(state.notice, |column, message| {
                column.child(banner(tr(message)))
            })
            .when_some(
                status.and_then(|status| status.error.clone()),
                |column, error| column.child(banner(error.into())),
            )
    }

    fn render_local_api_url(&self, cx: &mut Context<Self>) -> Div {
        let url = self.local_api.base_url.clone();
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(row_title(tr("settings-local-api-base-url")))
                    .child(
                        div()
                            .font_family(crate::assets::FONT_MONO)
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_secondary()))
                            .child(url.clone()),
                    ),
            )
            .child(
                self.application_target(
                    || SettingsTarget::LocalApi(LocalApiTarget::CopyUrl),
                    widgets::copy_button("local-api-copy-url", url, None, Some(cx.entity_id()))
                        .aria_label(tr("settings-local-api-copy-local-api-base-url"))
                        .tab_index(tab_order::LOCAL_COPY_URL)
                        .on_key_down(Self::on_press_key(Self::copy_local_api_url, cx)),
                ),
            )
    }

    fn render_local_api_port(&self, status: &LocalApiStatus, cx: &mut Context<Self>) -> Div {
        let state = &self.local_api;
        let enabled = status.config.enabled;
        let control = if state.can_edit_port() {
            self.application_target(
                || SettingsTarget::LocalApi(LocalApiTarget::Port),
                widgets::input_frame()
                    .w(gpui::px(96.))
                    .child(state.port.clone()),
            )
        } else {
            div()
                .font_family(crate::assets::FONT_MONO)
                .text_xs()
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(status.config.port.to_string())
                .into_any_element()
        };
        let invalid = state.typed_port.err().filter(|_| state.can_edit_port());
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(match invalid {
                Some(message) => div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(row_title(tr("settings-local-api-port")))
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::status_error()))
                            .child(tr(message)),
                    ),
                None => setting_copy(
                    tr("settings-local-api-port"),
                    if matches!(status.state, LocalApiState::Stopped | LocalApiState::Failed) {
                        tr("settings-local-api-port-address")
                    } else {
                        tr("settings-local-api-stop-to-edit")
                    },
                ),
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    // Save appears beside the field, so the field stays put.
                    .when(state.can_save_port(), |row| {
                        row.child(
                            self.application_target(
                                || SettingsTarget::LocalApi(LocalApiTarget::SavePort),
                                widgets::secondary_button("local-api-save-port")
                                    .py_1p5()
                                    .tab_index(tab_order::LOCAL_SAVE_PORT)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_local_api_port(cx);
                                    }))
                                    .on_key_down(Self::on_press_key(Self::save_local_api_port, cx))
                                    .child(if !enabled {
                                        tr("settings-remote-save")
                                    } else if status.state == LocalApiState::Stopped {
                                        tr("settings-local-api-save-resume")
                                    } else {
                                        tr("settings-local-api-save-retry")
                                    }),
                            ),
                        )
                    })
                    .child(control),
            )
    }
}

fn row_title(label: impl Into<SharedString>) -> Div {
    div()
        .text_sm()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(gpui::rgb(theme::text_primary()))
        .child(label.into())
}

fn parse_port(value: &str) -> Result<u16, &'static str> {
    value
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or(INVALID_PORT)
}

const OFF_DETAIL: &str = "settings-local-api-off-help";

fn state_label(status: &LocalApiStatus) -> SharedString {
    match status.state {
        LocalApiState::Stopped if status.config.enabled => tr("settings-local-api-paused"),
        LocalApiState::Stopped => tr("settings-local-api-off"),
        LocalApiState::Starting => tr("settings-local-api-starting"),
        LocalApiState::Running => tr("settings-local-api-running"),
        LocalApiState::Stopping => tr("settings-local-api-stopping"),
        LocalApiState::Failed => tr("settings-local-api-could-not-start"),
    }
}

fn state_color(status: &LocalApiStatus) -> u32 {
    match status.state {
        LocalApiState::Stopped if status.config.enabled => theme::status_warning(),
        LocalApiState::Running => theme::status_success(),
        LocalApiState::Failed => theme::status_error(),
        _ => theme::text_muted(),
    }
}

fn state_detail(status: &LocalApiStatus) -> UiText {
    match (status.state, status.active_requests) {
        (LocalApiState::Running, 0) => UiText::key("settings-local-api-ready-help"),
        (LocalApiState::Running, active) => {
            UiText::message("settings-local-api-requests", [("count", active.into())])
        }
        (LocalApiState::Stopped, _) if status.config.enabled => {
            UiText::key("settings-local-api-resume-help")
        }
        (LocalApiState::Failed, _) => UiText::key("settings-local-api-failed-help"),
        _ => UiText::key(OFF_DETAIL),
    }
}

fn retry_label(status: &LocalApiStatus) -> SharedString {
    if status.state == LocalApiState::Stopped {
        tr("settings-local-api-resume")
    } else {
        tr("settings-local-api-retry-start")
    }
}

#[cfg(test)]
mod tests {
    use super::super::Section;
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
                Section::Api,
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
                vec![target(LocalApiTarget::CopyUrl)]
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
            assert_eq!(this.local_api.typed_port, Err(INVALID_PORT));
            assert_eq!(this.local_api.notice, None);
        });
    }

    #[gpui::test]
    fn save_appears_only_for_a_different_valid_port(cx: &mut TestAppContext) {
        let settings = screen(cx);
        settings.update(cx, |this, cx| {
            this.local_api.refreshing = false;
            this.set_local_api_status(status(100, LocalApiState::Stopped, false, 8123), cx);
            assert!(!this.local_api.can_save_port());
            this.local_api
                .port
                .update(cx, |input, cx| input.set_text("9123", cx));
        });
        cx.run_until_parked();
        settings.update(cx, |this, cx| {
            assert!(this.local_api.can_save_port());
            assert!(
                this.local_api_targets()
                    .contains(&SettingsTarget::LocalApi(LocalApiTarget::SavePort))
            );
            this.local_api
                .port
                .update(cx, |input, cx| input.set_text("8123", cx));
        });
        cx.run_until_parked();
        settings.update(cx, |this, cx| {
            assert!(!this.local_api.can_save_port());
            this.local_api
                .port
                .update(cx, |input, cx| input.set_text("0", cx));
        });
        cx.run_until_parked();
        settings.update(cx, |this, _| {
            assert_eq!(this.local_api.typed_port, Err(INVALID_PORT));
            assert!(!this.local_api.can_save_port());
        });
    }

    #[test]
    fn detail_counts_requests_and_explains_stopped_states() {
        let mut running = status(1, LocalApiState::Running, true, 8123);
        assert_eq!(
            state_detail(&running)
                .resolve()
                .replace(['\u{2068}', '\u{2069}'], ""),
            "Ready for requests from apps on this computer."
        );
        running.active_requests = 1;
        assert_eq!(
            state_detail(&running)
                .resolve()
                .replace(['\u{2068}', '\u{2069}'], ""),
            "1 active request"
        );
        running.active_requests = 3;
        assert_eq!(
            state_detail(&running)
                .resolve()
                .replace(['\u{2068}', '\u{2069}'], ""),
            "3 active requests"
        );
        assert_eq!(
            state_detail(&status(2, LocalApiState::Stopped, false, 8123)).resolve(),
            tr(OFF_DETAIL)
        );
        assert_eq!(
            state_detail(&status(3, LocalApiState::Stopped, true, 8123)).resolve(),
            "Select Resume to accept requests on this computer."
        );
    }

    #[test]
    fn cached_status_detail_follows_language_without_another_host_snapshot() {
        let _guard = crate::ui::i18n::TestLocalizationGuard::enable();
        let detail = state_detail(&status(1, LocalApiState::Running, true, 8123));
        for (locale, expected) in [
            ("en", "Ready for requests from apps on this computer."),
            (
                "es",
                "Lista para recibir solicitudes de las aplicaciones de esta computadora.",
            ),
            ("en", "Ready for requests from apps on this computer."),
        ] {
            crate::ui::i18n::apply_preference(Some(locale));
            assert_eq!(detail.resolve().as_ref(), expected);
        }
    }

    #[gpui::test]
    fn refresh_is_offered_only_to_recover(cx: &mut TestAppContext) {
        let settings = screen(cx);
        settings.update(cx, |this, cx| {
            this.local_api.refreshing = false;
            assert!(this.local_api.can_refresh());
            this.set_local_api_status(status(100, LocalApiState::Running, true, 8123), cx);
            assert!(!this.local_api.can_refresh());
            this.local_api.notice = Some("failed");
            assert!(this.local_api.can_refresh());
        });
    }

    #[gpui::test]
    fn a_free_plan_keeps_the_local_api_controls(cx: &mut TestAppContext) {
        let settings = screen(cx);
        settings.update(cx, |this, cx| {
            this.local_api.refreshing = false;
            this.set_local_api_status(status(100, LocalApiState::Running, true, 8123), cx);
            this.billing.status = Some(Default::default());
            assert_eq!(this.api_access(), Some(false));
            let targets = this.api_targets();
            assert!(targets.contains(&SettingsTarget::LocalApi(LocalApiTarget::Enabled)));
            assert!(!targets.contains(&SettingsTarget::ApiKeys(
                super::super::api_keys::ApiKeysTarget::Name
            )));
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
            assert_eq!(state_label(this.local_api.status.as_ref().unwrap()), "Off");
            assert!(!this.local_api.can_retry());
        });
    }
}
