//! Settings screen: left navigation with content panes, following Maple's
//! settings layout. Sections: General (defaults), Account, Additional instructions,
//! Integrations, Keyboard Shortcuts, Usage, About.

use crate::ui::i18n::{UiText, tr};
use std::collections::HashSet;
use std::sync::Arc;

use gpui::{
    App, Context, Div, Entity, EventEmitter, Render, ScrollAnchor, ScrollHandle, Subscription,
    Window, div, prelude::*, px,
};

use maple_agent::agent::{
    AgentIntegration, AgentIntegrationAvailability, AgentIntegrationBackend,
    AgentIntegrationPermissions,
};
use maple_agent::store::records::{McpDefinition, McpServerEdit, McpTarget};
use zeroize::Zeroizing;

use crate::ui::icons::{icon, spinner};
use crate::ui::popup::{Menu, MenuItem, Placement, Popup};
use crate::ui::text_input::TextInput;

use crate::backend::AgentBackend;
use crate::settings::{self, AppSettings};
use crate::shortcuts::{
    ShortcutConflict, ShortcutConflictKind, ShortcutContextOverlap, ShortcutOverrides,
    ShortcutSnapshot,
};
use crate::ui::theme;
use crate::ui::widgets;

mod account;
#[cfg(feature = "acp")]
mod acp;
mod advanced;
mod api_credits;
mod api_keys;
mod billing;
mod general;
use general::{font_label, theme_label, voice_label};
mod catalog;
#[cfg(feature = "proxy")]
mod local_api;
mod mcp;
mod navigation;
pub(crate) mod presentation;
mod shortcut_labels;

use self::account::AccountState;
#[cfg(feature = "acp")]
use self::acp::AcpUiState;
use self::api_keys::ApiKeysState;
use self::billing::BillingState;
#[cfg(feature = "proxy")]
use self::local_api::LocalApiUiState;
use self::mcp::{McpSecretAction, McpSecretField, definition_edit, name_collides};
use self::navigation::{GeneralTarget, SettingsApplicationVimState, SettingsTarget};
pub(crate) use catalog::saved_description;

/// Emitted when the user leaves settings.
pub struct SettingsClosed(pub AppSettings);

/// Emitted when the user clicks Sign out in the settings header.
pub struct SignOutRequested;

/// Emitted after the server confirmed the account is deleted and the
/// local session is gone; the app returns to the login form.
pub struct AccountDeleted;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    General,
    Account,
    Billing,
    Api,
    #[cfg(feature = "acp")]
    Acp,
    Shortcuts,
    Prompt,
    Integrations,
    Remote,
    Usage,
    About,
    Advanced,
}

impl Section {
    fn id(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Account => "Account",
            Self::Billing => "Billing",
            Self::Api => "API",
            #[cfg(feature = "acp")]
            Self::Acp => "ACP",
            Self::Shortcuts => "Keyboard Shortcuts",
            Self::Prompt => "Additional instructions",
            Self::Integrations => "Integrations",
            Self::Remote => "Remote computers",
            Self::Usage => "Usage",
            Self::About => "About",
            Self::Advanced => "Privacy & Security",
        }
    }

    fn label(self) -> gpui::SharedString {
        super::i18n::tr(match self {
            Self::General => "settings-section-general",
            Self::Account => "settings-section-account",
            Self::Billing => "settings-section-billing",
            Self::Api => "settings-section-api",
            #[cfg(feature = "acp")]
            Self::Acp => "settings-section-acp",
            Self::Shortcuts => "settings-section-shortcuts",
            Self::Prompt => "settings-section-prompt",
            Self::Integrations => "settings-section-integrations",
            Self::Remote => "settings-section-remote",
            Self::Usage => "settings-section-usage",
            Self::About => "settings-section-about",
            Self::Advanced => "settings-section-advanced",
        })
    }

    const ALL: &[Self] = &[
        Self::General,
        Self::Account,
        Self::Billing,
        Self::Api,
        #[cfg(feature = "acp")]
        Self::Acp,
        Self::Shortcuts,
        Self::Prompt,
        Self::Integrations,
        Self::Remote,
        Self::Usage,
        Self::Advanced,
        Self::About,
    ];
}

/// One multi-value General row that selects from a dropdown instead of
/// toggling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingMenu {
    Appearance,
    Language,
    ChatFont,
    ChatSize,
    Voice,
    SpeechSpeed,
}

impl SettingMenu {
    /// Stable id fragment for the popup panel and its rows.
    fn id(self) -> &'static str {
        match self {
            Self::Appearance => "appearance",
            Self::Language => "language",
            Self::ChatFont => "chat-font",
            Self::ChatSize => "chat-size",
            Self::Voice => "voice",
            Self::SpeechSpeed => "speech-speed",
        }
    }
}

/// The id of choice `index` in `menu`'s dropdown.
fn option_id(menu: SettingMenu, index: usize) -> gpui::SharedString {
    gpui::SharedString::from(format!("setting-menu-{}-{index}", menu.id()))
}

pub struct SettingsScreen {
    locale: fluent_templates::LanguageIdentifier,
    messages: super::i18n::MessageCache,
    /// Backend-call bridges retained for thread-affinity; see
    /// [`crate::ui::task::call`].
    bridged_tasks: std::cell::RefCell<Vec<gpui::Task<()>>>,
    backend: Arc<AgentBackend>,
    integrations_backend: Arc<AgentBackend>,
    remote_screen: Entity<super::remote::RemoteScreen>,
    user_id: String,
    settings: AppSettings,
    update: crate::update::Snapshot,
    /// `settings.theme` parsed once; render only reads the label.
    theme: theme::Preference,
    section: Section,
    /// The multi-value row whose dropdown is open, if any.
    popup: Popup<SettingsScreen, SettingMenu>,
    account: AccountState,
    billing: BillingState,
    api_keys: ApiKeysState,
    #[cfg(feature = "proxy")]
    local_api: LocalApiUiState,
    #[cfg(feature = "acp")]
    acp: AcpUiState,
    /// Plan usage meter, same source as the sidebar card.
    plan: Option<crate::billing::PlanUsage>,
    /// Account MCP servers; None until loaded.
    mcp_servers: Option<Vec<McpDefinition>>,
    mcp_editor: Option<McpEditor>,
    mcp_address: Option<catalog::McpAddressPrompt>,
    /// The app being tried at its usual address, before asking for another.
    mcp_usual_address: Option<&'static maple_agent::agent::McpCatalogEntry>,
    mcp_notice: Option<UiText>,
    mcp_saving: bool,
    /// The server whose browser sign-in is in progress.
    mcp_signing_in: Option<String>,
    /// The server whose sign-in last failed, and why.
    mcp_sign_in_error: Option<(String, UiText)>,
    /// The app that needs something first, and what.
    mcp_app_missing: Option<(String, maple_agent::agent::McpAppMissing)>,
    catalog_search: Entity<TextInput>,
    catalog_query: String,
    catalog_category: Option<maple_agent::agent::McpCatalogCategory>,
    /// The catalog entries showing, worked out when they change.
    catalog_visible: Vec<&'static maple_agent::agent::McpCatalogEntry>,
    /// Device-local integrations discovered by the runtime; None until loaded.
    integrations: Option<Vec<AgentIntegration>>,
    integration_notice: Option<UiText>,
    integration_saving: HashSet<String>,
    /// Editor for the user's additional instructions (harness instructions).
    prompt_editor: Entity<TextInput>,
    prompt_notice: Option<UiText>,
    shortcut_snapshot: ShortcutSnapshot,
    shortcut_search: Entity<TextInput>,
    shortcut_query: String,
    shortcut_list_cache: ShortcutListCache,
    shortcut_notice: Option<UiText>,
    shortcut_recorder: Option<ShortcutRecorder>,
    shortcut_interceptor: Option<Subscription>,
    shortcut_reset_confirmation: bool,
    application_vim: SettingsApplicationVimState,
    application_focus: gpui::FocusHandle,
    application_focus_pending: bool,
    application_reveal_pending: bool,
    pane_scroll: ScrollHandle,
    application_anchor: ScrollAnchor,
    advanced: advanced::AdvancedState,
    advanced_subscriptions: Vec<Subscription>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ShortcutSettingsChange {
    Set {
        slot_id: String,
        sequence: String,
        disable_conflicts: Vec<String>,
    },
    Disable {
        slot_id: String,
    },
    Reset {
        slot_id: String,
    },
    ResetAll,
}

pub(crate) struct ShortcutSettingsRequested(pub(crate) ShortcutSettingsChange);

#[derive(Clone, Debug)]
struct ShortcutRecorder {
    slot_id: String,
    strokes: Vec<String>,
    conflicts: Vec<ShortcutConflict>,
    reviewing_conflicts: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ShortcutListCache {
    visible_indices: Vec<usize>,
    modified_count: usize,
}

impl ShortcutListCache {
    fn rebuild(snapshot: &ShortcutSnapshot, query: &str) -> Self {
        Self {
            visible_indices: snapshot
                .rows
                .iter()
                .enumerate()
                .filter_map(|(index, row)| shortcut_row_matches(row, query).then_some(index))
                .collect(),
            modified_count: snapshot.rows.iter().filter(|row| row.modified).count(),
        }
    }
}

/// Form state for adding or editing one MCP server.
struct McpEditor {
    /// Name of the server being edited; None when adding.
    original_name: Option<String>,
    original: Option<McpDefinition>,
    enabled: bool,
    http: bool,
    timeout_seconds: u64,
    name: Entity<TextInput>,
    description: Entity<TextInput>,
    /// Command line (stdio) or URL (HTTP).
    target: Entity<TextInput>,
    /// Only newly entered names/values. Saved values never populate inputs.
    environment: Entity<TextInput>,
    headers: Entity<TextInput>,
    saved_environment: Vec<McpSecretField>,
    saved_headers: Vec<McpSecretField>,
}

/// What the target field of the server form asks for.
fn mcp_target_placeholder(http: bool) -> &'static str {
    if http {
        "https://mcp.example.com/mcp"
    } else {
        "npx -y @modelcontextprotocol/server-everything stdio"
    }
}

/// Emitted with the section to open (composer "Manage integrations" link).
pub struct OpenSettingsSection(pub Section);

impl EventEmitter<super::remote::ComputersChanged> for SettingsScreen {}
impl EventEmitter<super::remote::ComputerNamed> for SettingsScreen {}
impl EventEmitter<SettingsClosed> for SettingsScreen {}
impl EventEmitter<crate::update::UpdateRequested> for SettingsScreen {}
impl EventEmitter<SignOutRequested> for SettingsScreen {}
impl EventEmitter<AccountDeleted> for SettingsScreen {}
impl EventEmitter<ShortcutSettingsRequested> for SettingsScreen {}

impl SettingsScreen {
    pub fn show_about(&mut self, cx: &mut Context<Self>) {
        self.select_section(Section::About, cx);
    }

