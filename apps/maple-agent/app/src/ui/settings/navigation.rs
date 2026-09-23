//! Stable-ID application Vim navigation for Settings.
//!
//! The settings screen owns this projection because its visible controls are
//! dynamic (shortcut filtering, integrations, MCP servers, and editors).
//! Selecting a row is side-effect free; Enter delegates to the same existing
//! method as its pointer control.

use gpui::{Context, Focusable, IntoElement, StatefulInteractiveElement, Window, div, prelude::*};

use super::account::AccountTarget;
use super::api_keys::ApiKeysTarget;
use super::billing::BillingTarget;
use super::{
    Section, SettingMenu, SettingsScreen, integration_can_setup, integration_can_toggle,
    integration_is_visible,
};
use crate::ui::application_vim::{self, CountOutcome, CountState, SpatialDirection};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SettingsRegion {
    Navigation,
    Pane,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GeneralTarget {
    Permission,
    Web,
    Appearance,
    ChatFont,
    ChatSize,
    ToolDetails,
    Notifications,
    ReduceMotion,
    ToolSummaries,
    ComposerVim,
    ApplicationVim,
    Voice,
    SpeechSpeed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SettingsTarget {
    General(GeneralTarget),
    Account(AccountTarget),
    Billing(BillingTarget),
    ApiKeys(ApiKeysTarget),
    Shortcut(String),
    PromptEditor,
    PromptSave,
    PromptReset,
    Integration(String),
    IntegrationSetup(String),
    McpAdd,
    McpServer(String),
}

#[derive(Clone, Debug)]
pub(super) struct SettingsApplicationVimState {
    pub(super) region: SettingsRegion,
    pub(super) section: Section,
    pub(super) target: Option<SettingsTarget>,
    count: CountState,
}

impl SettingsApplicationVimState {
    pub(super) fn new(section: Section) -> Self {
        Self {
            region: SettingsRegion::Navigation,
            section,
            target: None,
            count: CountState::default(),
        }
    }
}

impl SettingsScreen {
    pub(super) fn application_vim_context(&self) -> &'static str {
        if self.settings.application_vim_enabled {
            "Settings ApplicationVim"
        } else {
            "Settings"
        }
    }

    pub(super) fn set_application_vim_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let mut inputs = vec![self.prompt_editor.clone(), self.shortcut_search.clone()];
        inputs.extend(self.account.inputs());
        inputs.push(self.api_keys.name.clone());
        if let Some(editor) = &self.mcp_editor {
            inputs.extend([
                editor.name.clone(),
                editor.description.clone(),
                editor.target.clone(),
                editor.environment.clone(),
                editor.headers.clone(),
            ]);
        }
        for input in inputs {
            input.update(cx, |input, cx| {
                input.set_application_vim_enabled(enabled, cx)
            });
        }
        if self.settings.application_vim_enabled == enabled {
            return;
        }
        self.edit_setting(
            move |settings| settings.application_vim_enabled = enabled,
            cx,
        );
        self.application_vim.count.clear();
        if enabled {
            self.application_vim.region = SettingsRegion::Navigation;
            self.application_vim.section = self.section;
            self.reconcile_application_vim_target();
            self.application_focus_pending = true;
        } else {
            // Keep the always-mounted focus proxy, but release every semantic
            // target that exists only for Application Vim.
            self.application_vim = SettingsApplicationVimState::new(self.section);
        }
        cx.notify();
    }

    pub(super) fn visible_application_targets(&self) -> Vec<SettingsTarget> {
        match self.section {
            Section::General => [
                GeneralTarget::Permission,
                GeneralTarget::Web,
                GeneralTarget::Appearance,
                GeneralTarget::ChatFont,
                GeneralTarget::ChatSize,
                GeneralTarget::ToolDetails,
                GeneralTarget::Notifications,
                GeneralTarget::ReduceMotion,
                GeneralTarget::ToolSummaries,
                GeneralTarget::ComposerVim,
                GeneralTarget::ApplicationVim,
                GeneralTarget::Voice,
                GeneralTarget::SpeechSpeed,
            ]
            .into_iter()
            .map(SettingsTarget::General)
            .collect(),
            Section::Account => self.account_targets(),
            Section::Billing => self.billing_targets(),
            Section::ApiKeys => self.api_keys_targets(),
            Section::Shortcuts => self
                .shortcut_list_cache
                .visible_indices
                .iter()
                .filter_map(|&index| self.shortcut_snapshot.rows.get(index))
                .map(|row| SettingsTarget::Shortcut(row.slot_id.clone()))
                .collect(),
            Section::Prompt => vec![
                SettingsTarget::PromptEditor,
                SettingsTarget::PromptSave,
                SettingsTarget::PromptReset,
            ],
            Section::Integrations => self
                .integrations
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|integration| integration_is_visible(integration))
                .flat_map(|integration| {
                    [
                        integration_can_setup(integration)
                            .then(|| SettingsTarget::IntegrationSetup(integration.id.clone())),
                        integration_can_toggle(integration)
                            .then(|| SettingsTarget::Integration(integration.id.clone())),
                    ]
                    .into_iter()
                    .flatten()
                })
                .chain(std::iter::once(SettingsTarget::McpAdd))
                .chain(
                    self.mcp_servers
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .map(|server| SettingsTarget::McpServer(server.name.clone())),
                )
                .collect(),
            Section::Usage | Section::About => Vec::new(),
        }
    }

    pub(super) fn reconcile_application_vim_target(&mut self) {
        if !self.settings.application_vim_enabled {
            return;
        }
        self.application_vim.section = self.section;
        let targets = self.visible_application_targets();
        if self
            .application_vim
            .target
            .as_ref()
            .is_some_and(|selected| targets.contains(selected))
        {
            return;
        }
        let target = targets.first().cloned();
        if self.application_vim.target != target {
            self.application_vim.target = target;
            if self.application_vim.region == SettingsRegion::Pane {
                self.application_reveal_pending = true;
            }
        }
    }

    pub(super) fn application_vim_selects_section(&self, section: Section) -> bool {
        self.settings.application_vim_enabled
            && self.application_vim.region == SettingsRegion::Navigation
            && self.application_vim.section == section
    }

    pub(super) fn application_vim_selects_target(&self, target: &SettingsTarget) -> bool {
        self.settings.application_vim_enabled
            && self.application_vim.region == SettingsRegion::Pane
            && self.application_vim.target.as_ref() == Some(target)
    }

    pub(super) fn application_target(
        &self,
        target: impl FnOnce() -> SettingsTarget,
        child: impl IntoElement,
    ) -> gpui::AnyElement {
        if !self.settings.application_vim_enabled {
            return child.into_any_element();
        }
        let target = target();
        let selected = self.application_vim_selects_target(&target);
        let id = gpui::SharedString::from(format!("settings-application-{target:?}"));
        div()
            .id(id)
            .when(selected, |row| {
                row.rounded(crate::ui::theme::RADIUS_SM)
                    .border_l_2()
                    .border_color(gpui::rgb(crate::ui::theme::accent()))
                    .anchor_scroll(Some(self.application_anchor.clone()))
            })
            .child(child)
            .into_any_element()
    }

    fn move_application_selection(
        &mut self,
        direction: isize,
        count: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.application_vim.region {
            SettingsRegion::Navigation => {
                let current = Section::ALL
                    .iter()
                    .position(|section| *section == self.application_vim.section)
                    .unwrap_or(0);
                let index = stepped_index(Some(current), Section::ALL.len(), direction, count);
                self.application_vim.section = Section::ALL[index];
            }
            SettingsRegion::Pane => {
                let targets = self.visible_application_targets();
                if targets.is_empty() {
                    return;
                }
                let current = self
                    .application_vim
                    .target
                    .as_ref()
                    .and_then(|selected| targets.iter().position(|target| target == selected));
                let index = stepped_index(current, targets.len(), direction, count);
                self.application_vim.target = Some(targets[index].clone());
                self.application_reveal_pending = true;
            }
        }
        cx.notify();
    }

    fn select_application_edge(
        &mut self,
        first: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.application_vim.region {
            SettingsRegion::Navigation => {
                self.application_vim.section = if first {
                    Section::ALL[0]
                } else {
                    Section::ALL[Section::ALL.len() - 1]
                };
            }
            SettingsRegion::Pane => {
                let targets = self.visible_application_targets();
                self.application_vim.target = if first {
                    targets.first().cloned()
                } else {
                    targets.last().cloned()
                };
                self.application_reveal_pending = true;
            }
        }
        cx.notify();
    }

    fn move_application_region(
        &mut self,
        direction: SpatialDirection,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match (self.application_vim.region, direction) {
            (SettingsRegion::Navigation, SpatialDirection::Right) => {
                if self.section != self.application_vim.section {
                    self.select_section(self.application_vim.section, cx);
                }
                self.application_vim.region = SettingsRegion::Pane;
                self.reconcile_application_vim_target();
                self.application_reveal_pending = true;
            }
            (SettingsRegion::Pane, SpatialDirection::Left) => {
                self.application_vim.region = SettingsRegion::Navigation;
                self.application_vim.section = self.section;
            }
            _ => {
                self.shortcut_notice =
                    Some("There is no Settings region in that direction".to_string());
                cx.notify();
                return false;
            }
        }
        cx.notify();
        true
    }

    fn activate_application_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.application_vim.region == SettingsRegion::Navigation {
            let section = self.application_vim.section;
            self.select_section(section, cx);
            self.application_vim.region = SettingsRegion::Pane;
            self.reconcile_application_vim_target();
            self.application_reveal_pending = true;
            return;
        }
        match self.application_vim.target.clone() {
            Some(SettingsTarget::General(target)) => self.activate_general_target(target, cx),
            Some(SettingsTarget::Account(target)) => {
                self.activate_account_target(target, window, cx)
            }
            Some(SettingsTarget::Billing(target)) => self.activate_billing_target(target, cx),
            Some(SettingsTarget::ApiKeys(target)) => {
                self.activate_api_keys_target(target, window, cx)
            }
            Some(SettingsTarget::Shortcut(slot_id)) => self.begin_shortcut_recording(slot_id, cx),
            Some(SettingsTarget::PromptEditor) => {
                let handle = self.prompt_editor.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            Some(SettingsTarget::PromptSave) => self.save_prompt(cx),
            Some(SettingsTarget::PromptReset) => self.reset_prompt(cx),
            Some(SettingsTarget::Integration(id)) => self.toggle_integration(&id, cx),
            Some(SettingsTarget::IntegrationSetup(id)) => self.setup_integration(&id, cx),
            Some(SettingsTarget::McpAdd) => self.open_mcp_editor(None, cx),
            Some(SettingsTarget::McpServer(name)) => self.edit_mcp_server(&name, cx),
            None => {}
        }
    }

    fn activate_general_target(&mut self, target: GeneralTarget, cx: &mut Context<Self>) {
        match target {
            GeneralTarget::Permission => self.toggle_setting_menu(SettingMenu::Permission, cx),
            GeneralTarget::Web => self.toggle_web_default(cx),
            GeneralTarget::Appearance => self.toggle_setting_menu(SettingMenu::Appearance, cx),
            GeneralTarget::ChatFont => self.toggle_setting_menu(SettingMenu::ChatFont, cx),
            GeneralTarget::ChatSize => self.toggle_setting_menu(SettingMenu::ChatSize, cx),
            GeneralTarget::ToolDetails => self.toggle_tool_details(cx),
            GeneralTarget::Notifications => self.toggle_desktop_notifications(cx),
            GeneralTarget::ReduceMotion => self.toggle_reduce_motion(cx),
            GeneralTarget::ToolSummaries => self.toggle_tool_summaries(cx),
            GeneralTarget::ComposerVim => self.toggle_composer_vim(cx),
            GeneralTarget::ApplicationVim => {
                let enabled = !self.settings.application_vim_enabled;
                self.set_application_vim_enabled(enabled, cx);
            }
            GeneralTarget::Voice => self.toggle_setting_menu(SettingMenu::Voice, cx),
            GeneralTarget::SpeechSpeed => self.toggle_setting_menu(SettingMenu::SpeechSpeed, cx),
        }
    }

    fn search_application_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.section != Section::Shortcuts {
            self.select_section(Section::Shortcuts, cx);
        }
        let handle = self.shortcut_search.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn application_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.application_text_input_focused(window, cx) {
            window.focus(&self.application_focus, cx);
            cx.notify();
            return;
        }
        if self.shortcut_recorder.is_some() {
            self.stop_shortcut_recording();
            cx.notify();
            return;
        }
        // An open dropdown closes before Escape leaves Settings. It
        // normally has focus and closes on its own Escape binding; this
        // covers one that has not taken focus yet.
        if self.popup.close(cx) {
            return;
        }
        self.close(cx);
    }

    fn application_text_input_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        let focused = window.focused(cx);
        let mut inputs = vec![&self.prompt_editor, &self.shortcut_search];
        if let Some(editor) = &self.mcp_editor {
            inputs.extend([
                &editor.name,
                &editor.description,
                &editor.target,
                &editor.environment,
                &editor.headers,
            ]);
        }
        inputs
            .into_iter()
            .any(|input| Some(input.read(cx).focus_handle(cx)) == focused)
    }

    fn execute_application_vim(
        &mut self,
        command: SettingsApplicationCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.application_vim_enabled {
            return;
        }
        match command {
            SettingsApplicationCommand::Next | SettingsApplicationCommand::Previous => {
                let count = self.application_vim.count.take();
                self.move_application_selection(
                    match command {
                        SettingsApplicationCommand::Next => 1,
                        _ => -1,
                    },
                    count,
                    window,
                    cx,
                );
            }
            SettingsApplicationCommand::First | SettingsApplicationCommand::Last => {
                self.application_vim.count.clear();
                self.select_application_edge(
                    matches!(command, SettingsApplicationCommand::First),
                    window,
                    cx,
                );
            }
            SettingsApplicationCommand::Activate => {
                self.application_vim.count.clear();
                self.activate_application_selection(window, cx);
            }
            SettingsApplicationCommand::Search => {
                self.application_vim.count.clear();
                self.search_application_settings(window, cx);
            }
            SettingsApplicationCommand::Escape => {
                self.application_vim.count.clear();
                self.application_escape(window, cx);
            }
            SettingsApplicationCommand::MoveRegion(direction) => {
                let count = self.application_vim.count.take();
                for _ in 0..count {
                    if !self.move_application_region(direction, window, cx) {
                        break;
                    }
                }
            }
            SettingsApplicationCommand::Unavailable => {
                self.application_vim.count.clear();
                self.shortcut_notice = Some(
                    "That application Vim command is not available on this Settings target"
                        .to_string(),
                );
                cx.notify();
            }
        }
    }

    pub(super) fn app_vim_count(
        &mut self,
        action: &application_vim::CountDigit,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            self.application_vim.count.push(action.digit),
            CountOutcome::Capped(_)
        ) {
            self.shortcut_notice = Some("Application Vim count capped at 999999".to_string());
        }
        cx.notify();
    }
}

