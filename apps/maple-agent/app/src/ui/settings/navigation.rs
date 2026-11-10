//! Stable-ID application Vim navigation for Settings.
//!
//! The settings screen owns this projection because its visible controls are
//! dynamic (shortcut filtering, integrations, tools, MCP servers, and ACP
//! examples).
//! Selecting a row is side-effect free; Enter delegates to the same existing
//! method as its pointer control.

use crate::ui::i18n::UiText;
use gpui::{Context, Focusable, IntoElement, StatefulInteractiveElement, Window, div, prelude::*};

#[cfg(feature = "acp")]
use super::DeveloperTab;
use super::account::AccountTarget;
#[cfg(feature = "acp")]
use super::acp::AcpTarget;
use super::api_keys::ApiKeysTarget;
use super::billing::BillingTarget;
#[cfg(feature = "proxy")]
use super::local_api::LocalApiTarget;
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
    Language,
    Notifications,
    PasswordNotificationDetails,
    ComposerVim,
    ApplicationVim,
    Voice,
    SpeechSpeed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AppearanceTarget {
    Theme(crate::ui::theme::Preference),
    ReduceMotion,
    ChatFont,
    ChatSize,
    ToolDetails,
    ToolSummaries,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SettingsTarget {
    General(GeneralTarget),
    Appearance(AppearanceTarget),
    #[cfg(feature = "acp")]
    DeveloperTab(DeveloperTab),
    Account(AccountTarget),
    Billing(BillingTarget),
    ApiKeys(ApiKeysTarget),
    #[cfg(feature = "proxy")]
    LocalApi(LocalApiTarget),
    #[cfg(feature = "acp")]
    Acp(AcpTarget),
    Shortcut(String),
    /// The one refresh control on pages that fetch remote state: the icon in
    /// the page header.
    PageRefresh,
    PromptEditor,
    PromptSave,
    PromptClear,
    Integration(String),
    IntegrationSetup(String),
    Tool(String),
    McpAdd,
    McpServer(String),
    McpCatalog(String),
    /// A tile on the Popular apps strip; distinct from the same app's
    /// catalog card so cursor motion resolves to one element.
    McpFeatured(String),
    /// "See all" on the Popular apps strip.
    McpCatalogSeeAll,
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
        let mut inputs = vec![
            self.sidebar_filter.clone(),
            self.prompt_editor.clone(),
            self.shortcut_search.clone(),
            self.catalog_search.clone(),
        ];
        inputs.extend(self.account.inputs());
        inputs.push(self.advanced.confirmation.clone());
        inputs.push(self.api_keys.name.clone());
        inputs.push(self.billing.credits.custom.clone());
        #[cfg(feature = "proxy")]
        inputs.push(self.local_api.port.clone());
        if let Some(editor) = &self.mcp_editor {
            inputs.extend([
                editor.name.clone(),
                editor.description.clone(),
                editor.target.clone(),
                editor.environment.clone(),
                editor.headers.clone(),
            ]);
        }
        if let Some(prompt) = &self.mcp_address {
            inputs.push(prompt.input.clone());
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
        // The server dialog covers the page; its fields take the keyboard
        // themselves, so nothing underneath is a target while it is open.
        if self.mcp_editor.is_some()
            || self.mcp_address.is_some()
            || self.advanced.erase
            || self.advanced.form.is_some()
        {
            return Vec::new();
        }
        if self.account.delete != super::account::DeleteFlow::Idle {
            return self.delete_dialog_targets();
        }
        match self.section {
            Section::General => [
                GeneralTarget::Language,
                GeneralTarget::Notifications,
                GeneralTarget::PasswordNotificationDetails,
                GeneralTarget::ComposerVim,
                GeneralTarget::ApplicationVim,
                GeneralTarget::Voice,
                GeneralTarget::SpeechSpeed,
            ]
            .into_iter()
            .map(SettingsTarget::General)
            .chain([
                SettingsTarget::PromptEditor,
                SettingsTarget::PromptClear,
                SettingsTarget::PromptSave,
            ])
            .collect(),
            Section::Appearance => [
                AppearanceTarget::Theme(crate::ui::theme::Preference::System),
                AppearanceTarget::Theme(crate::ui::theme::Preference::Dark),
                AppearanceTarget::Theme(crate::ui::theme::Preference::Light),
                AppearanceTarget::ReduceMotion,
                AppearanceTarget::ChatFont,
                AppearanceTarget::ChatSize,
                AppearanceTarget::ToolDetails,
                AppearanceTarget::ToolSummaries,
            ]
            .into_iter()
            .map(SettingsTarget::Appearance)
            .collect(),
            Section::Account => self.account_targets(),
            Section::Usage => self.usage_targets(),
            Section::Billing => self.billing_targets(),
            Section::Api => {
                #[cfg(feature = "acp")]
                {
                    let mut targets = Vec::new();
                    if self.developer_tab == DeveloperTab::Api {
                        targets.push(SettingsTarget::PageRefresh);
                    }
                    targets.extend([
                        SettingsTarget::DeveloperTab(DeveloperTab::Api),
                        SettingsTarget::DeveloperTab(DeveloperTab::Acp),
                    ]);
                    targets.extend(match self.developer_tab {
                        DeveloperTab::Api => self.api_targets(),
                        DeveloperTab::Acp => self.acp_targets(),
                    });
                    targets
                }
                #[cfg(not(feature = "acp"))]
                {
                    let mut targets = vec![SettingsTarget::PageRefresh];
                    targets.extend(self.api_targets());
                    targets
                }
            }
            Section::Shortcuts => self
                .shortcut_list_cache
                .visible_indices
                .iter()
                .filter_map(|&index| self.shortcut_snapshot.rows.get(index))
                .map(|row| SettingsTarget::Shortcut(row.slot_id.clone()))
                .collect(),
            Section::Integrations => {
                // Display order: the Popular apps strip, built-ins, connected
                // servers, then the add row. The directory is the Apps page.
                let mut targets: Vec<SettingsTarget> = Vec::new();
                if !self.catalog_featured.is_empty() {
                    targets.push(SettingsTarget::McpCatalogSeeAll);
                    targets.extend(
                        self.catalog_featured
                            .iter()
                            .map(|entry| SettingsTarget::McpFeatured(entry.id.clone())),
                    );
                }
                targets.extend(
                    self.integrations
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .filter(|integration| integration_is_visible(integration))
                        .flat_map(|integration| {
                            [
                                integration_can_setup(integration).then(|| {
                                    SettingsTarget::IntegrationSetup(integration.id.clone())
                                }),
                                integration_can_toggle(integration)
                                    .then(|| SettingsTarget::Integration(integration.id.clone())),
                            ]
                            .into_iter()
                            .flatten()
                        }),
                );
                targets.extend(
                    self.mcp_servers
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .map(|server| SettingsTarget::McpServer(server.name.clone())),
                );
                targets.push(SettingsTarget::McpAdd);
                targets
            }
            Section::Apps => {
                // Display order: Popular, then the directory by category (or
                // the flat list of matches while searching).
                let mut targets: Vec<SettingsTarget> = Vec::new();
                if self.catalog_query.is_empty() {
                    targets.extend(
                        self.catalog_featured
                            .iter()
                            .map(|entry| SettingsTarget::McpFeatured(entry.id.clone())),
                    );
                }
                targets.extend(
                    self.catalog_display_order()
                        .into_iter()
                        .map(|entry| SettingsTarget::McpCatalog(entry.id.clone())),
                );
                targets
            }
            Section::Tools => self.tool_targets(),
            Section::Remote | Section::About => Vec::new(),
        }
    }

    /// Re-derive the Application Vim target after the visible controls
    /// changed. Data loads call this from background tasks, so it must not
    /// move a sidebar cursor the user has parked on another section: only the
    /// pane region is bound to the selected section.
    pub(super) fn reconcile_application_vim_target(&mut self) {
        if !self.settings.application_vim_enabled {
            return;
        }
        if self.application_vim.region == SettingsRegion::Pane {
            self.application_vim.section = self.section;
        }
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
                let sections = self.visible_sections();
                if sections.is_empty() {
                    return;
                }
                let current = sections
                    .iter()
                    .position(|section| *section == self.application_vim.section)
                    .unwrap_or(0);
                let index = stepped_index(Some(current), sections.len(), direction, count);
                let next = sections[index];
                self.application_vim.section = next;
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
                let sections = self.visible_sections();
                if sections.is_empty() {
                    return;
                }
                let next = if first {
                    sections[0]
                } else {
                    sections[sections.len() - 1]
                };
                self.application_vim.section = next;
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
                self.shortcut_notice = Some(UiText::key("settings-navigation-no-region"));
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
        let target = self.application_vim.target.clone();
        if target
            .as_ref()
            .is_some_and(|target| !self.visible_application_targets().contains(target))
        {
            // A dialog or a reload hid the target since it was selected.
            return;
        }
        match target {
            Some(SettingsTarget::General(target)) => self.activate_general_target(target, cx),
            Some(SettingsTarget::Appearance(target)) => self.activate_appearance_target(target, cx),
            #[cfg(feature = "acp")]
            Some(SettingsTarget::DeveloperTab(tab)) => self.select_developer_tab(tab, cx),
            Some(SettingsTarget::Account(target)) => {
                self.activate_account_target(target, window, cx)
            }
            Some(SettingsTarget::Billing(target)) => {
                self.activate_billing_target(target, window, cx)
            }
            Some(SettingsTarget::ApiKeys(target)) => {
                self.activate_api_keys_target(target, window, cx)
            }
            #[cfg(feature = "proxy")]
            Some(SettingsTarget::LocalApi(target)) => {
                self.activate_local_api_target(target, window, cx)
            }
            #[cfg(feature = "acp")]
            Some(SettingsTarget::Acp(target)) => self.activate_acp_target(target, cx),
            Some(SettingsTarget::Shortcut(slot_id)) => self.begin_shortcut_recording(slot_id, cx),
            Some(SettingsTarget::PromptEditor) => {
                let handle = self.prompt_editor.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            Some(SettingsTarget::PromptSave) => self.save_prompt(cx),
            Some(SettingsTarget::PromptClear) => self.clear_prompt(cx),
            Some(SettingsTarget::Integration(id)) => self.toggle_integration(&id, cx),
            Some(SettingsTarget::IntegrationSetup(id)) => self.setup_integration(&id, cx),
            Some(SettingsTarget::Tool(id)) => self.toggle_tool(&id, cx),
            Some(SettingsTarget::McpAdd) => self.open_mcp_editor(None, cx),
            Some(SettingsTarget::McpServer(name)) => self.edit_mcp_server(&name, cx),
            Some(SettingsTarget::McpCatalog(id) | SettingsTarget::McpFeatured(id)) => {
                self.connect_catalog_entry(&id, cx)
            }
            Some(SettingsTarget::McpCatalogSeeAll) => self.open_apps_directory(cx),
            Some(SettingsTarget::PageRefresh) => self.refresh_page(cx),
            None => {}
        }
    }

    fn activate_general_target(&mut self, target: GeneralTarget, cx: &mut Context<Self>) {
        match target {
            GeneralTarget::Language => self.toggle_setting_menu(SettingMenu::Language, cx),
            GeneralTarget::Notifications => self.toggle_desktop_notifications(cx),
            GeneralTarget::PasswordNotificationDetails => {
                self.toggle_password_notification_details(cx);
            }
            GeneralTarget::ComposerVim => self.toggle_composer_vim(cx),
            GeneralTarget::ApplicationVim => {
                let enabled = !self.settings.application_vim_enabled;
                self.set_application_vim_enabled(enabled, cx);
            }
            GeneralTarget::Voice => self.toggle_setting_menu(SettingMenu::Voice, cx),
            GeneralTarget::SpeechSpeed => self.toggle_setting_menu(SettingMenu::SpeechSpeed, cx),
        }
    }

    fn activate_appearance_target(&mut self, target: AppearanceTarget, cx: &mut Context<Self>) {
        match target {
            AppearanceTarget::Theme(preference) => self.choose_theme(preference, cx),
            AppearanceTarget::ReduceMotion => self.toggle_reduce_motion(cx),
            AppearanceTarget::ChatFont => self.toggle_setting_menu(SettingMenu::ChatFont, cx),
            AppearanceTarget::ChatSize => self.toggle_setting_menu(SettingMenu::ChatSize, cx),
            AppearanceTarget::ToolDetails => self.toggle_tool_details(cx),
            AppearanceTarget::ToolSummaries => self.toggle_tool_summaries(cx),
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
        // An open dropdown closes first. It normally has focus and closes
        // on its own Escape binding; this covers one that has not taken
        // focus yet.
        if self.popup.close(cx) {
            return;
        }
        if self.mcp_editor.is_some() {
            self.close_mcp_editor(cx);
            return;
        }
        if self.mcp_address.is_some() {
            self.close_mcp_address(cx);
            return;
        }
        if self.account.delete != super::account::DeleteFlow::Idle {
            self.dismiss_delete_dialog(cx);
            return;
        }
        if self.advanced.erase {
            self.close_erase_dialog(cx);
            return;
        }
        if self.advanced.form.is_some() {
            self.dismiss_password_form(cx);
            return;
        }
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
        self.go_back(cx);
    }

    fn application_text_input_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        let focused = window.focused(cx);
        let mut inputs = vec![
            &self.sidebar_filter,
            &self.prompt_editor,
            &self.shortcut_search,
        ];
        if self.section == Section::Api {
            inputs.push(&self.api_keys.name);
            #[cfg(feature = "proxy")]
            inputs.push(&self.local_api.port);
        }
        if let Some(editor) = &self.mcp_editor {
            inputs.extend([
                &editor.name,
                &editor.description,
                &editor.target,
                &editor.environment,
                &editor.headers,
            ]);
        }
        if let Some(prompt) = &self.mcp_address {
            inputs.push(&prompt.input);
        }
        if self.advanced.erase {
            inputs.push(&self.advanced.confirmation);
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
                self.shortcut_notice = Some(UiText::key("settings-navigation-unavailable"));
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
            self.shortcut_notice = Some(UiText::key("settings-navigation-count-limit"));
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
    use super::super::{SettingsClosed, option_id};
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
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
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
                    name: "Computer use".to_string(),
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
                    backend: Some(maple_agent::agent::AgentIntegrationBackend::Embedded),
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
                    backend: None,
                },
            ]);
            this.mcp_servers = Some(vec![maple_agent::store::records::McpDefinition {
                key: "custom".to_string(),
                name: "custom".to_string(),
                description: String::new(),
                enabled: true,
                timeout_seconds: 300,
                target: maple_agent::store::records::McpTarget::Stdio {
                    command: "custom-mcp".to_string(),
                },
                environment: Vec::new(),
                headers: Vec::new(),
            }]);

            assert_eq!(
                this.visible_application_targets(),
                std::iter::once(SettingsTarget::McpCatalogSeeAll)
                    .chain(
                        this.catalog_featured
                            .iter()
                            .map(|entry| SettingsTarget::McpFeatured(entry.id.clone()))
                    )
                    .chain([
                        SettingsTarget::IntegrationSetup("cua-driver".to_string()),
                        SettingsTarget::Integration("cua-driver".to_string()),
                        SettingsTarget::McpServer("custom".to_string()),
                        SettingsTarget::McpAdd,
                    ])
                    .collect::<Vec<_>>()
            );

            // The Apps page: Popular, then every category in catalog order.
            this.section = Section::Apps;
            assert_eq!(
                this.visible_application_targets(),
                this.catalog_featured
                    .iter()
                    .map(|entry| SettingsTarget::McpFeatured(entry.id.clone()))
                    .chain(
                        this.catalog_display_order()
                            .into_iter()
                            .map(|entry| SettingsTarget::McpCatalog(entry.id.clone()))
                    )
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                this.catalog_display_order().len(),
                maple_agent::agent::mcp_catalog().len()
            );
        });
    }

    /// The strip and the directory list the same apps under different
    /// targets, so `j` from the first catalog card must land on the second
    /// card, not resolve to the strip tile and jump back up.
    #[gpui::test]
    fn featured_strip_drops_connected_apps_and_keeps_catalog_motion_linear(
        cx: &mut TestAppContext,
    ) {
        let (settings, cx) = dropdown_window(cx, true);
        settings.update(cx, |this, cx| {
            this.select_section(Section::Integrations, cx);
            let notion = maple_agent::agent::mcp_catalog()
                .iter()
                .find(|entry| entry.id == "notion")
                .expect("notion in catalog");
            this.mcp_servers = Some(vec![maple_agent::store::records::McpDefinition {
                key: maple_agent::agent::AgentMcpServer::key_for_name(&notion.name),
                name: notion.name.clone(),
                description: String::new(),
                enabled: true,
                timeout_seconds: 300,
                target: maple_agent::store::records::McpTarget::Stdio {
                    command: "notion-mcp".to_string(),
                },
                environment: Vec::new(),
                headers: Vec::new(),
            }]);
            this.refresh_catalog();

            let featured: Vec<&str> = this
                .catalog_featured
                .iter()
                .map(|entry| entry.id.as_str())
                .collect();
            assert_eq!(
                featured,
                ["stripe", "cloudflare", "vercel", "dropbox"],
                "connected apps leave the strip and nothing backfills"
            );
            assert!(
                !this
                    .catalog_visible
                    .iter()
                    .any(|entry| entry.id == "notion"),
                "connected apps leave the directory too"
            );
        });

        // On the Apps page, start from a directory row whose app is also in
        // Popular: a shared target would resolve to the Popular row and jump
        // back up.
        cx.update(|window, app| {
            settings.update(app, |this, cx| {
                this.section = Section::Apps;
                let order = this.catalog_display_order();
                let index = order
                    .iter()
                    .position(|entry| this.catalog_featured.iter().any(|f| f.id == entry.id))
                    .expect("a featured app is in the directory");
                let start = order[index].id.clone();
                let next = order[index + 1].id.clone();
                this.application_vim.region = SettingsRegion::Pane;
                this.application_vim.target = Some(SettingsTarget::McpCatalog(start));
                this.move_application_selection(1, 1, window, cx);
                assert_eq!(
                    this.application_vim.target,
                    Some(SettingsTarget::McpCatalog(next))
                );
            });
        });

        // No parking here: the test backend's server list would land and
        // replace the connected Notion above.
        settings.update(cx, |this, cx| {
            this.section = Section::Integrations;
            this.catalog_query = "git".to_string();
            this.refresh_catalog();
            assert!(this.catalog_visible.len() < maple_agent::agent::mcp_catalog().len() - 1);
            this.open_apps_directory(cx);
            assert_eq!(this.section, Section::Apps);
            assert!(this.catalog_query.is_empty());
            assert_eq!(
                this.catalog_visible.len(),
                maple_agent::agent::mcp_catalog().len() - 1,
                "See all shows every app that is not connected"
            );
        });
    }

    #[gpui::test]
    fn tool_rows_are_targets_and_enter_switches_one(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, true);
        let switch =
            |id: &str, enabled: bool, available: bool| maple_agent::agent::AgentToolSwitch {
                id: id.to_string(),
                enabled,
                enabled_by_default: enabled,
                available,
            };
        cx.update(|window, app| {
            settings.update(app, |this, cx| {
                this.select_section(Section::Tools, cx);
                this.tools.switches = Some(vec![
                    switch("read", true, true),
                    // A tool this computer cannot offer has no row.
                    switch("python", false, false),
                    switch("grep", false, true),
                ]);
                assert_eq!(
                    this.visible_application_targets(),
                    [
                        SettingsTarget::Tool("read".to_string()),
                        SettingsTarget::Tool("grep".to_string()),
                    ]
                );
                this.application_vim.region = SettingsRegion::Pane;
                this.application_vim.target = Some(SettingsTarget::Tool("grep".to_string()));
                this.execute_application_vim(SettingsApplicationCommand::Activate, window, cx);
                assert!(
                    this.tools.saving.contains("grep"),
                    "Enter switches the row, as a click does"
                );
            })
        });
    }

    #[gpui::test]
    fn application_vim_off_keeps_settings_projection_empty(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
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
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
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

                // General starts at Language: the web switch moved to Tools,
                // and theme has its own page.
                assert_eq!(
                    this.visible_application_targets().first(),
                    Some(&SettingsTarget::General(GeneralTarget::Language))
                );
                this.move_application_selection(1, 1, window, cx);
                assert_eq!(
                    this.application_vim.target,
                    Some(SettingsTarget::General(GeneralTarget::Notifications))
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
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
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
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
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

    /// The General pane starts at Appearance: there is no "Default
    /// permission mode" row because the composer's chip remembers the mode,
    /// and no web row because Settings → Tools has the web switch.
    #[gpui::test]
    fn general_settings_have_no_permission_row(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, true);
        assert!(
            cx.debug_bounds("setting-value-default-permission-mode")
                .is_none()
        );
        assert!(
            cx.debug_bounds("setting-value-language").is_some(),
            "the other General rows still render"
        );
        settings.update(cx, |this, _| {
            assert_eq!(
                this.visible_application_targets().first(),
                Some(&SettingsTarget::General(GeneralTarget::Language))
            );
        });
    }

    /// The content area below the title bar bounds the pane: General is
    /// taller than the window, so the pane has a scroll range and a wheel
    /// over it moves its content.
    #[gpui::test]
    fn the_pane_scrolls_below_the_title_bar(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, false);
        let content = cx
            .debug_bounds("settings-content")
            .expect("settings content");
        let range = settings.update(cx, |this, _| this.pane_scroll.max_offset().y);
        assert!(range > px(0.), "General overflows its content area");
        cx.simulate_event(gpui::ScrollWheelEvent {
            // In the pane's gutter, so no row takes the wheel.
            position: gpui::point(content.left() + px(8.), content.center().y),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-200.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let offset = settings.update(cx, |this, _| this.pane_scroll.offset().y);
        assert!(offset < px(0.), "the wheel scrolls the pane");
    }

    /// Sign out lives in Account only, so it must not wait for the account:
    /// while the profile loads, or after it fails to, the session can still
    /// end from Settings.
    #[gpui::test]
    fn account_offers_sign_out_before_the_account_loads(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, false);
        settings.update(cx, |this, cx| this.select_section(Section::Account, cx));
        cx.run_until_parked();
        assert!(settings.update(cx, |this, _| this.account.info.is_none()));
        assert!(
            cx.debug_bounds("account-sign-out").is_some(),
            "while loading"
        );
        settings.update(cx, |this, cx| {
            this.account.load_error = Some("offline".into());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("account-sign-out").is_some(),
            "after a failed load"
        );
    }

    #[gpui::test]
    fn spanish_general_preserves_controls_and_saved_values(cx: &mut TestAppContext) {
        let _localization = crate::ui::i18n::TestLocalizationGuard::enable();
        crate::ui::i18n::initialize_saved_preference(Some("es"));
        let (settings, cx) = dropdown_window(cx, true);
        cx.run_until_parked();
        assert!(cx.debug_bounds("setting-value-language").is_some());
        settings.update(cx, |this, cx| {
            this.select_section(Section::Appearance, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("setting-appearance-system").is_some());
        assert!(cx.debug_bounds("setting-value-text-size").is_some());
        settings.update(cx, |this, cx| {
            assert_eq!(this.menu_value(SettingMenu::ChatFont), "Sistema");
            let voices = this.menu_options(SettingMenu::Voice);
            assert_eq!(voices.len(), crate::settings::TTS_VOICES.len());
            assert_eq!(voices[8].label, "Con acento español — Femenina");
            this.pick_setting_option(SettingMenu::Voice, 8, cx);
            assert_eq!(this.settings.tts_voice, "es_female");
            assert_eq!(this.menu_value(SettingMenu::Voice), voices[8].label);
            this.choose_theme(crate::ui::theme::Preference::Dark, cx);
            assert_eq!(this.settings.theme, "dark");
            assert_eq!(Section::General.id(), "General");
            assert_eq!(Section::Account.label().as_ref(), "Cuenta y seguridad");
        });
    }

    #[gpui::test]
    fn live_language_switch_preserves_forms_and_rebuilds_spanish_shortcut_search(
        cx: &mut TestAppContext,
    ) {
        let _locale = crate::ui::i18n::TestLocalizationGuard::enable();
        crate::ui::i18n::apply_preference(Some("en"));
        let (settings, cx) = dropdown_window(cx, false);
        settings.update(cx, |this, cx| {
            this.prompt_editor
                .update(cx, |input, cx| input.set_text("Keep my instructions", cx));
            this.api_keys
                .name
                .update(cx, |input, cx| input.set_text("My editor", cx));
            this.prompt_notice = Some(UiText::key(
                "settings-settings-saved-new-tasks-use-these-instructions",
            ));
            this.shortcut_snapshot.rows = vec![crate::shortcuts::ShortcutRow {
                slot_id: "chat.new_task".into(),
                label: "New task".into(),
                category: "Chat".into(),
                context: Some("Chat".into()),
                default_sequence: "secondary-n".into(),
                current_sequence: Some("secondary-n".into()),
                modified: false,
                conflicts: vec![],
            }];
            this.shortcut_search
                .update(cx, |input, cx| input.set_text("nueva", cx));
        });
        cx.run_until_parked();
        for (locale, visible) in [("es", 1), ("en", 0), ("es", 1)] {
            cx.update(|_, cx| {
                crate::ui::i18n::apply_preference(Some(locale));
                cx.refresh_windows();
            });
            cx.run_until_parked();
            settings.update(cx, |this, cx| {
                assert_eq!(this.prompt_editor.read(cx).text(), "Keep my instructions");
                assert_eq!(this.api_keys.name.read(cx).text(), "My editor");
                assert_eq!(this.shortcut_list_cache.visible_indices.len(), visible);
                assert_eq!(this.shortcut_snapshot.rows[0].slot_id, "chat.new_task");
                assert_eq!(
                    this.shortcut_snapshot.rows[0].current_sequence.as_deref(),
                    Some("secondary-n")
                );
                assert!(
                    this.prompt_notice
                        .as_ref()
                        .unwrap()
                        .resolve()
                        .starts_with(if locale == "es" { "Guardado" } else { "Saved" })
                );
            });
        }
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
            settings.update(cx, |this, _| this.popup.highlighted().cloned()),
            Some(option_id(SettingMenu::Voice, saved).into())
        );

        // Down advances one option, wrapping; Enter picks it and closes.
        cx.simulate_keystrokes("down");
        let picked = (saved + 1) % crate::settings::TTS_VOICES.len();
        assert_eq!(
            settings.update(cx, |this, _| this.popup.highlighted().cloned()),
            Some(option_id(SettingMenu::Voice, picked).into())
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
            this.toggle_setting_menu(SettingMenu::Language, cx);
            assert_eq!(this.popup.open_key(), Some(&SettingMenu::Language));
        });
    }

    /// Issue #997: a second press on the value button closes its dropdown.
    #[gpui::test]
    fn a_second_press_on_a_dropdown_closes_it(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, false);
        for open in [Some(SettingMenu::Language), None] {
            let bounds = cx
                .debug_bounds("setting-value-language")
                .expect("the language value button renders");
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
            settings.update(cx, |this, _| this.popup.highlighted().cloned())
        };
        let speed = |index| Some(option_id(SettingMenu::SpeechSpeed, index).into());
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
        assert_eq!(
            highlighted(cx),
            speed(crate::settings::TTS_SPEEDS.len() - 1)
        );
        cx.simulate_keystrokes("g g j");
        assert_eq!(highlighted(cx), speed(1));
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
    fn language_dropdown_supports_pointer_and_keyboard_selection(cx: &mut TestAppContext) {
        let _localization = crate::ui::i18n::TestLocalizationGuard::enable();
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        crate::settings::install_test_settings(crate::settings::AppSettings::default());
        let (settings, cx) = dropdown_window(cx, false);
        for expected in [Some("en"), Some("es"), None] {
            let button = cx
                .debug_bounds("setting-value-language")
                .expect("language selector");
            cx.simulate_click(button.center(), gpui::Modifiers::default());
            cx.simulate_keystrokes("down enter");
            settings.update(cx, |this, _| {
                assert_eq!(this.settings.locale.as_deref(), expected);
                assert_eq!(
                    crate::ui::i18n::tr("settings-language").as_ref(),
                    if expected == Some("es") {
                        "Idioma"
                    } else {
                        "Language"
                    }
                );
                assert_eq!(this.popup.open_key(), None);
            });
        }
        crate::settings::discard_decrypted_cache();
    }

    #[gpui::test]
    fn picking_from_the_dropdown_persists_in_reopened_store(cx: &mut TestAppContext) {
        let _localization = crate::ui::i18n::TestLocalizationGuard::enable();
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        let root = tempfile::tempdir().unwrap();
        let paths = maple_agent::store::StorePaths::new(root.path());
        let store = std::sync::Arc::new(maple_agent::store::Store::open(paths.clone()).unwrap());
        crate::settings::install_test_store(store.clone());
        let settings = dropdown_screen(cx, false);
        let (_host, cx) = cx.add_window_view(|_window, _cx| DropdownHost {
            settings: settings.clone(),
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));

        settings.update(cx, |this, cx| {
            this.choose_theme(crate::ui::theme::Preference::Light, cx);
            this.toggle_setting_menu(SettingMenu::Language, cx);
            this.pick_setting_option(SettingMenu::Language, 2, cx);
        });
        crate::settings::update_settings_and_wait(|_| {});
        // Detach the writer's Arc and discard its cached value before closing
        // the database. The assertion must read the independently reopened
        // authoritative store, even when the in-memory cache disagrees.
        crate::settings::discard_decrypted_cache();
        assert_ne!(crate::settings::load_settings().theme, "light");
        store.close().unwrap();
        drop(store);
        let reopened = maple_agent::store::Store::open(paths).unwrap();
        let saved = reopened
            .app_setting("ui")
            .unwrap()
            .expect("saved UI settings");
        assert_eq!(
            saved.get("theme").and_then(serde_json::Value::as_str),
            Some("light"),
            "a dropdown pick must reach the authoritative store"
        );
        assert_eq!(
            saved.get("locale").and_then(serde_json::Value::as_str),
            Some("es")
        );
        reopened.close().unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    fn sidebar_filter_narrows_sections_and_handles_empty_matches(cx: &mut TestAppContext) {
        let _localization = crate::ui::i18n::TestLocalizationGuard::enable();
        crate::ui::i18n::apply_preference(Some("en"));
        let (settings, cx) = dropdown_window(cx, true);

        settings.update(cx, |this, cx| {
            assert_eq!(this.visible_sections(), Section::ALL.to_vec());
            this.sidebar_filter
                .update(cx, |input, cx| input.set_text("appearance", cx));
        });
        cx.run_until_parked();
        settings.update(cx, |this, _| {
            assert_eq!(this.visible_sections(), vec![Section::Appearance]);
            assert_eq!(this.application_vim.section, Section::Appearance);
        });

        // Empty search matches must leave navigation motion safe without panicking.
        settings.update(cx, |this, cx| {
            this.sidebar_filter
                .update(cx, |input, cx| input.set_text("zzzz-unmatched", cx));
        });
        cx.run_until_parked();
        cx.update(|window, app| {
            settings.update(app, |this, cx| {
                assert!(this.visible_sections().is_empty());
                this.move_application_selection(1, 1, window, cx);
                this.select_application_edge(false, window, cx);
            });
        });
    }

    /// Erasing local data confirms in a dialog: the field is focused, the
    /// destructive button is inert until ERASE is typed, Escape and the
    /// backdrop close it and clear the field, and a busy erase ignores close.
    #[gpui::test]
    fn erase_dialog_focuses_gates_and_closes(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, true);
        settings.update(cx, |this, cx| {
            this.select_section(Section::Account, cx);
            this.advanced.status = Some(
                serde_json::from_value(serde_json::json!({
                    "state": {"state": "open"},
                    "mode": "plain",
                    "store_id": "current",
                    "password_settings_available": true
                }))
                .expect("store status"),
            );
            assert!(this.advanced.available());
            this.open_erase_dialog(cx);
            assert!(this.advanced.erase);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("advanced-erase-dialog").is_some());
        assert!(
            cx.debug_bounds("advanced-erase").is_some(),
            "the row's button renders"
        );
        cx.update(|window, app| {
            settings.update(app, |this, cx| {
                let field = this.advanced.confirmation.read(cx).focus_handle(cx);
                assert!(field.is_focused(window), "the ERASE field is focused");
                this.advanced
                    .confirmation
                    .update(cx, |input, cx| input.set_text("ERAS", cx));
            });
        });
        cx.run_until_parked();
        // A wrong word keeps the erase inert: nothing is sent.
        settings.update(cx, |this, cx| {
            let before = this.advanced.busy;
            this.erase_local_store(cx);
            assert_eq!(
                this.advanced.busy, before,
                "no erase starts on a wrong word"
            );
            assert!(
                this.advanced.notice.is_some(),
                "and the dialog explains why"
            );
        });
        // With Application Vim on the first Escape leaves the field, the
        // second reaches the dialog. Nothing underneath is a Vim target.
        settings.update(cx, |this, _| {
            assert!(this.visible_application_targets().is_empty())
        });
        cx.simulate_keystrokes("escape escape");
        cx.run_until_parked();
        settings.update(cx, |this, cx| {
            assert!(!this.advanced.erase, "Escape closes the dialog");
            assert!(this.advanced.confirmation.read(cx).text().is_empty());
            // Busy erases ignore close.
            this.open_erase_dialog(cx);
            this.advanced.busy = true;
            this.close_erase_dialog(cx);
            assert!(this.advanced.erase);
            this.advanced.busy = false;
            this.close_erase_dialog(cx);
            assert!(!this.advanced.erase);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("advanced-erase-dialog").is_none());
    }

    /// The unlock-password form opens as a dialog and dismisses through the
    /// form's own close, which is a no-op while an operation runs.
    #[gpui::test]
    fn password_form_opens_as_a_dialog_and_dismisses_through_the_form(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, false);
        settings.update(cx, |this, cx| {
            this.select_section(Section::Account, cx);
            this.advanced.status = Some(
                serde_json::from_value(serde_json::json!({
                    "state": {"state": "open"},
                    "mode": "plain",
                    "store_id": "current",
                    "password_settings_available": true
                }))
                .expect("store status"),
            );
            this.open_password_form(crate::ui::store_gate::PasswordAction::Set, cx);
            assert!(this.advanced.form.is_some(), "the form opened");
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("advanced-password-dialog").is_some());
        settings.update(cx, |this, cx| {
            this.dismiss_password_form(cx);
        });
        cx.run_until_parked();
        settings.update(cx, |this, _| {
            assert!(this.advanced.form.is_none(), "an idle form dismisses");
        });
        assert!(cx.debug_bounds("advanced-password-dialog").is_none());
    }

    /// Account deletion runs in a dialog: it focuses the acknowledgement
    /// field, Escape and the backdrop cancel it before a code is emailed but
    /// not after, and while it is open Vim sees only its controls.
    #[gpui::test]
    fn delete_account_dialog_focuses_scopes_and_guards_the_code_step(cx: &mut TestAppContext) {
        use super::super::account::{AccountTarget, DeleteFlow};
        let (settings, cx) = dropdown_window(cx, true);
        settings.update(cx, |this, cx| {
            this.select_section(Section::Account, cx);
            this.account.info = Some(crate::backend::MapleAccount {
                user_id: "u".into(),
                email: Some("a@b.c".into()),
                name: None,
                email_verified: true,
                login_method: crate::backend::MapleLoginMethod::Email,
                created_at: String::new(),
            });
            assert!(
                this.visible_application_targets()
                    .contains(&SettingsTarget::Account(AccountTarget::DeleteStart)),
                "the quiet row is reachable"
            );
            this.begin_account_deletion(cx);
            assert_eq!(
                this.visible_application_targets(),
                vec![
                    SettingsTarget::Account(AccountTarget::DeleteAcknowledge),
                    SettingsTarget::Account(AccountTarget::DeleteCancel),
                    SettingsTarget::Account(AccountTarget::DeleteRequestCode),
                ],
                "only the dialog's controls while it is open"
            );
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("account-delete-dialog").is_some());
        cx.update(|window, app| {
            settings.update(app, |this, cx| {
                let ack = this.account.delete_ack.read(cx).focus_handle(cx);
                assert!(
                    ack.is_focused(window),
                    "the acknowledgement field is focused"
                );
                this.account
                    .delete_ack
                    .update(cx, |input, cx| input.set_text("DEL", cx));
            });
        });

        // Escape before a code is emailed cancels and clears the field. With
        // Application Vim on, the first Escape leaves the text field (as in
        // every Settings field); the second reaches the dialog.
        cx.simulate_keystrokes("escape escape");
        cx.run_until_parked();
        settings.update(cx, |this, cx| {
            assert_eq!(this.account.delete, DeleteFlow::Idle);
            assert!(this.account.delete_ack.read(cx).text().is_empty());
        });
        assert!(cx.debug_bounds("account-delete-dialog").is_none());

        // After the code is emailed, a light dismissal keeps the flow; only
        // Cancel ends it. A busy flow ignores both.
        settings.update(cx, |this, cx| {
            this.account.delete = DeleteFlow::CodeSent {
                secret: "s".to_string(),
            };
            this.dismiss_delete_dialog(cx);
            assert!(matches!(this.account.delete, DeleteFlow::CodeSent { .. }));
            this.account.delete_busy = true;
            this.cancel_account_deletion(cx);
            assert!(matches!(this.account.delete, DeleteFlow::CodeSent { .. }));
            this.account.delete_busy = false;
            this.cancel_account_deletion(cx);
            assert_eq!(this.account.delete, DeleteFlow::Idle);
        });
    }

    /// Back unwinds in-app links (See all apps, Manage plan) before it closes
    /// Settings; a sidebar choice resets that.
    #[gpui::test]
    fn back_returns_to_the_page_an_in_app_link_left(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, false);
        let closed = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = closed.clone();
        cx.update(|_, app| {
            app.subscribe(&settings, move |_, _: &SettingsClosed, _| {
                seen.set(seen.get() + 1)
            })
            .detach();
        });
        settings.update(cx, |this, cx| {
            this.select_section(Section::Integrations, cx);
            this.open_apps_directory(cx);
            assert_eq!(this.section, Section::Apps);
            this.go_back(cx);
            assert_eq!(
                this.section,
                Section::Integrations,
                "Back returns to Integrations"
            );
            assert_eq!(closed.get(), 0, "Settings stays open");

            this.navigate_to(Section::Billing, cx);
            this.navigate_to(Section::Billing, cx);
            assert_eq!(
                this.section_history,
                vec![Section::Integrations],
                "no duplicate entry"
            );
            this.navigate_to(Section::Apps, cx);
            assert_eq!(
                this.section_history,
                vec![Section::Integrations, Section::Billing],
                "a second hop keeps the first"
            );
            this.go_back(cx);
            this.go_back(cx);
            assert_eq!(this.section, Section::Integrations);
            this.select_section(Section::General, cx);
            assert!(
                this.section_history.is_empty(),
                "a sidebar choice resets history"
            );
            this.go_back(cx);
        });
        cx.run_until_parked();
        assert_eq!(closed.get(), 1, "with nothing to unwind, Back closes");
    }

    /// Coming back from a portal or checkout tab refetches billing while the
    /// handoff is recent; otherwise activation does nothing.
    #[gpui::test]
    fn window_activation_refetches_billing_only_after_a_recent_handoff(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, false);
        cx.run_until_parked();

        // No handoff: nothing is fetched.
        settings.update(cx, |this, cx| {
            let before = this.billing.loads;
            this.billing.awaiting_browser = None;
            this.on_window_activated(cx);
            assert_eq!(this.billing.loads, before);
        });

        // A stale handoff is dropped without a fetch.
        settings.update(cx, |this, cx| {
            this.billing.awaiting_browser =
                Some(std::time::Instant::now() - SettingsScreen::BROWSER_HANDOFF_WINDOW * 2);
            let before = this.billing.loads;
            this.on_window_activated(cx);
            assert!(this.billing.awaiting_browser.is_none());
            assert_eq!(this.billing.loads, before);
        });

        // A recent handoff fetches, keeps the notice, and stays armed for the
        // next return.
        settings.update(cx, |this, cx| {
            this.billing.awaiting_browser = Some(std::time::Instant::now());
            this.billing.notice = Some(crate::ui::i18n::UiText::from("opened"));
            let before = this.billing.loads;
            this.on_window_activated(cx);
            assert_eq!(this.billing.loads, before + 1, "a billing fetch ran");
            assert!(this.billing.notice.is_some(), "the handoff notice stays");
            assert!(this.billing.awaiting_browser.is_some());
        });
    }

    /// Background loads (billing, profile, keys…) re-reconcile the Vim
    /// target when they land. That must not yank a sidebar cursor the user
    /// moved away from the selected section back onto it.
    #[gpui::test]
    fn data_loads_keep_the_sidebar_vim_cursor_where_the_user_left_it(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, true);
        settings.update(cx, |this, _| {
            assert_eq!(this.section, Section::General);
            this.application_vim.region = SettingsRegion::Navigation;
            this.application_vim.section = Section::Appearance;

            this.reconcile_application_vim_target();
            assert_eq!(this.application_vim.section, Section::Appearance);

            this.application_vim.region = SettingsRegion::Pane;
            this.reconcile_application_vim_target();
            assert_eq!(this.application_vim.section, Section::General);
        });
    }

    #[gpui::test]
    fn appearance_segmented_theme_switches_on_pointer_and_vim_activation(cx: &mut TestAppContext) {
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        crate::settings::install_test_settings(crate::settings::AppSettings::default());
        let (settings, cx) = dropdown_window(cx, true);
        // The Settings header Sign out is removed; Sign out lives only in Account.
        assert!(cx.debug_bounds("settings-sign-out").is_none());

        settings.update(cx, |this, cx| {
            this.select_section(Section::Appearance, cx);
        });
        cx.run_until_parked();

        let dark = cx
            .debug_bounds("setting-appearance-dark")
            .expect("Dark segmented pill renders");
        cx.simulate_click(dark.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        settings.update(cx, |this, _| {
            assert_eq!(this.settings.theme, "dark");
            assert_eq!(this.theme, crate::ui::theme::Preference::Dark);
        });

        cx.update(|window, app| {
            settings.update(app, |this, cx| {
                this.application_vim.region = SettingsRegion::Pane;
                this.application_vim.target = Some(SettingsTarget::Appearance(
                    AppearanceTarget::Theme(crate::ui::theme::Preference::Light),
                ));
                this.execute_application_vim(SettingsApplicationCommand::Activate, window, cx);
                assert_eq!(this.settings.theme, "light");
                assert_eq!(this.theme, crate::ui::theme::Preference::Light);
            });
        });
        crate::settings::discard_decrypted_cache();
    }

    /// Without Application Vim, the pills are ordinary tab stops: Tab walks
    /// them and Space or Enter presses the focused one. (Under Application
    /// Vim, Space is the leader prefix and Enter activates the Vim target.)
    #[gpui::test]
    fn appearance_theme_pills_are_keyboard_tab_stops(cx: &mut TestAppContext) {
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        crate::settings::install_test_settings(crate::settings::AppSettings::default());
        let (settings, cx) = dropdown_window(cx, false);
        settings.update(cx, |this, cx| {
            this.select_section(Section::Appearance, cx);
        });
        cx.run_until_parked();

        // Clicking Dark focuses it; Tab then reaches Light.
        let dark = cx
            .debug_bounds("setting-appearance-dark")
            .expect("Dark segmented pill renders");
        cx.simulate_click(dark.center(), gpui::Modifiers::default());
        cx.simulate_keystrokes("tab space");
        cx.run_until_parked();
        settings.update(cx, |this, _| {
            assert_eq!(this.settings.theme, "light");
            assert_eq!(this.theme, crate::ui::theme::Preference::Light);
        });
        cx.simulate_keystrokes("shift-tab enter");
        cx.run_until_parked();
        settings.update(cx, |this, _| {
            assert_eq!(this.settings.theme, "dark");
        });
        crate::settings::discard_decrypted_cache();
    }

    /// A stalled or failed profile fetch must not take Sign out with it:
    /// the page header no longer has one.
    #[gpui::test]
    fn sign_out_renders_while_the_profile_is_loading(cx: &mut TestAppContext) {
        let (settings, cx) = dropdown_window(cx, false);
        settings.update(cx, |this, cx| {
            assert!(
                this.account.info.is_none(),
                "unreachable backend: no profile"
            );
            this.select_section(Section::Account, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("account-sign-out").is_some());
    }

    /// The "Add custom server" row is a Tab stop and opens the editor from
    /// the keyboard; the directory lives on the Apps page, not here.
    #[gpui::test]
    fn integrations_add_row_is_a_keyboard_tab_stop_and_directory_is_the_apps_page(
        cx: &mut TestAppContext,
    ) {
        let (settings, cx) = dropdown_window(cx, false);
        settings.update(cx, |this, cx| {
            this.select_section(Section::Integrations, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("mcp-add").is_some(), "add row renders");
        let first = &maple_agent::agent::mcp_catalog()[0];
        assert_eq!(
            first.id, "notion",
            "the literal selector below tracks the first entry"
        );
        let first_card = "catalog-card-notion";
        assert!(
            cx.debug_bounds(first_card).is_none(),
            "the directory is not on Integrations"
        );
        assert!(
            cx.debug_bounds("catalog-see-all").is_some(),
            "the strip links to the Apps page"
        );
        // Tab out of the sidebar filter (tab 0) lands on the add row (tab 1);
        // Space presses it.
        cx.update(|window, app| {
            settings.update(app, |this, cx| {
                let handle = this.sidebar_filter.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            });
        });
        cx.simulate_keystrokes("tab space");
        cx.run_until_parked();
        settings.update(cx, |this, cx| {
            assert!(this.mcp_editor.is_some());
            this.mcp_editor = None;
            cx.notify();
        });
        cx.run_until_parked();
        let add = cx.debug_bounds("mcp-add").unwrap();
        cx.simulate_click(add.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        settings.update(cx, |this, _| assert!(this.mcp_editor.is_some()));
        // The form is a dialog over the page: the page does not move, and
        // Escape closes it.
        assert!(cx.debug_bounds("mcp-editor").is_some(), "dialog renders");
        assert_eq!(
            cx.debug_bounds("mcp-add").unwrap().origin,
            add.origin,
            "opening the dialog does not scroll the page"
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        settings.update(cx, |this, _| assert!(this.mcp_editor.is_none()));
        assert!(cx.debug_bounds("mcp-editor").is_none());

        // The Apps page renders the directory with its search.
        settings.update(cx, |this, cx| {
            this.mcp_editor = None;
            this.select_section(Section::Apps, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("catalog-search").is_some());
        assert!(cx.debug_bounds(first_card).is_some());
    }

    /// Former page names and moved controls still find their new section.
    #[gpui::test]
    fn sidebar_filter_matches_former_names_and_moved_controls(cx: &mut TestAppContext) {
        let _localization = crate::ui::i18n::TestLocalizationGuard::enable();
        crate::ui::i18n::apply_preference(Some("en"));
        let (settings, cx) = dropdown_window(cx, false);
        for (query, expected) in [
            ("privacy", Section::Account),
            ("usage", Section::Usage),
            ("mcp", Section::Integrations),
            ("acp", Section::Api),
            ("theme", Section::Appearance),
        ] {
            settings.update(cx, |this, cx| {
                this.sidebar_filter
                    .update(cx, |input, cx| input.set_text(query, cx));
            });
            cx.run_until_parked();
            settings.update(cx, |this, _| {
                assert!(
                    this.visible_sections().contains(&expected),
                    "{query:?} should surface {expected:?}, got {:?}",
                    this.visible_sections()
                );
            });
        }
    }
}