    pub fn set_update(&mut self, snapshot: crate::update::Snapshot, cx: &mut Context<Self>) {
        if self.update != snapshot {
            self.update = snapshot;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(super) fn remote_screen_for_test(&self) -> Entity<super::remote::RemoteScreen> {
        self.remote_screen.clone()
    }

    pub(crate) fn settings_snapshot(&self) -> AppSettings {
        self.settings.clone()
    }

    /// Store mode comes from the owner, independently of edits made in this
    /// screen. Refresh that derived field without replacing pending user edits.
    pub(crate) fn refresh_store_password_mode(
        &mut self,
        password_mode: bool,
        cx: &mut Context<Self>,
    ) {
        self.settings.store_password_mode = password_mode;
        cx.notify();
    }

    pub(crate) fn refresh_remote_sidebars(
        &mut self,
        sidebars: std::collections::HashMap<String, settings::RemoteSidebarSettings>,
        cx: &mut Context<Self>,
    ) {
        let names = sidebars
            .iter()
            .map(|(key, sidebar)| (key.clone(), sidebar.display_name.clone()))
            .collect();
        self.settings.remote_sidebars = sidebars;
        self.remote_screen
            .update(cx, |screen, cx| screen.set_names(names, cx));
        cx.notify();
    }

    fn visible_sections(&self) -> Vec<Section> {
        Section::ALL
            .iter()
            .copied()
            .filter(|&section| {
                section != Section::Advanced
                    || self.section == Section::Advanced
                    || self.advanced_has_content()
            })
            .collect()
    }

    pub fn update_remote_status(
        &mut self,
        status: crate::remote::manager::Status,
        cx: &mut Context<Self>,
    ) {
        self.remote_screen
            .update(cx, |screen, cx| screen.update_status(status, cx));
    }

    pub fn new(
        backend: Arc<AgentBackend>,
        integrations_backend: Arc<AgentBackend>,
        user_id: String,
        settings: AppSettings,
        shortcut_snapshot: ShortcutSnapshot,
        section: Section,
        cx: &mut Context<Self>,
    ) -> Self {
        let names = settings
            .remote_sidebars
            .iter()
            .map(|(key, value)| (key.clone(), value.display_name.clone()))
            .collect();
        let remote_screen = cx.new(|cx| {
            super::remote::RemoteScreen::new(
                backend.clone(),
                user_id.clone(),
                section == Section::Remote,
                names,
                cx,
            )
        });
        cx.subscribe(
            &remote_screen,
            |this, _, event: &super::remote::ComputerNamed, cx| {
                this.settings
                    .remote_sidebars
                    .entry(event.key.clone())
                    .or_default()
                    .display_name = event.name.clone();
                cx.emit(super::remote::ComputerNamed {
                    key: event.key.clone(),
                    name: event.name.clone(),
                });
            },
        )
        .detach();
        cx.subscribe(
            &remote_screen,
            |_, _, event: &super::remote::ComputersChanged, cx| {
                cx.emit(super::remote::ComputersChanged(event.0.clone()));
            },
        )
        .detach();
        let prompt_text = settings.harness_instructions.clone();
        let application_vim_enabled = settings.application_vim_enabled;
        let application_focus = cx.focus_handle();
        let prompt_application_focus = application_focus.clone();
        let prompt_editor = cx.new(move |cx| {
            let mut input =
                TextInput::localized("settings-settings-add-instructions-for-every-task", cx)
                    .with_tab_index(1)
                    .multiline(16)
                    .spell_check()
                    .application_vim(application_vim_enabled)
                    .on_application_escape(move |window, cx| {
                        window.focus(&prompt_application_focus, cx)
                    });
            input.set_text(&prompt_text, cx);
            input
        });
        let search_application_focus = application_focus.clone();
        let shortcut_search = cx.new(move |cx| {
            TextInput::localized(
                "settings-settings-search-by-command-context-or-shortcut",
                cx,
            )
            .with_tab_index(2)
            .application_vim(application_vim_enabled)
            .on_application_escape(move |window, cx| window.focus(&search_application_focus, cx))
        });
        let catalog_application_focus = application_focus.clone();
        let catalog_search = cx.new(move |cx| {
            TextInput::localized("settings-catalog-search", cx)
                .with_tab_index(3)
                .application_vim(application_vim_enabled)
                .on_application_escape(move |window, cx| {
                    window.focus(&catalog_application_focus, cx)
                })
        });
        cx.on_release(|this, _| this.abandon_mcp_sign_in()).detach();
        cx.observe(&catalog_search, |this, search, cx| {
            this.catalog_query = search.read(cx).text().trim().to_lowercase();
            this.refresh_catalog();
            cx.notify();
        })
        .detach();
        cx.observe(&shortcut_search, |this, search, cx| {
            this.shortcut_query = search.read(cx).text().trim().to_lowercase();
            this.rebuild_shortcut_list_cache();
            if this.settings.application_vim_enabled {
                this.reconcile_application_vim_target();
            }
            cx.notify();
        })
        .detach();
        let shortcut_list_cache = ShortcutListCache::rebuild(&shortcut_snapshot, "");
        let pane_scroll = ScrollHandle::new();
        let application_anchor = ScrollAnchor::for_handle(pane_scroll.clone());
        let application_vim = SettingsApplicationVimState::new(section);
        let application_focus_pending = settings.application_vim_enabled;
        let mut this = Self {
            locale: super::i18n::current_locale(),
            messages: Default::default(),
            bridged_tasks: std::cell::RefCell::new(Vec::new()),
            backend,
            integrations_backend,
            remote_screen,
            user_id,
            theme: theme::Preference::parse(&settings.theme),
            settings,
            update: crate::update::Snapshot::initial(),
            section,
            popup: Popup::new(|this| &mut this.popup, cx),
            account: AccountState::new(application_vim_enabled, application_focus.clone(), cx),
            billing: BillingState::new(application_vim_enabled, application_focus.clone(), cx),
            api_keys: ApiKeysState::new(
                application_vim_enabled,
                application_focus.clone(),
                pane_scroll.clone(),
                cx,
            ),
            #[cfg(feature = "proxy")]
            local_api: LocalApiUiState::new(application_vim_enabled, application_focus.clone(), cx),
            #[cfg(feature = "acp")]
            acp: AcpUiState::new(),
            plan: None,
            mcp_servers: None,
            mcp_editor: None,
            mcp_address: None,
            mcp_usual_address: None,
            mcp_notice: None,
            mcp_saving: false,
            mcp_signing_in: None,
            mcp_sign_in_error: None,
            mcp_app_missing: None,
            catalog_search,
            catalog_query: String::new(),
            catalog_category: None,
            catalog_visible: Vec::new(),
            integrations: None,
            integration_notice: None,
            integration_saving: HashSet::new(),
            prompt_editor,
            prompt_notice: None,
            shortcut_snapshot,
            shortcut_search,
            shortcut_query: String::new(),
            shortcut_list_cache,
            shortcut_notice: None,
            shortcut_recorder: None,
            shortcut_interceptor: None,
            shortcut_reset_confirmation: false,
            application_vim,
            application_focus,
            application_focus_pending,
            application_reveal_pending: false,
            pane_scroll,
            application_anchor,
            advanced: advanced::AdvancedState::new(cx),
            advanced_subscriptions: Vec::new(),
        };
        this.load_account(cx);
        this.load_billing(cx);
        this.load_api_keys(cx);
        this.load_plan(cx);
        this.refresh_catalog();
        this.load_mcp_servers(cx);
        this.load_integrations(cx);
        this.load_advanced_status(cx);
        #[cfg(feature = "proxy")]
        let this = {
            let mut this = this;
            this.refresh_local_api_status(cx);
            this
        };
        this
    }

    fn load_plan(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.plan_usage(&user_id).await },
            cx,
            |this, result, cx| {
                if let Ok(plan) = result {
                    this.plan = plan;
                    cx.notify();
                }
            },
        );
    }

    fn call<T, F>(
        &self,
        future: F,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    ) where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    {
        let bridge = crate::ui::task::call(&self.backend, future, cx, then);
        crate::ui::task::retain(&self.bridged_tasks, bridge);
    }