#[derive(Clone, Copy)]
enum SettingsApplicationCommand {
    Next,
    Previous,
    First,
    Last,
    Activate,
    Search,
    Escape,
    MoveRegion(SpatialDirection),
    Unavailable,
}

macro_rules! adapter {
    ($method:ident, $action:ty, $command:expr) => {
        impl SettingsScreen {
            pub(super) fn $method(
                &mut self,
                _: &$action,
                window: &mut Window,
                cx: &mut Context<Self>,
            ) {
                self.execute_application_vim($command, window, cx);
            }
        }
    };
}

adapter!(
    app_vim_next,
    application_vim::Next,
    SettingsApplicationCommand::Next
);
adapter!(
    app_vim_previous,
    application_vim::Previous,
    SettingsApplicationCommand::Previous
);
adapter!(
    app_vim_first,
    application_vim::First,
    SettingsApplicationCommand::First
);
adapter!(
    app_vim_last,
    application_vim::Last,
    SettingsApplicationCommand::Last
);
adapter!(
    app_vim_activate,
    application_vim::Activate,
    SettingsApplicationCommand::Activate
);
adapter!(
    app_vim_search,
    application_vim::Search,
    SettingsApplicationCommand::Search
);
adapter!(
    app_vim_escape,
    application_vim::Escape,
    SettingsApplicationCommand::Escape
);
adapter!(
    app_vim_collapse,
    application_vim::Collapse,
    SettingsApplicationCommand::Unavailable
);
adapter!(
    app_vim_expand,
    application_vim::Expand,
    SettingsApplicationCommand::Unavailable
);
adapter!(
    app_vim_copy,
    application_vim::CopyTarget,
    SettingsApplicationCommand::Unavailable
);
adapter!(
    app_vim_composer,
    application_vim::FocusComposer,
    SettingsApplicationCommand::Unavailable
);
adapter!(
    app_vim_newest_assistant,
    application_vim::NewestAssistant,
    SettingsApplicationCommand::Unavailable
);
adapter!(
    app_vim_next_assistant,
    application_vim::NextAssistant,
    SettingsApplicationCommand::Unavailable
);
adapter!(
    app_vim_previous_assistant,
    application_vim::PreviousAssistant,
    SettingsApplicationCommand::Unavailable
);
adapter!(
    app_vim_next_annotation,
    application_vim::NextAnnotation,
    SettingsApplicationCommand::Unavailable
);
adapter!(
    app_vim_previous_annotation,
    application_vim::PreviousAnnotation,
    SettingsApplicationCommand::Unavailable
);