    fn load_mcp_servers(&self, cx: &mut Context<Self>) {
        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.list_mcp_definitions(&user_id).await },
            cx,
            |this, result, cx| {
                match result {
                    Ok(servers) => {
                        this.mcp_servers = Some(servers);
                        this.refresh_catalog();
                    }
                    Err(message) => this.mcp_notice = Some(message.into()),
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    fn load_integrations(&self, cx: &mut Context<Self>) {
        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.list_integrations(&user_id).await },
            cx,
            |this, result, cx| {
                match result {
                    Ok(integrations) => {
                        this.integrations = Some(integrations);
                        this.integration_notice = None;
                    }
                    Err(message) => {
                        this.integrations = Some(Vec::new());
                        this.integration_notice = Some(message.into());
                    }
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    fn toggle_integration(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.integration_saving.contains(id) {
            return;
        }
        let Some((currently_enabled, can_enable)) = self
            .integrations
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|integration| integration.id == id)
            .map(|integration| {
                (
                    integration.enabled_for_new_tasks,
                    integration_can_enable(integration),
                )
            })
        else {
            return;
        };
        if !currently_enabled && !can_enable {
            self.integration_notice = Some(UiText::key(
                "settings-settings-this-integration-is-not-ready-to-enable-on-this-device",
            ));
            cx.notify();
            return;
        }

        let enabled = !currently_enabled;
        let id = id.to_string();
        self.integration_saving.insert(id.clone());
        self.integration_notice = None;
        cx.notify();

        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        let request_id = id.clone();
        self.call(
            async move {
                backend
                    .set_integration_enabled(&user_id, &request_id, enabled)
                    .await
            },
            cx,
            move |this, result, cx| {
                this.integration_saving.remove(&id);
                match result {
                    Ok(integrations) => this.integrations = Some(integrations),
                    Err(message) => this.integration_notice = Some(message.into()),
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    fn setup_integration(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.integration_saving.contains(id) {
            return;
        }
        let can_setup = self
            .integrations
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|integration| integration.id == id)
            .is_some_and(integration_can_setup);
        if !can_setup {
            self.integration_notice = Some(UiText::key(
                "settings-settings-this-integration-does-not-need-setup-on-this-device",
            ));
            cx.notify();
            return;
        }

        let id = id.to_string();
        let permissions = match self.integrations_backend.begin_integration_setup(&id) {
            Ok(permissions) => permissions,
            Err(message) => {
                self.integration_notice = Some(message.into());
                cx.notify();
                return;
            }
        };
        self.integration_notice = integration_setup_notice(&permissions);
        self.integration_saving.insert(id.clone());
        cx.notify();

        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        let request_id = id.clone();
        self.call(
            async move {
                backend
                    .setup_integration(&user_id, &request_id, permissions)
                    .await
            },
            cx,
            move |this, result, cx| {
                this.integration_saving.remove(&id);
                match result {
                    Ok(integrations) => {
                        // The notice above described the state before setup
                        // ran. The refreshed card carries what is left to do,
                        // so drop the stale one rather than leave it telling
                        // the user to repeat what they just did.
                        this.integration_notice = None;
                        this.integrations = Some(integrations);
                    }
                    Err(message) => {
                        this.integration_notice = Some(match this.integration_notice.take() {
                            Some(notice) => format!("{notice} {message}").into(),
                            None => message.into(),
                        });
                    }
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    fn save_mcp_servers(&mut self, servers: Vec<McpServerEdit>, cx: &mut Context<Self>) {
        if self.mcp_saving {
            return;
        }
        self.mcp_saving = true;
        self.mcp_notice = None;
        cx.notify();
        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.save_mcp_definition_edits(&user_id, servers).await },
            cx,
            |this, result, cx| {
                this.mcp_saving = false;
                match result {
                    Ok(servers) => {
                        this.mcp_servers = Some(servers);
                        this.mcp_editor = None;
                        this.refresh_catalog();
                    }
                    Err(message) => this.mcp_notice = Some(message.into()),
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    fn open_mcp_editor(&mut self, existing: Option<McpDefinition>, cx: &mut Context<Self>) {
        let application_focus = self.application_focus.clone();
        let application_vim_enabled = self.settings.application_vim_enabled;
        let mut field = |placeholder: &str, value: &str, index: isize, secret: bool| {
            let value = value.to_string();
            let field_application_focus = application_focus.clone();
            cx.new(move |cx| {
                let mut input = TextInput::new(placeholder, cx)
                    .with_tab_index(index)
                    .application_vim(application_vim_enabled)
                    .on_application_escape(move |window, cx| {
                        window.focus(&field_application_focus, cx)
                    });
                if secret {
                    input = input.masked();
                }
                if !value.is_empty() {
                    input.set_text(&value, cx);
                }
                input
            })
        };
        let (http, target) = match existing.as_ref().map(|s| &s.target) {
            Some(McpTarget::Stdio { command }) => (false, command.clone()),
            Some(McpTarget::StreamableHttp { url }) => (true, url.clone()),
            None => (false, String::new()),
        };
        let saved_environment = existing
            .as_ref()
            .map(|server| server.environment.as_slice())
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, presence)| McpSecretField {
                name: presence.name.clone(),
                has_value: presence.has_value,
                action: McpSecretAction::Keep,
                input: field(
                    &tr("settings-settings-enter-a-new-value"),
                    "",
                    10 + index as isize,
                    true,
                ),
            })
            .collect();
        let saved_headers = existing
            .as_ref()
            .map(|server| server.headers.as_slice())
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, presence)| McpSecretField {
                name: presence.name.clone(),
                has_value: presence.has_value,
                action: McpSecretAction::Keep,
                input: field(
                    &tr("settings-settings-enter-a-new-value"),
                    "",
                    200 + index as isize,
                    true,
                ),
            })
            .collect();
        self.mcp_editor = Some(McpEditor {
            original_name: existing.as_ref().map(|s| s.name.clone()),
            original: existing.clone(),
            enabled: existing.as_ref().map(|s| s.enabled).unwrap_or(true),
            http,
            timeout_seconds: existing.as_ref().map(|s| s.timeout_seconds).unwrap_or(300),
            name: field(
                &tr("settings-settings-my-server"),
                existing.as_ref().map(|s| s.name.as_str()).unwrap_or(""),
                1,
                false,
            ),
            description: field(
                &tr("settings-settings-what-this-server-helps-the-agent-do"),
                existing
                    .as_ref()
                    .map(|s| s.description.as_str())
                    .unwrap_or(""),
                2,
                false,
            ),
            target: field(mcp_target_placeholder(http), &target, 3, false),
            environment: field("KEY=value; OTHER=value", "", 4, false),
            headers: field("Authorization=Bearer …; X-Api-Key=…", "", 5, false),
            saved_environment,
            saved_headers,
        });
        self.mcp_notice = None;
        cx.notify();
    }

    /// Sign in to the HTTP server in the form, through the browser. The host
    /// saves the server once the sign-in works, so nothing else is entered.
    fn sign_in_mcp_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.mcp_editor.as_ref() else {
            return;
        };
        let request = maple_agent::agent::McpSignIn {
            name: editor.name.read(cx).text().trim().to_string(),
            description: editor.description.read(cx).text().trim().to_string(),
            url: editor.target.read(cx).text().trim().to_string(),
            scopes: Vec::new(),
        };
        if request.name.is_empty() || request.url.is_empty() {
            self.mcp_notice = Some(UiText::key("settings-catalog-server-required"));
            cx.notify();
            return;
        }
        self.start_mcp_sign_in(request, cx);
    }

    fn submit_mcp_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.mcp_editor.as_ref() else {
            return;
        };
        let name = editor.name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.mcp_notice = Some(UiText::key("settings-settings-enter-a-server-name"));
            cx.notify();
            return;
        }
        let existing = self.mcp_servers.as_deref().unwrap_or_default();
        if name_collides(existing, editor.original_name.as_deref(), &name) {
            self.mcp_notice = Some(UiText::message(
                "settings-server-exists",
                [("name", (name).to_string().into())],
            ));
            cx.notify();
            return;
        }
        let target = editor.target.read(cx).text().trim().to_string();
        if target.is_empty() {
            self.mcp_notice = Some(if editor.http {
                UiText::key("settings-settings-enter-the-server-url")
            } else {
                UiText::key("settings-settings-enter-the-server-command")
            });
            cx.notify();
            return;
        }
        let mut server = McpServerEdit {
            key: maple_agent::agent::AgentMcpServer::key_for_name(&name),
            name,
            description: editor.description.read(cx).text().trim().to_string(),
            enabled: editor.enabled,
            timeout_seconds: editor.timeout_seconds,
            target: if editor.http {
                McpTarget::StreamableHttp { url: target }
            } else {
                McpTarget::Stdio { command: target }
            },
            environment: Vec::new(),
            headers: Vec::new(),
        };
        let edits = (|| {
            server.environment = mcp::field_edits(&editor.saved_environment, cx)?;
            server
                .environment
                .extend(mcp::new_value_edits(&Zeroizing::new(
                    editor.environment.read(cx).text(),
                ))?);
            server.headers = mcp::field_edits(&editor.saved_headers, cx)?;
            server.headers.extend(mcp::new_value_edits(&Zeroizing::new(
                editor.headers.read(cx).text(),
            ))?);
            mcp::require_target_choice(editor.original.as_ref(), &server)?;
            if !editor.http
                && server.headers.iter().any(|entry| {
                    !matches!(
                        entry.update,
                        maple_agent::store::records::McpSecretUpdate::Clear
                    )
                })
            {
                return Err(
                    tr("settings-settings-clear-each-saved-http-header-before-switching-to-standard-io").to_string(),
                );
            }
            Ok(())
        })();
        if let Err(message) = edits {
            self.mcp_notice = Some(message.into());
            cx.notify();
            return;
        }
        let original = editor.original_name.clone();
        let mut servers: Vec<_> = self
            .mcp_servers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(definition_edit)
            .collect();
        match original.and_then(|o| servers.iter().position(|s| s.name == o)) {
            Some(index) => servers[index] = server,
            None => servers.push(server),
        }
        self.save_mcp_servers(servers, cx);
    }

    /// Open the editor for the server called `name`, looked up on click so
    /// the list rows do not clone a server per frame.
    fn edit_mcp_server(&mut self, name: &str, cx: &mut Context<Self>) {
        let server = self
            .mcp_servers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|server| server.name == name)
            .cloned();
        if server.is_some() {
            self.open_mcp_editor(server, cx);
        }
    }

    fn toggle_mcp_server(&mut self, name: &str, cx: &mut Context<Self>) {
        let mut servers: Vec<_> = self
            .mcp_servers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(definition_edit)
            .collect();
        if let Some(server) = servers.iter_mut().find(|s| s.name == name) {
            server.enabled = !server.enabled;
        }
        self.save_mcp_servers(servers, cx);
    }

    fn remove_mcp_server(&mut self, name: &str, cx: &mut Context<Self>) {
        let mut servers: Vec<_> = self
            .mcp_servers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(definition_edit)
            .collect();
        servers.retain(|s| s.name != name);
        self.save_mcp_servers(servers, cx);
    }

    /// Change one setting: apply it to the local copy, queue the write
    /// off the UI thread, and re-render. Every toggle goes through here.
    ///
    /// `apply` must set a value, not toggle one: it runs on the local copy
    /// now and again on the file on disk from the writer thread, so both
    /// end in the same state whatever the file held.
    fn edit_setting(
        &mut self,
        apply: impl FnOnce(&mut AppSettings) + Clone + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        apply.clone()(&mut self.settings);
        settings::update_settings_in_background(apply);
        cx.notify();
    }

    /// Open or close one row's dropdown. Opening a row closes another
    /// open one. Like a native popup button, the menu opens on the saved
    /// value.
    pub(super) fn toggle_setting_menu(&mut self, menu: SettingMenu, cx: &mut Context<Self>) {
        if self.popup.is_open(&menu) {
            self.popup.close(cx);
        } else {
            let current = self
                .menu_options(menu)
                .iter()
                .position(|option| option.current)
                .unwrap_or(0);
            self.popup
                .open_highlighted(menu, option_id(menu, current), cx);
        }
    }

    pub(super) fn close_setting_menu(&mut self, cx: &mut Context<Self>) {
        self.popup.close(cx);
    }

    /// The choices a dropdown offers, in display order. Render and pick
    /// share this order, so an index means the same option in both.
    fn menu_options(&self, menu: SettingMenu) -> Vec<SettingOption> {
        match menu {
            SettingMenu::Language => super::i18n::LANGUAGES
                .iter()
                .map(|locale| SettingOption {
                    label: super::i18n::preference_label(*locale).to_string(),
                    current: self.settings.locale.as_deref() == *locale,
                })
                .collect(),
            SettingMenu::Appearance => [
                theme::Preference::System,
                theme::Preference::Dark,
                theme::Preference::Light,
            ]
            .iter()
            .map(|&preference| SettingOption {
                label: theme_label(preference).to_string(),
                current: self.theme == preference,
            })
            .collect(),
            SettingMenu::ChatFont => crate::ui::typography::ChatFontFamily::ALL
                .iter()
                .map(|&family| SettingOption {
                    label: font_label(family).to_string(),
                    current: crate::ui::typography::ChatFontFamily::parse(
                        &self.settings.chat_font_family,
                    ) == family,
                })
                .collect(),
            SettingMenu::ChatSize => (crate::ui::typography::CHAT_FONT_SIZE_MIN
                ..=crate::ui::typography::CHAT_FONT_SIZE_MAX)
                .map(|size| SettingOption {
                    label: format!("{size} px"),
                    current: crate::ui::typography::clamp_chat_font_size(
                        self.settings.chat_font_size,
                    ) == size,
                })
                .collect(),
            SettingMenu::Voice => settings::TTS_VOICES
                .iter()
                .map(|(id, _)| SettingOption {
                    label: voice_label(id),
                    current: *id == self.settings.tts_voice,
                })
                .collect(),
            SettingMenu::SpeechSpeed => settings::TTS_SPEEDS
                .iter()
                .map(|&speed| SettingOption {
                    label: format!("{speed:.1}\u{d7}"),
                    current: (speed - self.settings.tts_speed).abs() < 0.01,
                })
                .collect(),
        }
    }

    /// The saved value shown on the dropdown's trigger button.
    fn menu_value(&self, menu: SettingMenu) -> String {
        match menu {
            SettingMenu::Language => {
                super::i18n::preference_label(self.settings.locale.as_deref()).to_string()
            }
            SettingMenu::Appearance => theme_label(self.theme).to_string(),
            SettingMenu::ChatFont => font_label(crate::ui::typography::ChatFontFamily::parse(
                &self.settings.chat_font_family,
            ))
            .to_string(),
            SettingMenu::ChatSize => format!(
                "{} px",
                crate::ui::typography::clamp_chat_font_size(self.settings.chat_font_size)
            ),
            SettingMenu::Voice => voice_label(&self.settings.tts_voice),
            SettingMenu::SpeechSpeed => format!("{:.1}\u{d7}", self.settings.tts_speed),
        }
    }

    /// The trigger label, drawn in the selected chat face when this is
    /// the font picker so System vs Manrope is visible before leaving
    /// Settings.
    fn menu_value_label(&self, menu: SettingMenu) -> gpui::AnyElement {
        let label = div().child(self.menu_value(menu));
        if menu == SettingMenu::ChatFont {
            crate::ui::typography::with_family(
                label,
                crate::ui::typography::ChatFontFamily::parse(&self.settings.chat_font_family),
            )
            .into_any_element()
        } else {
            label.into_any_element()
        }
    }

    /// Apply the option at `index`: an absolute set, so the local copy
    /// and the file end in the same state whatever the file held.
    pub(super) fn pick_setting_option(
        &mut self,
        menu: SettingMenu,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        match menu {
            SettingMenu::Language => {
                let Some(locale) = super::i18n::LANGUAGES.get(index) else {
                    return;
                };
                let locale = locale.map(str::to_owned);
                super::i18n::apply_preference(locale.as_deref());
                cx.refresh_windows();
                self.edit_setting(move |settings| settings.locale = locale, cx);
            }
            SettingMenu::Appearance => {
                let Some(preference) = [
                    theme::Preference::System,
                    theme::Preference::Dark,
                    theme::Preference::Light,
                ]
                .get(index) else {
                    return;
                };
                self.choose_theme(*preference, cx);
            }
            SettingMenu::ChatFont => {
                let Some(family) = crate::ui::typography::ChatFontFamily::ALL.get(index) else {
                    return;
                };
                self.choose_chat_font(*family, cx);
            }
            SettingMenu::ChatSize => {
                let size = crate::ui::typography::CHAT_FONT_SIZE_MIN.saturating_add(index as u8);
                if size > crate::ui::typography::CHAT_FONT_SIZE_MAX {
                    return;
                }
                self.choose_chat_font_size(size, cx);
            }
            SettingMenu::Voice => {
                let Some((id, _)) = settings::TTS_VOICES.get(index) else {
                    return;
                };
                let id = id.to_string();
                self.edit_setting(move |settings| settings.tts_voice = id, cx);
            }
            SettingMenu::SpeechSpeed => {
                let Some(speed) = settings::TTS_SPEEDS.get(index) else {
                    return;
                };
                let speed = *speed;
                self.edit_setting(move |settings| settings.tts_speed = speed, cx);
            }
        }
        self.close_setting_menu(cx);
    }

    fn toggle_web_default(&mut self, cx: &mut Context<Self>) {
        let next = !self.settings.default_web_enabled;
        self.edit_setting(move |settings| settings.default_web_enabled = next, cx);
    }

    fn choose_theme(&mut self, preference: theme::Preference, cx: &mut Context<Self>) {
        self.theme = preference;
        self.edit_setting(
            move |settings| settings.theme = preference.as_str().to_string(),
            cx,
        );
        // The root view resolves the palette on its next render and
        // refreshes every view when it changed.
        crate::ui::theme::apply_preference(preference, cx);
    }

    fn choose_chat_font(
        &mut self,
        family: crate::ui::typography::ChatFontFamily,
        cx: &mut Context<Self>,
    ) {
        let size = crate::ui::typography::clamp_chat_font_size(self.settings.chat_font_size);
        crate::ui::typography::apply(family, size);
        self.edit_setting(
            move |settings| settings.chat_font_family = family.as_str().to_string(),
            cx,
        );
    }

    fn choose_chat_font_size(&mut self, size: u8, cx: &mut Context<Self>) {
        let size = crate::ui::typography::clamp_chat_font_size(size);
        let family = crate::ui::typography::ChatFontFamily::parse(&self.settings.chat_font_family);
        crate::ui::typography::apply(family, size);
        self.edit_setting(move |settings| settings.chat_font_size = size, cx);
    }

    fn toggle_tool_details(&mut self, cx: &mut Context<Self>) {
        let next = !self.settings.tool_details;
        self.edit_setting(move |settings| settings.tool_details = next, cx);
    }

    fn toggle_desktop_notifications(&mut self, cx: &mut Context<Self>) {
        let next = !self.settings.desktop_notifications;
        self.edit_setting(move |settings| settings.desktop_notifications = next, cx);
        if self.settings.desktop_notifications {
            // Fire a test notification so enabling gives immediate feedback
            // and delivery problems surface right away.
            let enabled_at = chrono::Local::now().format("%H:%M").to_string();
            crate::notify::notify(
                cx,
                "settings:test",
                &super::i18n::tr("settings-notification-test-title"),
                &super::i18n::tr_args(
                    "settings-notification-test-body",
                    [("time", enabled_at.into())],
                ),
                &[],
            );
        }
    }

    fn toggle_password_notification_details(&mut self, cx: &mut Context<Self>) {
        let next = !self.settings.password_notification_details;
        self.edit_setting(
            move |settings| settings.password_notification_details = next,
            cx,
        );
    }

    /// Build one multi-value row: the saved value sits in a pebble button
    /// that opens a dropdown of every choice. Picking one applies and
    /// persists it; clicking away dismisses.
    fn setting_menu_row(
        &self,
        title: &str,
        description: &str,
        menu: SettingMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let dropdown = self.popup.is_open(&menu).then(|| {
            let options = self.menu_options(menu);
            let dropdown = Menu::new(
                gpui::SharedString::from(format!("setting-menu-{}", menu.id())),
                px(if menu == SettingMenu::Voice {
                    360.
                } else {
                    260.
                }),
            )
            .label(title.to_string())
            .max_height(px(320.))
            .application_vim(self.settings.application_vim_enabled)
            .items(options.into_iter().enumerate().map(|(index, option)| {
                let item = MenuItem::new(
                    option_id(menu, index),
                    option.label,
                    move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                        this.pick_setting_option(menu, index, cx);
                    },
                )
                .current(option.current);
                // Each font option previews itself.
                match crate::ui::typography::ChatFontFamily::ALL.get(index) {
                    Some(&family) if menu == SettingMenu::ChatFont => {
                        item.style(move |row| crate::ui::typography::with_family(row, family))
                    }
                    _ => item,
                }
            }));
            self.popup.render(dropdown, Placement::BelowEnd, window, cx)
        });
        let slug = match menu {
            SettingMenu::Appearance => "appearance",
            SettingMenu::Language => "language",
            SettingMenu::ChatFont => "chat-font",
            SettingMenu::ChatSize => "text-size",
            SettingMenu::Voice => "speech-voice",
            SettingMenu::SpeechSpeed => "speech-speed",
        }
        .to_owned();
        let button =
            widgets::secondary_button(gpui::SharedString::from(format!("setting-value-{slug}")))
                .debug_selector(move || format!("setting-value-{slug}"))
                .aria_label(title.to_string())
                .aria_value(self.menu_value(menu))
                .py_1p5()
                .gap_1()
                .child(self.menu_value_label(menu))
                .child(icon("chevron-down", px(12.), theme::text_muted()));
        widgets::card_row()
            .id(gpui::SharedString::from(format!(
                "setting-{}",
                match menu {
                    SettingMenu::ChatSize => "text-size",
                    SettingMenu::Voice => "speech-voice",
                    _ => menu.id(),
                }
            )))
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(setting_copy(title, description))
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(self.popup.trigger(menu, button, cx, move |this, _, cx| {
                        this.toggle_setting_menu(menu, cx)
                    }))
                    .children(dropdown),
            )
    }

    fn toggle_reduce_motion(&mut self, cx: &mut Context<Self>) {
        let next = !self.settings.reduce_motion;
        self.edit_setting(move |settings| settings.reduce_motion = next, cx);
        cx.set_reduce_motion(next);
    }

    fn toggle_tool_summaries(&mut self, cx: &mut Context<Self>) {
        let next = !self.settings.tool_summaries;
        self.edit_setting(move |settings| settings.tool_summaries = next, cx);
    }

    fn toggle_composer_vim(&mut self, cx: &mut Context<Self>) {
        let next = !self.settings.composer_vim_enabled;
        self.edit_setting(move |settings| settings.composer_vim_enabled = next, cx);
    }

    fn toggle_application_vim(&mut self, cx: &mut Context<Self>) {
        let next = !self.settings.application_vim_enabled;
        self.set_application_vim_enabled(next, cx);
    }

    /// Persist the editor text as the additional instructions and hand the
    /// combined harness instructions to the running backend.
    fn save_prompt(&mut self, cx: &mut Context<Self>) {
        self.settings.harness_instructions = self.prompt_editor.read(cx).text().trim().to_string();
        let instructions = self.settings.harness_instructions.clone();
        settings::update_settings_in_background(move |s| s.harness_instructions = instructions);
        self.backend
            .set_harness_instructions(self.settings.effective_harness_instructions());
        self.prompt_notice = Some(UiText::key(
            "settings-settings-saved-new-tasks-use-these-instructions",
        ));
        cx.notify();
    }

    fn clear_prompt(&mut self, cx: &mut Context<Self>) {
        self.prompt_editor.update(cx, |input, cx| {
            input.set_text("", cx);
        });
        self.prompt_notice = None;
        cx.notify();
    }

    pub(crate) fn apply_shortcut_result(
        &mut self,
        shortcut_overrides: ShortcutOverrides,
        snapshot: ShortcutSnapshot,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        if result.is_ok() {
            merge_shortcut_overrides(&mut self.settings, shortcut_overrides);
            self.shortcut_reset_confirmation = false;
            self.stop_shortcut_recording();
        }
        self.shortcut_snapshot = snapshot;
        self.rebuild_shortcut_list_cache();
        if self.settings.application_vim_enabled {
            self.reconcile_application_vim_target();
        }
        self.shortcut_notice = Some(match result {
            Ok(()) => UiText::key("settings-settings-shortcut-settings-applied"),
            Err(message) => UiText::message(
                "settings-shortcut-change-failed",
                [("message", (message).to_string().into())],
            ),
        });
        cx.notify();
    }

    fn request_shortcut_change(&mut self, change: ShortcutSettingsChange, cx: &mut Context<Self>) {
        self.shortcut_notice = None;
        cx.emit(ShortcutSettingsRequested(change));
    }

    fn rebuild_shortcut_list_cache(&mut self) {
        self.shortcut_list_cache =
            ShortcutListCache::rebuild(&self.shortcut_snapshot, &self.shortcut_query);
    }

    fn begin_shortcut_recording(&mut self, slot_id: String, cx: &mut Context<Self>) {
        self.stop_shortcut_recording();
        self.shortcut_notice = None;
        self.shortcut_recorder = Some(ShortcutRecorder {
            slot_id,
            strokes: Vec::new(),
            conflicts: Vec::new(),
            reviewing_conflicts: false,
        });

        let settings = cx.entity().downgrade();
        self.shortcut_interceptor = Some(cx.intercept_keystrokes(move |event, _window, app| {
            let Some(settings) = settings.upgrade() else {
                return;
            };
            settings.update(app, |this, cx| {
                this.record_shortcut_keystroke(&event.keystroke, cx);
            });
            // Recording owns the keystroke. No existing shortcut or text
            // input handler may observe it underneath the recorder.
            app.stop_propagation();
        }));
        cx.notify();
    }

    fn record_shortcut_keystroke(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        let plain = !keystroke.modifiers.modified();
        match (plain, keystroke.key.as_str()) {
            (true, "escape") => {
                self.stop_shortcut_recording();
                self.shortcut_notice = Some(UiText::key(
                    "settings-settings-shortcut-recording-cancelled",
                ));
            }
            (true, "backspace") => {
                if let Some(recorder) = self.shortcut_recorder.as_mut() {
                    recorder.strokes.pop();
                }
            }
            (true, "enter") => self.review_recorded_shortcut(cx),
            _ => {
                let Some(recorder) = self.shortcut_recorder.as_mut() else {
                    return;
                };
                if recorder.strokes.len() < 4 {
                    recorder.strokes.push(portable_keystroke_token(keystroke));
                } else {
                    self.shortcut_notice = Some(UiText::key(
                        "settings-settings-a-shortcut-can-contain-at-most-four-strokes",
                    ));
                }
            }
        }
        cx.notify();
    }

    fn review_recorded_shortcut(&mut self, cx: &mut Context<Self>) {
        let Some(recorder) = self.shortcut_recorder.as_ref() else {
            return;
        };
        if recorder.strokes.is_empty() {
            self.shortcut_notice = Some(UiText::key(
                "settings-settings-press-at-least-one-shortcut-key",
            ));
            return;
        }
        let slot_id = recorder.slot_id.clone();
        let sequence = recorder.strokes.join(" ");
        match self.shortcut_snapshot.conflicts_for(&slot_id, &sequence) {
            Ok(conflicts) if conflicts.is_empty() => {
                self.stop_shortcut_recording();
                self.request_shortcut_change(
                    ShortcutSettingsChange::Set {
                        slot_id,
                        sequence,
                        disable_conflicts: Vec::new(),
                    },
                    cx,
                );
            }
            Ok(conflicts) => {
                // Conflict review uses ordinary buttons, so release the
                // keystroke interceptor before showing it.
                self.shortcut_interceptor = None;
                if let Some(recorder) = self.shortcut_recorder.as_mut() {
                    recorder.conflicts = conflicts;
                    recorder.reviewing_conflicts = true;
                }
            }
            Err(message) => {
                self.shortcut_notice = Some(UiText::message(
                    "settings-shortcut-invalid",
                    [("message", (message).to_string().into())],
                ));
            }
        }
    }

    fn save_recorded_shortcut(&mut self, replace_conflicts: bool, cx: &mut Context<Self>) {
        let Some(recorder) = self.shortcut_recorder.take() else {
            return;
        };
        self.shortcut_interceptor = None;
        let mut disable_conflicts = if replace_conflicts {
            recorder
                .conflicts
                .into_iter()
                .map(|conflict| conflict.other_slot_id)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        disable_conflicts.sort();
        disable_conflicts.dedup();
        self.request_shortcut_change(
            ShortcutSettingsChange::Set {
                slot_id: recorder.slot_id,
                sequence: recorder.strokes.join(" "),
                disable_conflicts,
            },
            cx,
        );
    }

    fn stop_shortcut_recording(&mut self) {
        self.shortcut_interceptor = None;
        self.shortcut_recorder = None;
    }

    fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.password_operation_busy(cx) {
            return;
        }
        if section != Section::Advanced {
            self.advanced.form = None;
            self.advanced_subscriptions.clear();
            self.advanced.erase = false;
        }
        if section != Section::Shortcuts {
            self.stop_shortcut_recording();
        }
        // A dropdown belongs to the pane that opened it.
        self.close_setting_menu(cx);
        self.section = section;
        #[cfg(feature = "proxy")]
        if section == Section::Api {
            self.refresh_local_api_status(cx);
        }
        self.remote_screen.update(cx, |screen, cx| {
            screen.set_active(section == Section::Remote, cx)
        });
        if self.settings.application_vim_enabled {
            self.application_vim.section = section;
            self.reconcile_application_vim_target();
        }
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.password_operation_busy(cx) {
            return;
        }
        self.stop_shortcut_recording();
        cx.emit(SettingsClosed(self.settings.clone()));
    }
}

fn merge_shortcut_overrides(settings: &mut AppSettings, shortcut_overrides: ShortcutOverrides) {
    settings.shortcut_overrides = shortcut_overrides;
}

impl Render for SettingsScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let locale = super::i18n::current_locale();
        if self.locale != locale {
            self.locale = locale;
            self.rebuild_shortcut_list_cache();
            self.refresh_credit_placeholder(cx);
            self.refresh_catalog();
        }
        // First, so focus requested below wins over a dropdown handing it
        // back.
        self.popup.sync_focus(window, cx);
        if self.application_focus_pending {
            self.application_focus_pending = false;
            if self.settings.application_vim_enabled {
                window.focus(&self.application_focus, cx);
            }
        }
        if self.application_reveal_pending {
            self.application_reveal_pending = false;
            if self.application_vim.target.is_some() {
                // The selected row attaches this anchor later in the same render.
                // ScrollAnchor defers until the fresh layout origin is available.
                self.application_anchor.scroll_to(window, cx);
            }
        }
        if std::mem::take(&mut self.api_keys.reveal_card) {
            self.api_keys.card_anchor.scroll_to(window, cx);
        }
        self.focus_mcp_address(window, cx);
        let mcp_address = self.render_mcp_address(window, cx);
        div()
            .key_context(self.application_vim_context())
            .track_focus(&self.application_focus)
            .on_action(cx.listener(Self::app_vim_next))
            .on_action(cx.listener(Self::app_vim_previous))
            .on_action(cx.listener(Self::app_vim_first))
            .on_action(cx.listener(Self::app_vim_last))
            .on_action(cx.listener(Self::app_vim_activate))
            .on_action(cx.listener(Self::app_vim_collapse))
            .on_action(cx.listener(Self::app_vim_expand))
            .on_action(cx.listener(Self::app_vim_copy))
            .on_action(cx.listener(Self::app_vim_search))
            .on_action(cx.listener(Self::app_vim_escape))
            .on_action(cx.listener(Self::app_vim_composer))
            .on_action(cx.listener(Self::app_vim_newest_assistant))
            .on_action(cx.listener(Self::app_vim_next_assistant))
            .on_action(cx.listener(Self::app_vim_previous_assistant))
            .on_action(cx.listener(Self::app_vim_next_annotation))
            .on_action(cx.listener(Self::app_vim_previous_annotation))
            .on_action(cx.listener(Self::app_vim_count))
            .on_action(cx.listener(Self::app_vim_move_region))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(theme::bg_app()))
            .child(crate::ui::titlebar::drag_region(
                div()
                    .id("settings-header")
                    .flex()
                    .items_center()
                    .gap_3()
                    .pl(crate::ui::titlebar::top_row_inset(px(16.)))
                    .pr_4()
                    .py_3()
                    .border_b_1()
                    .border_color(gpui::rgb(theme::border()))
                    .child(
                        widgets::ghost_button("settings-back")
                            .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.close(cx);
                            }))
                            .child(super::i18n::tr("settings-back")),
                    )
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(super::i18n::tr("settings-title")),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("settings-sign-out")
                            .px_2()
                            .py_1()
                            .rounded(theme::RADIUS_SM)
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .hover(|style| {
                                style
                                    .text_color(gpui::rgb(theme::status_error()))
                                    .cursor_pointer()
                            })
                            .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(|_this, _event, _window, cx| {
                                cx.emit(SignOutRequested);
                            }))
                            .child(super::i18n::tr("settings-sign-out")),
                    ),
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(self.render_nav(cx))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_pane(window, cx))
                            .child(crate::ui::scrollbar::scrollbar(
                                "settings-scrollbar",
                                self.pane_scroll.clone(),
                            )),
                    ),
            )
            .children(mcp_address)
    }
}