impl SettingsScreen {
    pub(super) fn app_vim_move_region(
        &mut self,
        action: &application_vim::MoveRegion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_application_vim(
            SettingsApplicationCommand::MoveRegion(action.direction),
            window,
            cx,
        );
    }
}

fn stepped_index(current: Option<usize>, len: usize, direction: isize, count: usize) -> usize {
    debug_assert!(len > 0);
    match current {
        Some(start) if direction > 0 => start.saturating_add(count).min(len - 1),
        Some(start) => start.saturating_sub(count),
        None if direction > 0 => count.saturating_sub(1).min(len - 1),
        None => len.saturating_sub(count.max(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Context, Entity, IntoElement, Render, TestAppContext, Window, div, px};

    #[test]
    fn counted_settings_navigation_clamps() {
        assert_eq!(stepped_index(Some(2), 6, 1, 20), 5);
        assert_eq!(stepped_index(Some(4), 6, -1, 20), 0);
    }

    #[gpui::test]
    fn integration_targets_follow_the_visible_control_order(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend,
                "user".to_string(),
                crate::settings::AppSettings::default(),
                crate::shortcuts::ShortcutSnapshot {
                    generation: 1,
                    rows: Vec::new(),
                    last_error: None,
                    compatibility_warning: None,
                },
                Section::Integrations,
                cx,
            )
        });

        settings.update(cx, |this, _cx| {
            this.integrations = Some(vec![
                maple_agent::agent::AgentIntegration {
                    id: "cua-driver".to_string(),
                    name: "Cua".to_string(),
                    description: String::new(),
                    availability: maple_agent::agent::AgentIntegrationAvailability::Available,
                    version: None,
                    enabled_for_new_tasks: true,
                    detail: None,
                    permissions: Some(
                        maple_agent::agent::AgentIntegrationPermissions::default()
                            .with(
                                maple_agent::agent::AgentIntegrationPermissionKind::Accessibility,
                                false,
                            )
                            .with(
                                maple_agent::agent::AgentIntegrationPermissionKind::ScreenRecording,
                                false,
                            ),
                    ),
                    setup_available: true,
                    standalone_version: Some("0.23.2".to_string()),
                    backend: Some(maple_agent::agent::AgentIntegrationBackend::External),
                },
                // An undetected integration contributes no focus target.
                maple_agent::agent::AgentIntegration {
                    id: "not-ready".to_string(),
                    name: "Not ready".to_string(),
                    description: String::new(),
                    availability: maple_agent::agent::AgentIntegrationAvailability::NotDetected,
                    version: None,
                    enabled_for_new_tasks: false,
                    detail: None,
                    permissions: None,
                    setup_available: false,
                    standalone_version: None,
                    backend: None,
                },
            ]);
            this.mcp_servers = Some(vec![maple_agent::agent::AgentMcpServer {
                name: "custom".to_string(),
                description: String::new(),
                enabled: true,
                timeout_seconds: 300,
                transport: maple_agent::agent::AgentMcpTransport::Stdio {
                    command: "custom-mcp".to_string(),
                    environment: Vec::new(),
                },
            }]);

            assert_eq!(
                this.visible_application_targets(),
                vec![
                    SettingsTarget::IntegrationSetup("cua-driver".to_string()),
                    SettingsTarget::Integration("cua-driver".to_string()),
                    SettingsTarget::McpAdd,
                    SettingsTarget::McpServer("custom".to_string()),
                ]
            );
        });
    }

    #[gpui::test]
    fn application_vim_off_keeps_settings_projection_empty(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend,
                "user".to_string(),
                crate::settings::AppSettings {
                    application_vim_enabled: false,
                    ..Default::default()
                },
                crate::shortcuts::ShortcutSnapshot {
                    generation: 1,
                    rows: vec![crate::shortcuts::ShortcutRow {
                        slot_id: "application_vim.search".to_string(),
                        label: "Application Vim search".to_string(),
                        category: "Application Vim".to_string(),
                        context: Some(crate::ui::application_vim::ROOT_CONTEXT.to_string()),
                        default_sequence: "/".to_string(),
                        current_sequence: Some("/".to_string()),
                        modified: false,
                        conflicts: Vec::new(),
                    }],
                    last_error: None,
                    compatibility_warning: None,
                },
                Section::Shortcuts,
                cx,
            )
        });

        settings.update(cx, |this, _cx| {
            this.shortcut_query = "application vim".to_string();
            this.rebuild_shortcut_list_cache();
            assert_eq!(this.shortcut_list_cache.visible_indices, vec![0]);

            // The guard is defensive as well as call-site based: ordinary
            // Settings filtering cannot populate the disabled projection.
            this.reconcile_application_vim_target();
            assert_eq!(this.application_vim.target, None);
            assert!(!this.application_reveal_pending);

            let target_built = std::cell::Cell::new(false);
            let _element = this.application_target(
                || {
                    target_built.set(true);
                    SettingsTarget::Shortcut("unused".to_string())
                },
                div(),
            );
            assert!(
                !target_built.get(),
                "disabled rows must not allocate semantic targets"
            );
        });
    }

    #[gpui::test]
    fn settings_reveal_waits_for_the_selected_row_render(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        struct SettingsHost {
            settings: Entity<SettingsScreen>,
        }
        impl Render for SettingsHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().w(px(1200.)).h(px(800.)).child(self.settings.clone())
            }
        }

        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend,
                "user".to_string(),
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
                Section::General,
                cx,
            )
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| SettingsHost {
            settings: settings.clone(),
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));

        cx.update(|window, app| {
            settings.update(app, |this, cx| {
                assert!(this.move_application_region(SpatialDirection::Right, window, cx));
                assert_eq!(this.application_vim.region, SettingsRegion::Pane);
                assert!(
                    this.application_reveal_pending,
                    "entering the pane defers reveal until its selected row is rendered"
                );

                this.move_application_selection(1, 1, window, cx);
                assert_eq!(
                    this.application_vim.target,
                    Some(SettingsTarget::General(GeneralTarget::Web))
                );
                assert!(
                    this.application_reveal_pending,
                    "j/k must hand the new target to the next render before scrolling"
                );
            });
        });
    }

    #[gpui::test]
    fn disabling_application_vim_keeps_settings_focus_attached(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        struct SettingsHost {
            settings: Entity<SettingsScreen>,
        }
        impl Render for SettingsHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().w(px(1200.)).h(px(800.)).child(self.settings.clone())
            }
        }

        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend,
                "user".to_string(),
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
                Section::General,
                cx,
            )
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| SettingsHost {
            settings: settings.clone(),
        });
        let window_size = gpui::size(px(1200.), px(800.));
        cx.simulate_resize(window_size);

        cx.update(|window, app| {
            let settings = settings.read(app);
            assert_eq!(
                window.focused(app),
                Some(settings.application_focus.clone())
            );
            assert!(
                window
                    .context_stack()
                    .iter()
                    .any(|context| context.contains("ApplicationVim"))
            );
        });

        settings.update(cx, |this, cx| {
            // Exercise the render transition directly without persisting a
            // setting from this test process.
            this.settings.application_vim_enabled = false;
            cx.notify();
        });
        cx.simulate_resize(window_size);

        cx.update(|window, app| {
            let settings = settings.read(app);
            assert_eq!(
                window.focused(app),
                Some(settings.application_focus.clone())
            );
            let contexts = window.context_stack();
            assert!(contexts.iter().any(|context| context.contains("Settings")));
            assert!(
                contexts
                    .iter()
                    .all(|context| !context.contains("ApplicationVim"))
            );
        });
    }

    struct DropdownHost {
        settings: Entity<SettingsScreen>,
    }
    impl Render for DropdownHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(1200.))
                .h(px(800.))
                .flex()
                .flex_col()
                .child(self.settings.clone())
        }
    }

    fn dropdown_screen(cx: &mut TestAppContext, application_vim: bool) -> Entity<SettingsScreen> {
        cx.executor().allow_parking();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        cx.new(|cx| {
            SettingsScreen::new(
                backend,
                "user".to_string(),
                crate::settings::AppSettings {
                    application_vim_enabled: application_vim,
                    ..Default::default()
                },
                crate::shortcuts::ShortcutSnapshot {
                    generation: 1,
                    rows: Vec::new(),
                    last_error: None,
                    compatibility_warning: None,
                },
                Section::General,
                cx,
            )
        })
    }

    /// Settings in an active window with the shipped key bindings, so a
    /// dropdown takes the keyboard.
    fn dropdown_window(
        cx: &mut TestAppContext,
        application_vim: bool,
    ) -> (Entity<SettingsScreen>, &mut gpui::VisualTestContext) {
        cx.update(crate::desktop::register_key_bindings);
        let settings = dropdown_screen(cx, application_vim);
        let (_host, cx) = cx.add_window_view(|_window, _cx| DropdownHost {
            settings: settings.clone(),
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        (settings, cx)
    }

    #[gpui::test]
    fn dropdown_rows_select_highlight_and_close(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, false);
        // Opening highlights the saved voice.
        settings.update(cx, |this, cx| {
            this.toggle_setting_menu(SettingMenu::Voice, cx)
        });
        cx.run_until_parked();
        let saved = settings.update(cx, |this, _| {
            assert_eq!(this.popup.open_key(), Some(&SettingMenu::Voice));
            this.menu_options(SettingMenu::Voice)
                .iter()
                .position(|option| option.current)
                .expect("the saved voice is one of the options")
        });
        assert_eq!(
            settings.update(cx, |this, _| this.popup.highlighted()),
            Some(saved)
        );

        // Down advances one option, wrapping; Enter picks it and closes.
        cx.simulate_keystrokes("down");
        let picked = (saved + 1) % crate::settings::TTS_VOICES.len();
        assert_eq!(
            settings.update(cx, |this, _| this.popup.highlighted()),
            Some(picked)
        );
        cx.simulate_keystrokes("enter");
        settings.update(cx, |this, cx| {
            assert_eq!(this.popup.open_key(), None, "picking closes the dropdown");
            assert_eq!(
                this.settings.tts_voice,
                crate::settings::TTS_VOICES[picked].0,
                "the picked voice becomes the setting"
            );

            // Opening another row replaces the open one.
            this.toggle_setting_menu(SettingMenu::Voice, cx);
            this.toggle_setting_menu(SettingMenu::Appearance, cx);
            assert_eq!(this.popup.open_key(), Some(&SettingMenu::Appearance));
        });
    }

    /// Issue #997: a second press on the value button closes its dropdown.
    #[gpui::test]
    fn a_second_press_on_a_dropdown_closes_it(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, false);
        for open in [Some(SettingMenu::Appearance), None] {
            let bounds = cx
                .debug_bounds("setting-value-appearance")
                .expect("the appearance value button renders");
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            assert_eq!(
                settings.update(cx, |this, _| this.popup.open_key().copied()),
                open
            );
        }
    }

    #[gpui::test]
    fn vim_activation_opens_picks_and_escapes_the_dropdown(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, true);
        let highlighted = |cx: &mut gpui::VisualTestContext| {
            settings.update(cx, |this, _| this.popup.highlighted())
        };
        // Enter on the speech-speed row opens its dropdown.
        cx.update(|window, app| {
            settings.update(app, |this, cx| {
                this.application_vim.region = SettingsRegion::Pane;
                this.application_vim.target =
                    Some(SettingsTarget::General(GeneralTarget::SpeechSpeed));
                this.execute_application_vim(SettingsApplicationCommand::Activate, window, cx);
                assert_eq!(this.popup.open_key(), Some(&SettingMenu::SpeechSpeed));
            })
        });
        cx.run_until_parked();

        // The menu has the keyboard: j, G, and g g move its highlight, not
        // the row selection.
        let before = highlighted(cx);
        cx.simulate_keystrokes("j");
        assert_ne!(highlighted(cx), before);
        cx.simulate_keystrokes("G");
        assert_eq!(highlighted(cx), Some(crate::settings::TTS_SPEEDS.len() - 1));
        cx.simulate_keystrokes("g g j");
        assert_eq!(highlighted(cx), Some(1));
        assert_eq!(
            settings.update(cx, |this, _| this.application_vim.target.clone()),
            Some(SettingsTarget::General(GeneralTarget::SpeechSpeed)),
            "the row selection must not move while the menu is open"
        );

        // Enter picks the highlighted option and closes the menu.
        cx.simulate_keystrokes("enter");
        settings.update(cx, |this, _| {
            assert_eq!(this.popup.open_key(), None);
            let expected = crate::settings::TTS_SPEEDS[1];
            assert!((this.settings.tts_speed - expected).abs() < 0.01);
        });

        // Escape while a menu is open closes it instead of leaving
        // Settings.
        settings.update(cx, |this, cx| {
            this.toggle_setting_menu(SettingMenu::Voice, cx)
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        assert_eq!(
            settings.update(cx, |this, _| this.popup.open_key().copied()),
            None
        );
    }

    #[gpui::test]
    fn picking_from_the_dropdown_persists_to_disk(cx: &mut TestAppContext) {
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        let dir = std::env::temp_dir().join(format!(
            "maple-gpui-dropdown-persist-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        let settings = dropdown_screen(cx, false);
        let (_host, cx) = cx.add_window_view(|_window, _cx| DropdownHost {
            settings: settings.clone(),
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));

        settings.update(cx, |this, cx| {
            this.toggle_setting_menu(SettingMenu::Appearance, cx);
            this.pick_setting_option(SettingMenu::Appearance, 2, cx);
        });
        // Wait for the writer thread to flush the queued change.
        crate::settings::update_settings_and_wait(|_| {});
        let reloaded = crate::settings::load_settings();

        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            reloaded.theme, "light",
            "a dropdown pick must reach the settings file"
        );
    }
}