impl SettingsScreen {
    fn render_nav(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        div()
            .id("settings-nav")
            .role(gpui::Role::TabList)
            .aria_label(super::i18n::tr("settings-sections"))
            .aria_orientation(gpui::Orientation::Vertical)
            .w(gpui::px(220.))
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .border_r_1()
            .border_color(gpui::rgb(theme::border()))
            .bg(gpui::rgb(theme::bg_sidebar()))
            .children(self.visible_sections().iter().map(|section| {
                let selected = self.section == *section;
                let application_selected = self.application_vim_selects_section(*section);
                div()
                    .id(gpui::SharedString::from(format!(
                        "settings-nav-{}",
                        section.id()
                    )))
                    .role(gpui::Role::Tab)
                    .aria_label(section.label())
                    .aria_selected(selected)
                    .when(application_selected, |row| row.aria_active_descendant())
                    .px_3()
                    .py_2()
                    .rounded(theme::RADIUS_SM)
                    .text_sm()
                    .text_color(gpui::rgb(if selected {
                        theme::text_primary()
                    } else {
                        theme::text_secondary()
                    }))
                    .bg(gpui::rgb(if selected {
                        theme::bg_elevated()
                    } else {
                        theme::bg_sidebar()
                    }))
                    .when(application_selected, |row| {
                        row.border_l_2().border_color(gpui::rgb(theme::accent()))
                    })
                    .hover(|style| style.bg(gpui::rgb(theme::bg_elevated())).cursor_pointer())
                    .on_click({
                        let section = *section;
                        cx.listener(move |this, _event, _window, cx| {
                            this.select_section(section, cx);
                        })
                    })
                    .child(section.label().to_string())
            }))
    }

    fn render_pane(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let mut pane = div()
            .id("settings-pane")
            .role(gpui::Role::TabPanel)
            .aria_label(self.section.label())
            .w_full()
            .max_w(px(960.))
            .h_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .track_scroll(&self.pane_scroll)
            .overflow_y_scroll();
        match self.section {
            Section::Advanced => {
                pane = pane.child(self.render_advanced(cx));
            }
            Section::General => {
                pane = self.render_general(pane, window, cx);
            }
            Section::Account => {
                pane = pane.child(self.render_account_pane(cx));
            }
            Section::Billing => {
                pane = pane.child(self.render_billing_pane(cx));
            }
            Section::Api => {
                pane = pane.child(self.render_api_pane(cx));
            }
            #[cfg(feature = "acp")]
            Section::Acp => {
                pane = pane.child(self.render_acp_pane(cx));
            }
            Section::Shortcuts => {
                pane = pane.child(self.render_shortcuts_pane(cx));
            }
            Section::Prompt => {
                pane = pane.child(self.render_prompt_pane(cx));
            }
            Section::Integrations => {
                pane = pane.child(self.render_integrations_pane(cx));
            }
            Section::Remote => {
                pane = pane.child(self.remote_screen.clone());
            }
            Section::Usage => {
                pane = pane.child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .child(section_title(&tr("settings-settings-plan")))
                        .when_some(self.plan.as_ref(), |row, plan| {
                            row.child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(gpui::rgb(theme::accent()))
                                    .child(presentation::plan_name(plan, &self.messages)),
                            )
                        }),
                );
                pane = pane.child(match &self.plan {
                    Some(plan) => plan_card(plan, &self.messages),
                    None => div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(tr("settings-settings-plan-usage-unavailable")),
                });
            }
            Section::About => {
                pane = pane
                    .child(section_title(&tr("settings-settings-about")))
                    .child(info_row(
                        &tr("settings-settings-version"),
                        format!("v{}", crate::ui::titlebar::TitleBar::version()),
                    ))
                    .child(self.render_update_row(cx))
                    .child(toggle_row(
                        tr("settings-settings-automatically-download-updates"),
                        tr("settings-settings-download-verified-updates-in-the-background-installation-always-w"),
                        self.settings.automatic_update_downloads,
                        cx.listener(|this, _, _, cx| {
                            let enabled = !this.settings.automatic_update_downloads;
                            this.edit_setting(move |settings| settings.automatic_update_downloads = enabled, cx);
                            cx.emit(crate::update::UpdateRequested(crate::update::UpdateAction::AutomaticDownloads(enabled)));
                        }),
                    ));
            }
        }
        pane
    }
}

impl SettingsScreen {
    /// One status and the one thing to do next: a release action when
    /// there is one, otherwise a quiet check. Remote computer cards follow
    /// the same rule.
    fn render_update_row(&self, cx: &mut Context<Self>) -> Div {
        use crate::update::{State, UpdateAction, UpdateRequested};

        let request = |action| cx.listener(move |_, _, _, cx| cx.emit(UpdateRequested(action)));
        let actions = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .my(px(-8.))
            .when(
                self.update.state == State::Ready && self.update.show_banner,
                |row| {
                    row.child(
                        widgets::ghost_button("updates-later")
                            .on_click(request(UpdateAction::Later))
                            .child(tr("settings-settings-later")),
                    )
                },
            )
            .when(
                self.update.state == State::Error && self.update.manual_download_url.is_some(),
                |row| {
                    row.child(
                        widgets::ghost_button("updates-manual")
                            .on_click(request(UpdateAction::ManualDownload))
                            .child(tr("settings-settings-download-manually")),
                    )
                },
            )
            .when_some(next_update_action(&self.update), |row, action| {
                row.child(
                    widgets::primary_button("updates-primary")
                        .on_click(request(action))
                        .child(presentation::update_action(action)),
                )
            })
            .map(|row| match update_check(&self.update) {
                UpdateCheck::Hidden => row,
                UpdateCheck::Running => row.child(
                    div()
                        .flex_none()
                        .size_7()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(crate::ui::icons::spinning_icon(
                            "updates-check",
                            "rotate-cw",
                            widgets::ROW_ICON,
                            theme::text_secondary(),
                        )),
                ),
                UpdateCheck::Ready(label) => row.child(
                    widgets::icon_button(
                        "updates-check",
                        "rotate-cw",
                        tr(label),
                        widgets::ROW_ICON,
                        theme::text_secondary(),
                    )
                    .on_click(request(UpdateAction::Check)),
                ),
            });
        let has_actions = update_check(&self.update) != UpdateCheck::Hidden
            || next_update_action(&self.update).is_some();
        info_row(
            &tr("settings-settings-update"),
            presentation::update_label(&self.update, &self.messages),
        )
        .when(has_actions, |row| row.child(actions))
    }
}

/// The small check control in the Update row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateCheck {
    Hidden,
    Running,
    Ready(&'static str),
}

/// The release action the Update row offers, if any. A plain check is not
/// one: it has its own control.
fn next_update_action(update: &crate::update::Snapshot) -> Option<crate::update::UpdateAction> {
    update
        .primary
        .filter(|action| *action != crate::update::UpdateAction::Check)
}

fn update_check(update: &crate::update::Snapshot) -> UpdateCheck {
    use crate::update::State;

    if update.busy() {
        UpdateCheck::Running
    } else if matches!(update.state, State::Disabled | State::RestartRequired)
        || next_update_action(update).is_some()
    {
        UpdateCheck::Hidden
    } else if update.state == State::Error {
        UpdateCheck::Ready("settings-settings-retry")
    } else {
        UpdateCheck::Ready("settings-settings-check-for-updates")
    }
}

impl SettingsScreen {
    fn render_shortcuts_pane(&self, cx: &mut Context<Self>) -> Div {
        let modified_count = self.shortcut_list_cache.modified_count;
        let rows = &self.shortcut_list_cache.visible_indices;

        let mut pane = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section_title(&tr("settings-settings-keyboard-shortcuts")))
                    .when(
                        modified_count > 0 || self.shortcut_snapshot.last_error.is_some(),
                        |row| {
                            row.child(
                                widgets::ghost_button("shortcuts-reset-all")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.shortcut_reset_confirmation = true;
                                        this.shortcut_notice = None;
                                        cx.notify();
                                    }))
                                    .child(tr("settings-settings-reset-all")),
                            )
                        },
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(
                        tr("settings-settings-customize-the-shortcuts-maple-already-ships-this-page-does-not-ad"),
                    ),
            )
            .child(widgets::input_frame().text_sm().child(self.shortcut_search.clone()))
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(self.messages.get("settings-shortcut-counts", [("binding-count", (self.shortcut_snapshot.rows.len()).to_string().into()), ("modified-count", (modified_count).to_string().into())]).to_string()),
            );

        if let Some(error) = &self.shortcut_snapshot.last_error {
            pane = pane.child(widgets::banner(
                widgets::Tone::Warning,
                self.messages
                    .get(
                        "settings-shortcut-overrides-invalid",
                        [("error", (error).to_string().into())],
                    )
                    .to_string(),
            ));
        }
        if let Some(warning) = &self.shortcut_snapshot.compatibility_warning {
            pane = pane.child(widgets::banner(widgets::Tone::Warning, warning.clone()));
        }
        if let Some(notice) = &self.shortcut_notice {
            pane = pane.child(widgets::banner(widgets::Tone::Warning, notice.clone()));
        }
        if self.shortcut_reset_confirmation {
            pane = pane.child(
                widgets::banner(
                    widgets::Tone::Warning,
                    if self.shortcut_snapshot.last_error.is_some() && modified_count == 0 {
                        tr("settings-settings-clear-the-invalid-saved-shortcut-overrides")
                            .to_string()
                    } else {
                        self.messages
                            .get(
                                "settings-shortcut-reset-confirm",
                                [("modified-count", (modified_count).to_string().into())],
                            )
                            .to_string()
                    },
                )
                .child(
                    widgets::secondary_button("shortcuts-reset-all-confirm")
                        .flex_none()
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.request_shortcut_change(ShortcutSettingsChange::ResetAll, cx);
                        }))
                        .child(tr("settings-settings-reset")),
                )
                .child(
                    widgets::ghost_button("shortcuts-reset-all-cancel")
                        .flex_none()
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.shortcut_reset_confirmation = false;
                            cx.notify();
                        }))
                        .child(tr("settings-settings-cancel")),
                ),
            );
        }
        if let Some(recorder) = self.shortcut_recorder.as_ref() {
            pane = pane.child(self.render_shortcut_recorder(recorder, cx));
        }

        if rows.is_empty() {
            pane = pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(tr("settings-settings-no-shortcuts-match-this-search")),
            );
        } else {
            pane =
                pane.children(rows.iter().map(|&index| {
                    self.render_shortcut_row(&self.shortcut_snapshot.rows[index], cx)
                }));
        }
        pane
    }

    fn render_shortcut_recorder(&self, recorder: &ShortcutRecorder, cx: &mut Context<Self>) -> Div {
        let label = self
            .shortcut_snapshot
            .rows
            .iter()
            .find(|row| row.slot_id == recorder.slot_id)
            .map(|row| shortcut_labels::label(&row.label))
            .unwrap_or_else(|| recorder.slot_id.clone().into())
            .to_string();
        let sequence = if recorder.strokes.is_empty() {
            tr("settings-settings-waiting-for-keys").to_string()
        } else {
            recorder.strokes.join(" ")
        };
        let mut card = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded(theme::RADIUS_MD)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::accent()))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(self.messages.get("settings-shortcut-recording", [("label", (label).to_string().into())]).to_string()),
            )
            .child(shortcut_keycap(sequence, true))
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(if recorder.reviewing_conflicts {
                        tr("settings-settings-review-the-conflicts-below-before-saving")
                    } else {
                        tr("settings-settings-press-up-to-four-keys-enter-saves-backspace-removes-the-latest-st")
                    }),
            );

        if recorder.reviewing_conflicts {
            let can_keep_both = recorder.conflicts.iter().all(|conflict| {
                !matches!(conflict.kind, ShortcutConflictKind::Exact)
                    || !matches!(conflict.overlap, ShortcutContextOverlap::Equivalent)
            });
            card = card
                .children(recorder.conflicts.iter().map(|conflict| {
                    let other = self
                        .shortcut_snapshot
                        .rows
                        .iter()
                        .find(|row| row.slot_id == conflict.other_slot_id)
                        .map(|row| shortcut_labels::label(&row.label))
                        .unwrap_or_else(|| conflict.other_slot_id.clone().into());
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::status_warning()))
                        .child(
                            self.messages
                                .get(
                                    "settings-shortcut-collision",
                                    [
                                        (
                                            "conflict",
                                            (shortcut_conflict_label(conflict)).to_string().into(),
                                        ),
                                        ("other", (other).to_string().into()),
                                        ("slot-id", (conflict.other_slot_id).to_string().into()),
                                    ],
                                )
                                .to_string(),
                        )
                }))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            widgets::primary_button("shortcut-conflict-replace")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.save_recorded_shortcut(true, cx);
                                }))
                                .child(tr("settings-settings-replace-conflicts")),
                        )
                        .when(can_keep_both, |actions| {
                            actions.child(
                                widgets::ghost_button("shortcut-conflict-keep")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.save_recorded_shortcut(false, cx);
                                    }))
                                    .child(tr("settings-settings-keep-both")),
                            )
                        })
                        .child(
                            widgets::ghost_button("shortcut-conflict-cancel")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.stop_shortcut_recording();
                                    cx.notify();
                                }))
                                .child(tr("settings-settings-cancel")),
                        ),
                );
        } else {
            card = card.child(
                widgets::ghost_button("shortcut-record-cancel")
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.stop_shortcut_recording();
                        cx.notify();
                    }))
                    .child(tr("settings-settings-cancel")),
            );
        }
        card
    }

    fn render_shortcut_row(
        &self,
        row: &crate::shortcuts::ShortcutRow,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let slot_id = row.slot_id.clone();
        let record_id = slot_id.clone();
        let disable_id = slot_id.clone();
        let reset_id = slot_id.clone();
        let current = row
            .current_sequence
            .clone()
            .unwrap_or_else(|| tr("settings-settings-unbound").to_string());
        let card = widgets::card_row()
            .flex()
            .items_center()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(gpui::rgb(theme::text_primary()))
                                    .child(shortcut_labels::label(&row.label)),
                            )
                            .when(row.modified, |title| {
                                title.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_full()
                                        .text_xs()
                                        .bg(gpui::rgb(theme::bg_sidebar_pill()))
                                        .text_color(gpui::rgb(theme::accent()))
                                        .child(tr("settings-settings-modified")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(format!(
                                "{} · {}",
                                shortcut_labels::category(&row.category),
                                shortcut_context_label(row.context.as_deref()),
                            )),
                    )
                    .when(!row.conflicts.is_empty(), |column| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(theme::status_warning()))
                                .child(
                                    self.messages
                                        .get(
                                            "settings-shortcut-conflicts",
                                            [("count", (row.conflicts.len()).to_string().into())],
                                        )
                                        .to_string(),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_1()
                    .child(shortcut_keycap(current, row.current_sequence.is_some()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(
                                self.messages
                                    .get(
                                        "settings-shortcut-default",
                                        [("shortcut", (row.default_sequence).to_string().into())],
                                    )
                                    .to_string(),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        widgets::ghost_button(gpui::SharedString::from(format!(
                            "shortcut-record-{slot_id}"
                        )))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.begin_shortcut_recording(record_id.clone(), cx);
                        }))
                        .child(tr("settings-settings-record")),
                    )
                    .when(row.current_sequence.is_some(), |actions| {
                        actions.child(
                            widgets::ghost_button(gpui::SharedString::from(format!(
                                "shortcut-disable-{slot_id}"
                            )))
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.request_shortcut_change(
                                    ShortcutSettingsChange::Disable {
                                        slot_id: disable_id.clone(),
                                    },
                                    cx,
                                );
                            }))
                            .child(tr("settings-settings-disable")),
                        )
                    })
                    .when(row.modified, |actions| {
                        actions.child(
                            widgets::ghost_button(gpui::SharedString::from(format!(
                                "shortcut-reset-{slot_id}"
                            )))
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.request_shortcut_change(
                                    ShortcutSettingsChange::Reset {
                                        slot_id: reset_id.clone(),
                                    },
                                    cx,
                                );
                            }))
                            .child(tr("settings-settings-reset")),
                        )
                    }),
            );
        self.application_target(|| SettingsTarget::Shortcut(row.slot_id.clone()), card)
    }

    fn render_prompt_pane(&self, cx: &mut Context<Self>) -> Div {
        let mut pane = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title(&tr("settings-settings-additional-instructions")))
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(
                        tr("settings-settings-added-to-every-task-after-maple-s-own-system-prompt-use-it-for-pr"),
                    ),
            )
            .child(
                self.application_target(
                    || SettingsTarget::PromptEditor,
                    div()
                        .p_3()
                        .rounded(theme::RADIUS_SM)
                        .bg(gpui::rgb(theme::bg_input()))
                        .border_1()
                        .border_color(gpui::rgb(theme::border()))
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_primary()))
                        .child(self.prompt_editor.clone()),
                ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.application_target(
                        || SettingsTarget::PromptSave,
                        pill_button(
                            "prompt-save".to_string(),
                            tr("settings-settings-save"),
                            true,
                            cx.listener(|this, _event, _window, cx| this.save_prompt(cx)),
                        ),
                    ))
                    .child(self.application_target(
                        || SettingsTarget::PromptClear,
                        pill_button(
                            "prompt-clear".to_string(),
                            tr("settings-settings-clear"),
                            false,
                            cx.listener(|this, _event, _window, cx| this.clear_prompt(cx)),
                        ),
                    )),
            );
        if let Some(notice) = &self.prompt_notice {
            pane = pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(notice.clone()),
            );
        }
        pane
    }

    fn render_integrations_pane(&self, cx: &mut Context<Self>) -> Div {
        let mut pane = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title(&tr("settings-settings-integrations")))
            .child(div().text_sm().child(match self.integrations_backend.remote_host() {
                Some(host) => self.messages.get("settings-execution-computer", [("computer", (host[..host.len().min(10)]).to_string().into())]).to_string(),
                None => tr("settings-settings-execution-computer-this-computer").to_string(),
            }))
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(tr("settings-settings-manage-integrations-here-enable-coding-agents-to-make-them-availa")),
            );

        if let Some(notice) = &self.integration_notice {
            pane = pane.child(widgets::notice(
                "integration-notice-close",
                notice.resolve(),
                cx.listener(|this, _event, _window, cx| {
                    this.integration_notice = None;
                    cx.notify();
                }),
            ));
        }

        pane = pane.child(subsection_title(&tr("settings-built-in")));
        match &self.integrations {
            None => {
                pane = pane.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(tr("settings-settings-detecting-integrations")),
                );
            }
            Some(integrations) => {
                let mut visible = false;
                for integration in integrations
                    .iter()
                    .filter(|integration| integration_is_visible(integration))
                {
                    visible = true;
                    pane = pane.child(self.render_integration_card(integration, cx));
                }
                if !visible {
                    pane = pane.child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(tr("settings-settings-no-supported-integrations-detected-on-this-device")),
                    );
                }
            }
        }

        match &self.mcp_servers {
            Some(servers) if !servers.is_empty() => {
                pane = pane.child(catalog::section_heading(subsection_title(&tr(
                    "settings-settings-connected",
                ))));
                pane = pane.children(servers.iter().map(|server| {
                    let name = server.name.clone();
                    let summary = match &server.target {
                        McpTarget::Stdio { command } => format!("STDIO · {command}"),
                        McpTarget::StreamableHttp { url } => format!("HTTP · {url}"),
                    };
                    let card = widgets::card_row()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(gpui::rgb(theme::text_primary()))
                                        .child(server.name.clone()),
                                )
                                .when(!server.description.is_empty(), |col| {
                                    col.child(
                                        div()
                                            .text_xs()
                                            .text_color(gpui::rgb(theme::text_secondary()))
                                            .child(catalog::saved_description(
                                                &server.name,
                                                &server.description,
                                            )),
                                    )
                                })
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(gpui::rgb(theme::text_muted()))
                                        .line_clamp(1)
                                        .child(summary),
                                ),
                        )
                        .child(pill_button(
                            format!("mcp-toggle-{name}"),
                            if server.enabled {
                                tr("settings-settings-enabled")
                            } else {
                                tr("settings-settings-disabled")
                            },
                            server.enabled,
                            cx.listener({
                                let name = name.clone();
                                move |this, _event, _window, cx| this.toggle_mcp_server(&name, cx)
                            }),
                        ))
                        .child(
                            widgets::icon_button(
                                gpui::SharedString::from(format!("mcp-edit-{name}")),
                                "pencil",
                                tr("settings-settings-edit"),
                                widgets::ROW_ICON,
                                theme::text_secondary(),
                            )
                            .on_click(cx.listener({
                                let name = name.clone();
                                move |this, _event, _window, cx| this.edit_mcp_server(&name, cx)
                            })),
                        )
                        .child(
                            widgets::icon_button(
                                gpui::SharedString::from(format!("mcp-remove-{name}")),
                                "trash-2",
                                tr("settings-settings-remove"),
                                widgets::ROW_ICON,
                                theme::status_error(),
                            )
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| this.remove_mcp_server(&name, cx),
                            )),
                        );
                    self.application_target(|| SettingsTarget::McpServer(server.name.clone()), card)
                }));
            }
            _ => {}
        }
        pane.child(self.render_app_catalog(cx))
    }

    fn render_integration_card(
        &self,
        integration: &AgentIntegration,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (availability, availability_color) = integration_availability(integration);
        let can_toggle = integration_can_toggle(integration);
        let can_setup = integration_can_setup(integration);
        let saving = self.integration_saving.contains(&integration.id);
        let enabled = integration.enabled_for_new_tasks;
        let cua_driver = is_cua_driver(&integration.id);
        let display_name = if cua_driver {
            tr("settings-integration-cua-name")
        } else {
            integration.name.clone().into()
        };
        let description =
            presentation::integration_description(&integration.id, &integration.description);
        let mut details = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(display_name),
                    )
                    .child(widgets::status_badge(availability, availability_color)),
            )
            .when(!description.is_empty(), |col| {
                col.child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_secondary()))
                        .child(description),
                )
            });
        if cua_driver {
            details = details.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(tr(CUA_CREDIT))
                    .child(
                        div()
                            .flex()
                            .child(
                                div()
                                    .id("cua-project-link")
                                    .text_color(gpui::rgb(theme::link()))
                                    .hover(|style| style.underline().cursor_pointer())
                                    .on_click(|_, _, _| {
                                        crate::ui::rich_text::open_link(CUA_PROJECT_URL)
                                    })
                                    .child(tr("settings-settings-cua-team")),
                            )
                            .child("."),
                    ),
            );
            let mut metadata = div().flex().flex_wrap().items_center().gap_2();
            metadata = metadata.child(integration_metadata_badge(cua_backend_label(integration)));
            for permission in integration
                .permissions
                .iter()
                .flat_map(|permissions| permissions.required.iter())
            {
                metadata = metadata.child(cua_permission_badge(
                    presentation::permission(permission.kind),
                    permission.granted,
                    &self.messages,
                ));
            }
            details = details.child(metadata);
        } else if let Some(version) = integration.version.as_deref() {
            details = details.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(
                        self.messages
                            .get(
                                "settings-integration-version",
                                [("version", (version).to_string().into())],
                            )
                            .to_string(),
                    ),
            );
        }
        if let Some(detail) = &integration.detail {
            details = details.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    // A detail can carry the address of something the user has
                    // to fetch, which needs more room than a short status line.
                    .line_clamp(3)
                    .child(presentation::integration_detail(integration, detail)),
            );
        }

        let mut actions = div().flex().flex_none().flex_col().items_end().gap_2();
        if can_setup {
            let id = integration.id.clone();
            actions = actions.child(
                self.application_target(
                    || SettingsTarget::IntegrationSetup(integration.id.clone()),
                    pill_button(
                        format!("integration-setup-{}", integration.id),
                        tr("settings-settings-set-up"),
                        false,
                        cx.listener(move |this, _event, _window, cx| {
                            this.setup_integration(&id, cx);
                        }),
                    )
                    .when(saving, |button| button.opacity(0.5)),
                ),
            );
        }
        if can_toggle {
            let id = integration.id.clone();
            actions = actions.child(
                self.application_target(
                    || SettingsTarget::Integration(integration.id.clone()),
                    pill_button(
                        format!("integration-toggle-{}", integration.id),
                        if enabled {
                            tr("settings-settings-enabled")
                        } else {
                            tr("settings-settings-enable")
                        },
                        enabled,
                        cx.listener(move |this, _event, _window, cx| {
                            this.toggle_integration(&id, cx);
                        }),
                    )
                    .when(saving, |button| button.opacity(0.5)),
                ),
            );
        }

        let card = widgets::card_row()
            .flex()
            .items_start()
            .gap_3()
            .child(
                div()
                    .size(px(44.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    .bg(gpui::rgb(theme::bg_sidebar_card()))
                    .border_1()
                    .border_color(gpui::rgb(theme::border_subtle()))
                    .child(if cua_driver {
                        gpui::svg()
                            .path(if theme::is_light() {
                                "icons/cua-mark-black.svg"
                            } else {
                                "icons/cua-mark-white.svg"
                            })
                            .size(px(28.))
                            .text_color(gpui::rgb(theme::text_primary()))
                            .into_any_element()
                    } else if matches!(integration.id.as_str(), "codex" | "claude") {
                        gpui::svg()
                            .path(if integration.id == "claude" {
                                "icons/claude-mark.svg"
                            } else {
                                "icons/openai-mark.svg"
                            })
                            .size(px(26.))
                            .text_color(gpui::rgb(theme::text_primary()))
                            .into_any_element()
                    } else {
                        icon("plug", widgets::ROW_ICON, theme::text_secondary()).into_any_element()
                    }),
            )
            .child(details)
            .when(can_setup || can_toggle, |card| card.child(actions));
        card.into_any_element()
    }

    fn render_mcp_editor(&self, editor: &McpEditor, cx: &mut Context<Self>) -> Div {
        let signing_in = self.mcp_editor_signing_in(cx);
        let sign_in_error = self.sign_in_error_for(editor.name.read(cx).text().trim());
        let field = |label: &str, hint: &str, input: Entity<TextInput>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(gpui::rgb(theme::text_secondary()))
                        .child(label.to_string()),
                )
                .child(widgets::input_frame().text_sm().child(input))
                .when(!hint.is_empty(), |col| {
                    col.child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(hint.to_string()),
                    )
                })
        };
        let transport_segment = |id: &'static str, label: gpui::SharedString, active: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded(theme::RADIUS_SM)
                .text_sm()
                .text_color(gpui::rgb(if active {
                    theme::text_primary()
                } else {
                    theme::text_secondary()
                }))
                .when(active, |el| el.bg(gpui::rgb(theme::bg_sidebar_row_hover())))
                .hover(|style| {
                    style
                        .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                        .cursor_pointer()
                })
                .child(label)
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded(theme::RADIUS_MD)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(if editor.original_name.is_some() {
                        tr("settings-settings-edit-server")
                    } else {
                        tr("settings-settings-new-server")
                    }),
            )
            .child(field(&tr("settings-settings-name"), "", editor.name.clone()))
            .child(field(&tr("settings-settings-description"), "", editor.description.clone()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(gpui::rgb(theme::text_secondary()))
                            .child(tr("settings-settings-transport")),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .p_1()
                            .rounded(theme::RADIUS_SM)
                            .bg(gpui::rgb(theme::bg_sidebar_chrome()))
                            .w(px(320.))
                            .child(
                                transport_segment(
                                    "mcp-transport-stdio",
                                    tr("settings-settings-standard-io-stdio"),
                                    !editor.http,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        if let Some(editor) = this.mcp_editor.as_mut() {
                                            editor.http = false;
                                            editor.target.update(cx, |input, cx| {
                                                input.set_placeholder(
                                                    mcp_target_placeholder(false),
                                                    cx,
                                                )
                                            });
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                transport_segment(
                                    "mcp-transport-http",
                                    tr("settings-settings-streamable-http"),
                                    editor.http,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        if let Some(editor) = this.mcp_editor.as_mut() {
                                            editor.http = true;
                                            editor.target.update(cx, |input, cx| {
                                                input.set_placeholder(
                                                    mcp_target_placeholder(true),
                                                    cx,
                                                )
                                            });
                                        }
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            .child(if editor.http {
                field(&tr("settings-settings-url"), "", editor.target.clone())
            } else {
                field(&tr("settings-settings-command"), "", editor.target.clone())
            })
            .when(!editor.saved_environment.is_empty(), |col| {
                col.child(subsection_title(&tr("settings-settings-environment-values")))
                    .child(self.render_mcp_secret_fields(&editor.saved_environment, false, cx))
            })
            .when(!editor.saved_headers.is_empty(), |col| {
                col.child(subsection_title(&tr("settings-settings-header-values")))
                    .child(self.render_mcp_secret_fields(&editor.saved_headers, true, cx))
            })
            .child(field(
                &tr("settings-settings-new-environment-values"),
                &tr("settings-settings-key-value-pairs-separated-by-semicolons-existing-values-use-the-c"),
                editor.environment.clone(),
            ))
            .when(editor.http, |col| {
                col.child(field(
                    &tr("settings-settings-new-headers"),
                    &tr("settings-settings-name-value-pairs-separated-by-semicolons"),
                    editor.headers.clone(),
                ))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pt_1()
                    .child(
                        widgets::primary_button("mcp-save")
                            .when(self.mcp_saving, |el| el.opacity(0.5))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.submit_mcp_editor(cx);
                            }))
                            .child(tr("settings-settings-save")),
                    )
                    .when(
                        editor.http && editor.original.is_none(),
                        |row| {
                            row.child(if signing_in {
                                widgets::ghost_button("mcp-sign-in")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.cancel_mcp_sign_in(cx);
                                    }))
                                    .child(spinner(
                                        "mcp-sign-in",
                                        widgets::ROW_ICON,
                                        theme::text_secondary(),
                                    ))
                                    .child(tr("settings-catalog-stop-sign-in"))
                            } else {
                                widgets::ghost_button("mcp-sign-in")
                                    .when(self.mcp_signing_in.is_some(), |el| el.opacity(0.5))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.sign_in_mcp_editor(cx);
                                    }))
                                    .child(tr("settings-catalog-browser-sign-in"))
                            })
                        },
                    )
                    .child(
                        widgets::ghost_button("mcp-cancel")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                if this.mcp_editor_signing_in(cx) {
                                    this.cancel_mcp_sign_in(cx);
                                }
                                this.mcp_editor = None;
                                this.mcp_notice = None;
                                cx.notify();
                            }))
                            .child(tr("settings-settings-cancel")),
                    ),
            )
            .when_some(sign_in_error, |form, message| {
                form.child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::status_error()))
                        .child(message),
                )
            })
    }
}

/// Plan usage meter: plan pill, percent used, reset date, progress bar.
/// Mirrors the sidebar card at a larger size.
fn plan_card(plan: &crate::billing::PlanUsage, messages: &super::i18n::MessageCache) -> Div {
    let fraction = f32::from(plan.percent_used) / 100.0;
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .px_4()
        .py_4()
        .rounded(theme::RADIUS_MD)
        .bg(gpui::rgb(theme::bg_sidebar_card()))
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .text_base()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(gpui::rgb(theme::text_primary()))
                        .whitespace_nowrap()
                        .child(
                            messages
                                .get(
                                    "settings-usage-percent",
                                    [("percent", (plan.percent_used).to_string().into())],
                                )
                                .to_string(),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .whitespace_nowrap()
                        .child(
                            messages
                                .get(
                                    "settings-usage-resets",
                                    [(
                                        "reset",
                                        presentation::reset_date(plan, messages).to_string().into(),
                                    )],
                                )
                                .to_string(),
                        ),
                ),
        )
        .child(
            div()
                .w_full()
                .h(px(6.))
                .rounded_full()
                // Darker than the card so the empty part of the track shows.
                .bg(gpui::rgb(theme::bg_sidebar_pill()))
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(fraction.clamp(0., 1.)))
                        .rounded_full()
                        .bg(gpui::rgb(theme::accent())),
                ),
        )
}

fn integration_is_visible(integration: &AgentIntegration) -> bool {
    // External agents get a row even before installation: the card says how
    // to get it, where a hidden row would leave the feature undiscoverable.
    if integration.is_external_agent() {
        return true;
    }
    if is_cua_driver(&integration.id)
        && matches!(
            integration.availability,
            AgentIntegrationAvailability::NotDetected
        )
    {
        return false;
    }
    integration.enabled_for_new_tasks
        || !matches!(
            &integration.availability,
            AgentIntegrationAvailability::NotDetected
        )
}

fn integration_can_enable(integration: &AgentIntegration) -> bool {
    if !is_cua_driver(&integration.id) {
        return matches!(
            &integration.availability,
            AgentIntegrationAvailability::Available
        );
    }

    cua_permissions_ready(integration.permissions.as_ref())
}

fn integration_can_toggle(integration: &AgentIntegration) -> bool {
    integration.enabled_for_new_tasks || integration_can_enable(integration)
}

fn integration_can_setup(integration: &AgentIntegration) -> bool {
    is_cua_driver(&integration.id)
        && integration.setup_available
        && !matches!(
            integration.availability,
            AgentIntegrationAvailability::NotDetected
        )
        && !cua_permissions_ready(integration.permissions.as_ref())
}

fn integration_availability(integration: &AgentIntegration) -> (gpui::SharedString, u32) {
    if integration.enabled_for_new_tasks
        && is_cua_driver(&integration.id)
        && matches!(
            &integration.backend,
            Some(AgentIntegrationBackend::Embedded)
        )
        && !cua_permissions_ready(integration.permissions.as_ref())
    {
        return (tr("settings-settings-needs-setup"), theme::status_warning());
    }

    match &integration.availability {
        AgentIntegrationAvailability::NotDetected => {
            (tr("settings-settings-not-detected"), theme::text_muted())
        }
        AgentIntegrationAvailability::Available if is_cua_driver(&integration.id) => {
            if cua_permissions_ready(integration.permissions.as_ref()) {
                (tr("settings-settings-ready"), theme::status_success())
            } else {
                (tr("settings-settings-available"), theme::status_success())
            }
        }
        AgentIntegrationAvailability::Available => {
            (tr("settings-settings-detected"), theme::status_success())
        }
        AgentIntegrationAvailability::SetupRequired => (
            tr("settings-settings-setup-required"),
            theme::status_warning(),
        ),
    }
}

fn cua_permissions_ready(permissions: Option<&AgentIntegrationPermissions>) -> bool {
    permissions.is_some_and(AgentIntegrationPermissions::ready)
}

fn integration_setup_notice(permissions: &AgentIntegrationPermissions) -> Option<UiText> {
    use maple_agent::agent::AgentIntegrationPermissionKind;
    Some(UiText::key(match permissions.first_missing()? {
        AgentIntegrationPermissionKind::Accessibility => {
            "settings-permission-guidance-accessibility"
        }
        AgentIntegrationPermissionKind::ScreenRecording => {
            "settings-permission-guidance-screen-recording"
        }
        AgentIntegrationPermissionKind::DesktopHelper => return None,
    }))
}

/// Credit for the open-source project behind computer use, whose mark is
/// the card's icon. The card follows it with "Cua team", linked.
const CUA_CREDIT: &str = "settings-settings-powered-by-cua-an-open-source-project-from-the";
const CUA_PROJECT_URL: &str = "https://cua.ai";

fn cua_backend_label(_integration: &AgentIntegration) -> gpui::SharedString {
    tr("settings-settings-built-into-maple")
}

fn integration_metadata_badge(label: gpui::SharedString) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .border_color(gpui::rgb(theme::border()))
        .text_xs()
        .text_color(gpui::rgb(theme::text_muted()))
        .child(label)
}

fn cua_permission_badge(
    label: gpui::SharedString,
    granted: bool,
    messages: &super::i18n::MessageCache,
) -> Div {
    let (status, color) = if granted {
        ("settings-permission-granted", theme::status_success())
    } else {
        ("settings-permission-needed", theme::status_warning())
    };
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .border_color(gpui::rgb(theme::border()))
        .flex()
        .items_center()
        .gap_1p5()
        .text_xs()
        .text_color(gpui::rgb(theme::text_muted()))
        .child(div().size(px(5.)).rounded_full().bg(gpui::rgb(color)))
        .child(messages.get(status, [("permission", label.to_string().into())]))
}

fn is_cua_driver(id: &str) -> bool {
    id == "cua-driver"
}

/// Small on/off pill used in list rows.
fn pill_button(
    id: String,
    label: gpui::SharedString,
    on: bool,
    handler: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(gpui::SharedString::from(id))
        .px_2p5()
        .py_1()
        .rounded_full()
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .bg(gpui::rgb(if on {
            theme::accent()
        } else {
            theme::bg_sidebar_card()
        }))
        .text_color(gpui::rgb(if on {
            theme::on_accent()
        } else {
            theme::text_secondary()
        }))
        .hover(|style| style.opacity(0.85).cursor_pointer())
        .active(|style| style.opacity(0.7))
        .on_click(handler)
        .child(label)
}

/// Whether a key press presses the focused control, as a click does.
fn activates(event: &gpui::KeyDownEvent) -> bool {
    !event.keystroke.modifiers.modified()
        && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
}

/// Pane heading in the brand display face, like the section titles of
/// the brand kit.
fn section_title(label: &str) -> Div {
    div()
        .font_family(crate::assets::FONT_DISPLAY)
        .text_size(gpui::px(26.))
        .line_height(gpui::px(32.))
        .text_color(gpui::rgb(theme::display_text()))
        .child(label.to_string())
}

fn subsection_title(label: &str) -> Div {
    div()
        .text_sm()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(gpui::rgb(theme::text_primary()))
        .child(label.to_string())
}

fn shortcut_keycap(label: String, bound: bool) -> Div {
    div()
        .px_2p5()
        .py_1()
        .rounded(theme::RADIUS_SM)
        .border_1()
        .border_color(gpui::rgb(theme::border()))
        .bg(gpui::rgb(theme::bg_input()))
        .text_xs()
        .font_family(crate::assets::FONT_MONO)
        .text_color(gpui::rgb(if bound {
            theme::text_primary()
        } else {
            theme::text_faint()
        }))
        .child(label)
}

fn shortcut_context_label(context: Option<&str>) -> gpui::SharedString {
    match context {
        None => tr("settings-settings-global"),
        Some("Chat") => tr("settings-settings-chat"),
        Some("Transcript") => tr("settings-settings-transcript"),
        Some(crate::ui::popup::MENU_CONTEXT) => tr("settings-settings-menus"),
        Some("TextInput") | Some(crate::keymap::STANDARD_TEXT_CONTEXT) => {
            tr("settings-settings-text-fields")
        }
        Some(crate::ui::text_input::vim_actions::NORMAL_CONTEXT) => {
            tr("settings-settings-composer-normal")
        }
        Some(crate::ui::text_input::vim_actions::VISUAL_CONTEXT) => {
            tr("settings-settings-composer-visual")
        }
        Some(crate::ui::text_input::vim_actions::INSERT_CONTEXT) => {
            tr("settings-settings-composer-insert")
        }
        Some(crate::ui::text_input::vim_actions::WAITING_CONTEXT) => {
            tr("settings-settings-composer-pending-key")
        }
        Some(context) => context.to_string().into(),
    }
}

fn shortcut_row_matches(row: &crate::shortcuts::ShortcutRow, query: &str) -> bool {
    query.is_empty()
        || shortcut_labels::label(&row.label)
            .to_lowercase()
            .contains(query)
        || row.slot_id.to_lowercase().contains(query)
        || shortcut_labels::category(&row.category)
            .to_lowercase()
            .contains(query)
        || row
            .context
            .as_deref()
            .unwrap_or("global")
            .to_lowercase()
            .contains(query)
        || shortcut_context_label(row.context.as_deref())
            .to_lowercase()
            .contains(query)
        || row.default_sequence.to_lowercase().contains(query)
        || row
            .current_sequence
            .as_deref()
            .is_some_and(|sequence| sequence.to_lowercase().contains(query))
}

fn shortcut_conflict_label(conflict: &ShortcutConflict) -> String {
    let kind = match conflict.kind {
        ShortcutConflictKind::Exact => tr("settings-settings-exact-collision"),
        ShortcutConflictKind::Prefix => tr("settings-settings-prefix-collision"),
    };
    let overlap = match conflict.overlap {
        ShortcutContextOverlap::Equivalent => tr("settings-settings-same-context"),
        ShortcutContextOverlap::Scoped => tr("settings-settings-overlapping-scoped-context"),
        ShortcutContextOverlap::Possible => tr("settings-settings-possibly-overlapping-context"),
    };
    format!("{kind}, {overlap}")
}

fn portable_keystroke_token(keystroke: &gpui::Keystroke) -> String {
    let mut parts = Vec::with_capacity(6);
    if keystroke.modifiers.secondary() {
        parts.push("secondary".to_string());
    }
    if keystroke.modifiers.control && cfg!(target_os = "macos") {
        parts.push("ctrl".to_string());
    }
    if keystroke.modifiers.alt {
        parts.push("alt".to_string());
    }
    if keystroke.modifiers.shift {
        parts.push("shift".to_string());
    }
    if keystroke.modifiers.platform && !cfg!(target_os = "macos") {
        parts.push("cmd".to_string());
    }
    if keystroke.modifiers.function {
        parts.push("fn".to_string());
    }
    parts.push(match keystroke.key.as_str() {
        " " => "space".to_string(),
        key => key.to_lowercase(),
    });
    parts.join("-")
}

/// Title and description column shared by every settings row.
pub(crate) fn setting_copy(title: impl AsRef<str>, description: impl AsRef<str>) -> Div {
    let (title, description) = (title.as_ref(), description.as_ref());
    div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w_0()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(theme::text_primary()))
                .child(title.to_string()),
        )
        .child(
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(description.to_string()),
        )
}

/// One dropdown choice as data: the label shown in the menu and whether
/// it is the saved value.
struct SettingOption {
    label: String,
    current: bool,
}

/// An on/off setting with a switch. The whole row is the click target.
fn toggle_row(
    title: gpui::SharedString,
    description: gpui::SharedString,
    on: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<Div> {
    widgets::card_row()
        .id("setting-automatically-download-updates")
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .hover(|style| style.cursor_pointer())
        .on_click(on_click)
        .child(setting_copy(title, description))
        .child(widgets::switch(
            gpui::SharedString::from(format!(
                "setting-switch-{}",
                "automatically-download-updates"
            )),
            on,
        ))
}

fn info_row(label: &str, value: String) -> Div {
    widgets::card_row()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .child(
            div()
                .flex_shrink_0()
                .text_sm()
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_right()
                .text_sm()
                .text_color(gpui::rgb(theme::text_primary()))
                .child(value),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_row_offers_a_check_only_when_nothing_else_is_next() {
        use crate::update::{Snapshot, State, UpdateAction};

        let snapshot = |state, primary| {
            let mut update = Snapshot::initial();
            update.state = state;
            update.primary = primary;
            update
        };
        let check = UpdateCheck::Ready("settings-settings-check-for-updates");

        assert_eq!(update_check(&snapshot(State::Idle, None)), check);
        assert_eq!(
            update_check(&snapshot(State::Current, Some(UpdateAction::Check))),
            check
        );
        assert_eq!(
            update_check(&snapshot(State::Error, Some(UpdateAction::Check))),
            UpdateCheck::Ready("settings-settings-retry")
        );
        for state in [State::Checking, State::Downloading, State::Installing] {
            assert_eq!(update_check(&snapshot(state, None)), UpdateCheck::Running);
        }
        assert_eq!(
            update_check(&snapshot(State::Available, Some(UpdateAction::Download))),
            UpdateCheck::Hidden
        );
        assert_eq!(
            update_check(&snapshot(State::Ready, Some(UpdateAction::Restart))),
            UpdateCheck::Hidden
        );
        assert_eq!(
            update_check(&snapshot(
                State::RestartRequired,
                Some(UpdateAction::Relaunch)
            )),
            UpdateCheck::Hidden
        );
        assert_eq!(
            update_check(&snapshot(State::Disabled, None)),
            UpdateCheck::Hidden
        );
    }

    fn shortcut_row(
        slot_id: &str,
        label: &str,
        category: &str,
        context: Option<&str>,
        default_sequence: &str,
        current_sequence: Option<&str>,
        modified: bool,
    ) -> crate::shortcuts::ShortcutRow {
        crate::shortcuts::ShortcutRow {
            slot_id: slot_id.to_string(),
            label: label.to_string(),
            category: category.to_string(),
            context: context.map(str::to_string),
            default_sequence: default_sequence.to_string(),
            current_sequence: current_sequence.map(str::to_string),
            modified,
            conflicts: Vec::new(),
        }
    }

    fn server(name: &str) -> McpDefinition {
        McpDefinition {
            key: maple_agent::agent::AgentMcpServer::key_for_name(name),
            name: name.to_string(),
            description: String::new(),
            enabled: true,
            timeout_seconds: 300,
            target: McpTarget::Stdio {
                command: "cmd".to_string(),
            },
            environment: Vec::new(),
            headers: Vec::new(),
        }
    }

    fn integration(
        availability: AgentIntegrationAvailability,
        enabled_for_new_tasks: bool,
    ) -> AgentIntegration {
        let ready = matches!(availability, AgentIntegrationAvailability::Available);

        AgentIntegration {
            id: "cua-driver".to_string(),
            name: "Computer use".to_string(),
            description: "Computer use".to_string(),
            availability,
            version: Some("1.0.0".to_string()),
            enabled_for_new_tasks,
            detail: None,
            permissions: Some(macos_permissions(ready, ready)),
            setup_available: true,
            backend: Some(AgentIntegrationBackend::Embedded),
        }
    }

    /// The two macOS grants, in the order the platform asks for them.
    fn macos_permissions(
        accessibility: bool,
        screen_recording: bool,
    ) -> AgentIntegrationPermissions {
        use maple_agent::agent::AgentIntegrationPermissionKind;

        AgentIntegrationPermissions::default()
            .with(AgentIntegrationPermissionKind::Accessibility, accessibility)
            .with(
                AgentIntegrationPermissionKind::ScreenRecording,
                screen_recording,
            )
    }

    #[test]
    fn shortcut_result_preserves_unrelated_general_setting() {
        let mut settings = AppSettings::default();
        settings.default_web_enabled = !settings.default_web_enabled;
        let expected_web_enabled = settings.default_web_enabled;
        settings
            .shortcut_overrides
            .insert("chat.focus_search".into(), None);
        let overrides = ShortcutOverrides::from([(
            "chat.new_task".to_owned(),
            Some("secondary-shift-n".to_owned()),
        )]);

        merge_shortcut_overrides(&mut settings, overrides.clone());

        assert_eq!(settings.default_web_enabled, expected_web_enabled);
        assert_eq!(settings.shortcut_overrides, overrides);
    }

    #[test]
    fn renaming_onto_another_server_is_a_collision() {
        let servers = [server("alpha"), server("beta")];
        // Adding a new server with a taken name.
        assert!(name_collides(&servers, None, "alpha"));
        assert!(!name_collides(&servers, None, "gamma"));
        // Editing keeps its own name.
        assert!(!name_collides(&servers, Some("alpha"), "alpha"));
        // Renaming onto a sibling.
        assert!(name_collides(&servers, Some("alpha"), "beta"));
        assert!(!name_collides(&servers, Some("alpha"), "gamma"));
    }

    #[test]
    fn integration_cards_only_offer_valid_explicit_actions() {
        let available = integration(AgentIntegrationAvailability::Available, false);
        assert!(integration_is_visible(&available));
        assert!(integration_can_enable(&available));
        assert!(integration_can_toggle(&available));
        assert!(!integration_can_setup(&available));

        let setup_required = integration(AgentIntegrationAvailability::SetupRequired, false);
        assert!(integration_is_visible(&setup_required));
        assert!(!integration_can_enable(&setup_required));
        assert!(!integration_can_toggle(&setup_required));
        assert!(integration_can_setup(&setup_required));

        let missing = integration(AgentIntegrationAvailability::NotDetected, false);
        assert!(!integration_is_visible(&missing));
        assert!(!integration_can_toggle(&missing));

        // A platform with no CUA backend reports the integration undetected,
        // so the row stays hidden even if stale device-local state exists.
        let enabled_but_missing = integration(AgentIntegrationAvailability::NotDetected, true);
        assert!(!integration_is_visible(&enabled_but_missing));
        assert!(integration_can_toggle(&enabled_but_missing));

        // Other integrations retain the generic stale-default behavior.
        let mut generic_missing = enabled_but_missing.clone();
        generic_missing.id = "other".to_string();
        assert!(integration_is_visible(&generic_missing));

        // Codex shows even when it is not installed, so the card can say
        // how to install it; it can be enabled only once it is detected.
        let mut codex_missing = integration(AgentIntegrationAvailability::NotDetected, false);
        codex_missing.id = "codex".to_string();
        assert!(integration_is_visible(&codex_missing));
        assert!(!integration_can_enable(&codex_missing));
        assert!(!integration_can_setup(&codex_missing));
        let mut codex_ready = integration(AgentIntegrationAvailability::Available, false);
        codex_ready.id = "codex".to_string();
        assert!(integration_can_enable(&codex_ready));
        assert!(!integration_can_setup(&codex_ready));
        let mut claude_missing = integration(AgentIntegrationAvailability::NotDetected, false);
        claude_missing.id = "claude".to_string();
        assert!(integration_is_visible(&claude_missing));
        assert!(!integration_can_enable(&claude_missing));
        let mut claude_needs_setup =
            integration(AgentIntegrationAvailability::SetupRequired, false);
        claude_needs_setup.id = "claude".to_string();
        assert!(integration_is_visible(&claude_needs_setup));
        assert!(!integration_can_enable(&claude_needs_setup));

        // An embedded selection is not enableable after Maple's own grants
        // have been revoked. The setup action is the only route back to a
        // ready embedded backend.
        let mut revoked_embedded = integration(AgentIntegrationAvailability::Available, true);
        revoked_embedded.permissions = Some(macos_permissions(false, false));
        revoked_embedded.enabled_for_new_tasks = false;
        assert!(!integration_can_enable(&revoked_embedded));
        assert!(!integration_can_toggle(&revoked_embedded));
        assert!(integration_can_setup(&revoked_embedded));
    }

    #[test]
    fn built_in_cua_is_ready_only_after_both_maple_permissions() {
        assert!(!cua_permissions_ready(None));
        assert!(!cua_permissions_ready(Some(&macos_permissions(
            true, false
        ))));
        assert!(!cua_permissions_ready(Some(&macos_permissions(
            false, true
        ))));
        assert!(cua_permissions_ready(Some(&macos_permissions(true, true))));
    }

    #[test]
    fn setup_is_not_offered_once_only_the_user_can_finish_it() {
        // A desktop helper Maple has already written leaves one step Maple
        // cannot take: restarting the session. Offering Set up again would
        // repeat work the user just did and hide the step that matters.
        let mut integration = integration(AgentIntegrationAvailability::SetupRequired, false);
        integration.permissions = Some(AgentIntegrationPermissions::default().with(
            maple_agent::agent::AgentIntegrationPermissionKind::DesktopHelper,
            false,
        ));
        integration.setup_available = true;
        assert!(integration_can_setup(&integration));

        integration.setup_available = false;
        assert!(!integration_can_setup(&integration));
        // The card still reports the integration as unfinished either way.
        assert!(!integration_can_enable(&integration));
        assert!(integration_is_visible(&integration));
    }

    #[test]
    fn a_platform_that_needs_no_grant_is_ready() {
        // Portal-based desktops grant capability per session at first use, so
        // an empty requirement list is a ready integration, not a blocked one.
        let permissions = AgentIntegrationPermissions::none_required();
        assert!(cua_permissions_ready(Some(&permissions)));
        assert!(integration_setup_notice(&permissions).is_none());
    }

    #[test]
    fn the_setup_notice_names_the_requirement_and_its_remedy() {
        let notice = integration_setup_notice(&macos_permissions(false, false))
            .expect("a missing grant produces a notice");
        assert!(notice.resolve().contains("Accessibility"));
        assert!(notice.resolve().contains("System Settings"));

        let notice = integration_setup_notice(&macos_permissions(true, false))
            .expect("a missing grant produces a notice");
        assert!(notice.resolve().contains("Screen Recording"));

        // A compositor helper's remedy changes as the user works through it,
        // so it is written once on the integration itself. A second copy here
        // could only go stale and tell the user to redo a finished step.
        let helper = AgentIntegrationPermissions::default().with(
            maple_agent::agent::AgentIntegrationPermissionKind::DesktopHelper,
            false,
        );
        assert!(integration_setup_notice(&helper).is_none());
    }

    #[test]
    fn shortcut_recorder_uses_the_portable_primary_modifier() {
        let source = if cfg!(target_os = "macos") {
            "cmd-shift-p"
        } else {
            "ctrl-shift-p"
        };
        let keystroke = gpui::Keystroke::parse(source).expect("valid test keystroke");
        assert_eq!(portable_keystroke_token(&keystroke), "secondary-shift-p");
    }

    #[test]
    fn shortcut_recorder_keeps_literal_control_on_macos() {
        let keystroke = gpui::Keystroke::parse("ctrl-r").expect("valid test keystroke");
        let expected = if cfg!(target_os = "macos") {
            "ctrl-r"
        } else {
            "secondary-r"
        };
        assert_eq!(portable_keystroke_token(&keystroke), expected);
    }

    #[test]
    fn standard_text_shortcuts_are_labeled_as_text_fields() {
        assert_eq!(
            shortcut_context_label(Some(crate::keymap::STANDARD_TEXT_CONTEXT)),
            "Text fields"
        );
    }

    #[test]
    fn shortcut_search_matches_each_displayed_field() {
        let row = shortcut_row(
            "menu.next",
            "Next menu item",
            "Menus",
            Some("Menu"),
            "down",
            Some("ctrl-n"),
            false,
        );

        for query in [
            "next menu",
            "menu.next",
            "menus",
            "menu item",
            "down",
            "ctrl-n",
        ] {
            assert!(shortcut_row_matches(&row, query), "query {query:?}");
        }
        assert!(!shortcut_row_matches(&row, "transcript"));
        assert!(shortcut_row_matches(
            &shortcut_row(
                "app.quit",
                "Quit",
                "Application",
                None,
                "secondary-q",
                None,
                false
            ),
            "global"
        ));
    }

    #[test]
    fn shortcut_list_cache_rebuilds_after_snapshot_changes() {
        let snapshot = ShortcutSnapshot {
            generation: 1,
            rows: vec![
                shortcut_row(
                    "chat.search",
                    "Search tasks",
                    "Chat",
                    Some("Chat"),
                    "secondary-k",
                    Some("secondary-k"),
                    false,
                ),
                shortcut_row(
                    "composer.send",
                    "Send message",
                    "Composer",
                    Some("TextInput"),
                    "enter",
                    Some("secondary-enter"),
                    true,
                ),
            ],
            last_error: None,
            compatibility_warning: None,
        };
        assert_eq!(
            ShortcutListCache::rebuild(&snapshot, "secondary-enter"),
            ShortcutListCache {
                visible_indices: vec![1],
                modified_count: 1,
            }
        );

        let replacement = ShortcutSnapshot {
            generation: 2,
            rows: vec![
                shortcut_row(
                    "composer.send",
                    "Send message",
                    "Composer",
                    Some("TextInput"),
                    "enter",
                    Some("enter"),
                    false,
                ),
                shortcut_row(
                    "chat.search",
                    "Search tasks",
                    "Chat",
                    Some("Chat"),
                    "secondary-k",
                    Some("secondary-enter"),
                    true,
                ),
            ],
            last_error: None,
            compatibility_warning: None,
        };
        assert_eq!(
            ShortcutListCache::rebuild(&replacement, "secondary-enter"),
            ShortcutListCache {
                visible_indices: vec![1],
                modified_count: 1,
            }
        );
    }
}
