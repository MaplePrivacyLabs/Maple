//! Settings screen: left navigation with content panes, following Maple's
//! settings layout. Sections: General (defaults), Account, System prompt,
//! Integrations, Keyboard Shortcuts, Usage, About.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::{
    App, Context, Div, Entity, EventEmitter, Render, ScrollAnchor, ScrollHandle, Subscription,
    Window, div, prelude::*, px,
};

use maple_agent::agent::{
    AgentIntegration, AgentIntegrationAvailability, AgentIntegrationBackend,
    AgentIntegrationPermissions, AgentMcpKeyValue, AgentMcpServer, AgentMcpTransport,
};

use crate::ui::icons::icon;
use crate::ui::popup::{Menu, MenuItem, Placement, Popup};
use crate::ui::text_input::TextInput;

use crate::backend::AgentBackend;
use crate::settings::{self, AppSettings, PermissionMode, UsageSummary};
use crate::shortcuts::{
    ShortcutConflict, ShortcutConflictKind, ShortcutContextOverlap, ShortcutOverrides,
    ShortcutSnapshot,
};
use crate::ui::theme;
use crate::ui::widgets;

mod account;
mod api_keys;
mod billing;
mod navigation;
use self::account::AccountState;
use self::api_keys::ApiKeysState;
use self::billing::BillingState;
use self::navigation::{GeneralTarget, SettingsApplicationVimState, SettingsTarget};

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
    ApiKeys,
    Shortcuts,
    Prompt,
    Integrations,
    Usage,
    About,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Account => "Account",
            Self::Billing => "Billing",
            Self::ApiKeys => "API keys",
            Self::Shortcuts => "Keyboard Shortcuts",
            Self::Prompt => "System prompt",
            Self::Integrations => "Integrations",
            Self::Usage => "Usage",
            Self::About => "About",
        }
    }

    const ALL: [Self; 9] = [
        Self::General,
        Self::Account,
        Self::Billing,
        Self::ApiKeys,
        Self::Shortcuts,
        Self::Prompt,
        Self::Integrations,
        Self::Usage,
        Self::About,
    ];
}

/// One multi-value General row that selects from a dropdown instead of
/// toggling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingMenu {
    Permission,
    Appearance,
    ChatFont,
    ChatSize,
    Voice,
    SpeechSpeed,
}

impl SettingMenu {
    /// Stable id fragment for the popup panel and its rows.
    fn id(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Appearance => "appearance",
            Self::ChatFont => "chat-font",
            Self::ChatSize => "chat-size",
            Self::Voice => "voice",
            Self::SpeechSpeed => "speech-speed",
        }
    }
}

pub struct SettingsScreen {
    /// Backend-call bridges retained for thread-affinity; see
    /// [`crate::ui::task::call`].
    bridged_tasks: std::cell::RefCell<Vec<gpui::Task<()>>>,
    backend: Arc<AgentBackend>,
    user_id: String,
    settings: AppSettings,
    /// `settings.theme` parsed once; render only reads the label.
    theme: theme::Preference,
    section: Section,
    /// The multi-value row whose dropdown is open, if any.
    popup: Popup<SettingsScreen, SettingMenu>,
    account: AccountState,
    billing: BillingState,
    api_keys: ApiKeysState,
    usage: Option<UsageSummary>,
    /// Plan usage meter, same source as the sidebar card.
    plan: Option<crate::billing::PlanUsage>,
    /// Account MCP servers; None until loaded.
    mcp_servers: Option<Vec<AgentMcpServer>>,
    mcp_editor: Option<McpEditor>,
    mcp_notice: Option<String>,
    mcp_saving: bool,
    /// Device-local integrations discovered by the runtime; None until loaded.
    integrations: Option<Vec<AgentIntegration>>,
    integration_notice: Option<String>,
    integration_saving: HashSet<String>,
    /// Editor for the opening system prompt text (harness instructions).
    prompt_editor: Entity<TextInput>,
    prompt_notice: Option<String>,
    shortcut_snapshot: ShortcutSnapshot,
    shortcut_search: Entity<TextInput>,
    shortcut_query: String,
    shortcut_list_cache: ShortcutListCache,
    shortcut_notice: Option<String>,
    shortcut_recorder: Option<ShortcutRecorder>,
    shortcut_interceptor: Option<Subscription>,
    shortcut_reset_confirmation: bool,
    application_vim: SettingsApplicationVimState,
    application_focus: gpui::FocusHandle,
    application_focus_pending: bool,
    application_reveal_pending: bool,
    pane_scroll: ScrollHandle,
    application_anchor: ScrollAnchor,
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
    enabled: bool,
    http: bool,
    timeout_seconds: u64,
    name: Entity<TextInput>,
    description: Entity<TextInput>,
    /// Command line (stdio) or URL (HTTP).
    target: Entity<TextInput>,
    /// `KEY=VALUE; KEY2=VALUE2` pairs.
    environment: Entity<TextInput>,
    headers: Entity<TextInput>,
}

/// Emitted with the section to open (composer "Manage integrations" link).
pub struct OpenSettingsSection(pub Section);

impl EventEmitter<SettingsClosed> for SettingsScreen {}
impl EventEmitter<SignOutRequested> for SettingsScreen {}
impl EventEmitter<AccountDeleted> for SettingsScreen {}
impl EventEmitter<ShortcutSettingsRequested> for SettingsScreen {}

impl SettingsScreen {
    pub fn new(
        backend: Arc<AgentBackend>,
        user_id: String,
        settings: AppSettings,
        shortcut_snapshot: ShortcutSnapshot,
        section: Section,
        cx: &mut Context<Self>,
    ) -> Self {
        let prompt_text = settings.effective_harness_instructions();
        let application_vim_enabled = settings.application_vim_enabled;
        let application_focus = cx.focus_handle();
        let prompt_application_focus = application_focus.clone();
        let prompt_editor = cx.new(move |cx| {
            let mut input = TextInput::new("You are …", cx)
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
            TextInput::new("Search by command, context, or shortcut…", cx)
                .with_tab_index(2)
                .application_vim(application_vim_enabled)
                .on_application_escape(move |window, cx| {
                    window.focus(&search_application_focus, cx)
                })
        });
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
        let this = Self {
            bridged_tasks: std::cell::RefCell::new(Vec::new()),
            backend,
            user_id,
            theme: theme::Preference::parse(&settings.theme),
            settings,
            section,
            popup: Popup::new(|this| &mut this.popup, cx),
            account: AccountState::new(application_vim_enabled, application_focus.clone(), cx),
            billing: BillingState::new(),
            api_keys: ApiKeysState::new(application_vim_enabled, application_focus.clone(), cx),
            usage: None,
            plan: None,
            mcp_servers: None,
            mcp_editor: None,
            mcp_notice: None,
            mcp_saving: false,
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
        };
        this.load_account(cx);
        this.load_billing(cx);
        this.load_api_keys(cx);
        this.load_usage(cx);
        this.load_plan(cx);
        this.load_mcp_servers(cx);
        this.load_integrations(cx);
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
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.list_mcp_servers(&user_id).await },
            cx,
            |this, result, cx| {
                match result {
                    Ok(servers) => this.mcp_servers = Some(servers),
                    Err(message) => this.mcp_notice = Some(message),
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    fn load_integrations(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
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
                        this.integration_notice = Some(message);
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
            self.integration_notice =
                Some("This integration is not ready to enable on this device.".to_string());
            cx.notify();
            return;
        }

        let enabled = !currently_enabled;
        let id = id.to_string();
        self.integration_saving.insert(id.clone());
        self.integration_notice = None;
        cx.notify();

        let backend = self.backend.clone();
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
                    Err(message) => this.integration_notice = Some(message),
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
            self.integration_notice =
                Some("This integration does not need setup on this device.".to_string());
            cx.notify();
            return;
        }

        let id = id.to_string();
        let permissions = match self.backend.begin_integration_setup(&id) {
            Ok(permissions) => permissions,
            Err(message) => {
                self.integration_notice = Some(message);
                cx.notify();
                return;
            }
        };
        self.integration_notice = integration_setup_notice(&permissions);
        self.integration_saving.insert(id.clone());
        cx.notify();

        let backend = self.backend.clone();
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
                            Some(notice) => format!("{notice} {message}"),
                            None => message,
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

    fn save_mcp_servers(&mut self, servers: Vec<AgentMcpServer>, cx: &mut Context<Self>) {
        if self.mcp_saving {
            return;
        }
        self.mcp_saving = true;
        self.mcp_notice = None;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.save_mcp_servers(&user_id, servers).await },
            cx,
            |this, result, cx| {
                this.mcp_saving = false;
                match result {
                    Ok(servers) => {
                        this.mcp_servers = Some(servers);
                        this.mcp_editor = None;
                    }
                    Err(message) => this.mcp_notice = Some(message),
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    fn open_mcp_editor(&mut self, existing: Option<AgentMcpServer>, cx: &mut Context<Self>) {
        let application_focus = self.application_focus.clone();
        let application_vim_enabled = self.settings.application_vim_enabled;
        let mut field = |placeholder: &str, value: &str, index: isize| {
            let value = value.to_string();
            let field_application_focus = application_focus.clone();
            cx.new(move |cx| {
                let mut input = TextInput::new(placeholder, cx)
                    .with_tab_index(index)
                    .application_vim(application_vim_enabled)
                    .on_application_escape(move |window, cx| {
                        window.focus(&field_application_focus, cx)
                    });
                if !value.is_empty() {
                    input.set_text(&value, cx);
                }
                input
            })
        };
        let (http, target, environment, headers) = match existing.as_ref().map(|s| &s.transport) {
            Some(AgentMcpTransport::Stdio {
                command,
                environment,
            }) => (
                false,
                command.clone(),
                pairs_to_text(environment),
                String::new(),
            ),
            Some(AgentMcpTransport::StreamableHttp {
                url,
                environment,
                headers,
            }) => (
                true,
                url.clone(),
                pairs_to_text(environment),
                pairs_to_text(headers),
            ),
            None => (false, String::new(), String::new(), String::new()),
        };
        self.mcp_editor = Some(McpEditor {
            original_name: existing.as_ref().map(|s| s.name.clone()),
            enabled: existing.as_ref().map(|s| s.enabled).unwrap_or(true),
            http,
            timeout_seconds: existing.as_ref().map(|s| s.timeout_seconds).unwrap_or(300),
            name: field(
                "My server",
                existing.as_ref().map(|s| s.name.as_str()).unwrap_or(""),
                1,
            ),
            description: field(
                "What this server helps the agent do",
                existing
                    .as_ref()
                    .map(|s| s.description.as_str())
                    .unwrap_or(""),
                2,
            ),
            target: field(
                "npx -y @modelcontextprotocol/server-everything stdio",
                &target,
                3,
            ),
            environment: field("KEY=value; OTHER=value", &environment, 4),
            headers: field("Authorization=Bearer …; X-Api-Key=…", &headers, 5),
        });
        self.mcp_notice = None;
        cx.notify();
    }

    fn submit_mcp_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.mcp_editor.as_ref() else {
            return;
        };
        let name = editor.name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.mcp_notice = Some("Enter a server name".to_string());
            cx.notify();
            return;
        }
        let existing = self.mcp_servers.as_deref().unwrap_or_default();
        if name_collides(existing, editor.original_name.as_deref(), &name) {
            self.mcp_notice = Some(format!("A server named {name:?} already exists"));
            cx.notify();
            return;
        }
        let target = editor.target.read(cx).text().trim().to_string();
        if target.is_empty() {
            self.mcp_notice = Some(if editor.http {
                "Enter the server URL".to_string()
            } else {
                "Enter the server command".to_string()
            });
            cx.notify();
            return;
        }
        let environment = text_to_pairs(&editor.environment.read(cx).text());
        let transport = if editor.http {
            AgentMcpTransport::StreamableHttp {
                url: target,
                environment,
                headers: text_to_pairs(&editor.headers.read(cx).text()),
            }
        } else {
            AgentMcpTransport::Stdio {
                command: target,
                environment,
            }
        };
        let server = AgentMcpServer {
            name,
            description: editor.description.read(cx).text().trim().to_string(),
            enabled: editor.enabled,
            timeout_seconds: editor.timeout_seconds,
            transport,
        };
        let original = editor.original_name.clone();
        let mut servers = self.mcp_servers.clone().unwrap_or_default();
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
        let mut servers = self.mcp_servers.clone().unwrap_or_default();
        if let Some(server) = servers.iter_mut().find(|s| s.name == name) {
            server.enabled = !server.enabled;
        }
        self.save_mcp_servers(servers, cx);
    }

    fn remove_mcp_server(&mut self, name: &str, cx: &mut Context<Self>) {
        let mut servers = self.mcp_servers.clone().unwrap_or_default();
        servers.retain(|s| s.name != name);
        self.save_mcp_servers(servers, cx);
    }

    fn load_usage(&self, cx: &mut Context<Self>) {
        let (spawn_backend, usage_backend) = (self.backend.clone(), self.backend.clone());
        let user_id = self.user_id.clone();
        let task = spawn_backend.spawn(async move {
            let scope = usage_backend.account_scope(&user_id);
            scope.map(|scope| settings::load_usage(&scope))
        });
        cx.spawn(async move |this, cx| {
            let usage = task.await.ok().flatten().unwrap_or_default();
            this.update(cx, |this, cx| {
                this.usage = Some(usage);
                cx.notify();
            })
            .ok();
        })
        .detach();
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
            self.popup.open_at_row(menu, Some(current), cx);
        }
    }

    pub(super) fn close_setting_menu(&mut self, cx: &mut Context<Self>) {
        self.popup.close(cx);
    }

    /// The choices a dropdown offers, in display order. Render and pick
    /// share this order, so an index means the same option in both.
    fn menu_options(&self, menu: SettingMenu) -> Vec<SettingOption> {
        match menu {
            SettingMenu::Permission => [PermissionMode::SmartApprove, PermissionMode::Auto]
                .iter()
                .map(|&mode| SettingOption {
                    label: mode.label().to_string(),
                    current: self.settings.default_permission_mode == mode,
                })
                .collect(),
            SettingMenu::Appearance => [
                theme::Preference::System,
                theme::Preference::Dark,
                theme::Preference::Light,
            ]
            .iter()
            .map(|&preference| SettingOption {
                label: preference.label().to_string(),
                current: self.theme == preference,
            })
            .collect(),
            SettingMenu::ChatFont => crate::ui::typography::ChatFontFamily::ALL
                .iter()
                .map(|&family| SettingOption {
                    label: family.label().to_string(),
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
                .map(|(id, label)| SettingOption {
                    label: label.to_string(),
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
            SettingMenu::Permission => self.settings.default_permission_mode.label().to_string(),
            SettingMenu::Appearance => self.theme.label().to_string(),
            SettingMenu::ChatFont => {
                crate::ui::typography::ChatFontFamily::parse(&self.settings.chat_font_family)
                    .label()
                    .to_string()
            }
            SettingMenu::ChatSize => format!(
                "{} px",
                crate::ui::typography::clamp_chat_font_size(self.settings.chat_font_size)
            ),
            SettingMenu::Voice => settings::tts_voice_label(&self.settings.tts_voice).to_string(),
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

    fn chat_appearance_preview(&self) -> Div {
        let family = crate::ui::typography::ChatFontFamily::parse(&self.settings.chat_font_family);
        let size = crate::ui::typography::clamp_chat_font_size(self.settings.chat_font_size);
        let speaker = |name: &'static str| {
            div()
                .text_xs()
                .font_family(crate::assets::FONT_BODY)
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(gpui::rgb(theme::text_muted()))
                .child(name)
        };
        // The settings pane and user bubbles both sit on elevated fills in
        // dark mode. Recess a real chat well (`bg_app`) so assistant text
        // and the user bubble separate from the card, matching the transcript.
        widgets::card_row()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .font_family(crate::assets::FONT_BODY)
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(format!("Live preview · {} · {size} px", family.label())),
            )
            .child(
                div()
                    .w_full()
                    .rounded(theme::RADIUS_MD)
                    .border_1()
                    .border_color(gpui::rgb(theme::border()))
                    .bg(gpui::rgb(if theme::is_light() {
                        theme::bg_sidebar()
                    } else {
                        theme::bg_app()
                    }))
                    .px_3()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        crate::ui::typography::chat_reading(div())
                            .max_w(gpui::relative(0.82))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(speaker("Assistant"))
                            .child(
                                "Clear, comfortable text makes longer conversations easier to follow.",
                            )
                            .child(
                                div()
                                    .font_weight(crate::ui::typography::emphasis_weight())
                                    .child("Bold should look crisp, not blurry."),
                            ),
                    )
                    .child(
                        div().w_full().flex().justify_end().child(
                            crate::ui::typography::chat_reading(div())
                                .px_3()
                                .py_2()
                                .rounded(theme::RADIUS_MD)
                                .bg(gpui::rgb(theme::bg_user_bubble()))
                                .border_1()
                                .border_color(gpui::rgb(theme::user_bubble_border()))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .text_color(gpui::rgb(theme::text_primary()))
                                .child(speaker("You"))
                                .child("This size feels just right."),
                        ),
                    ),
            )
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
            SettingMenu::Permission => {
                let Some(mode) = [PermissionMode::SmartApprove, PermissionMode::Auto].get(index)
                else {
                    return;
                };
                let mode = *mode;
                self.edit_setting(move |settings| settings.default_permission_mode = mode, cx);
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
                "Desktop notifications on",
                &format!("You will see alerts like this at {enabled_at}."),
                &[],
            );
        }
    }

    /// Build one multi-value row: the saved value sits in a pebble button
    /// that opens a dropdown of every choice. Picking one applies and
    /// persists it; clicking away dismisses.
    fn setting_menu_row(
        &self,
        title: &str,
        description: &str,
        menu: SettingMenu,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let dropdown = self.popup.is_open(&menu).then(|| {
            let options = self.menu_options(menu);
            let dropdown = Menu::new(
                gpui::SharedString::from(format!("setting-menu-{}", menu.id())),
                px(260.),
            )
            .label(title.to_string())
            .max_height(px(320.))
            .application_vim(self.settings.application_vim_enabled)
            .items(options.into_iter().enumerate().map(|(index, option)| {
                let item = MenuItem::new(
                    gpui::SharedString::from(format!("setting-menu-{}-{index}", menu.id())),
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
            self.popup.render(dropdown, Placement::BelowEnd, cx)
        });
        let slug = title.to_lowercase().replace(' ', "-");
        let button =
            widgets::secondary_button(gpui::SharedString::from(format!("setting-value-{slug}")))
                .debug_selector(move || format!("setting-value-{slug}"))
                .py_1p5()
                .gap_1()
                .child(self.menu_value_label(menu))
                .child(icon("chevron-down", px(12.), theme::text_muted()));
        widgets::card_row()
            .id(setting_row_id(title))
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

    /// Persist the editor text as the harness instructions and hand it to
    /// the running backend. Text equal to the default is saved as empty so
    /// a future default change still applies.
    fn save_prompt(&mut self, cx: &mut Context<Self>) {
        let text = self.prompt_editor.read(cx).text().trim().to_string();
        self.settings.harness_instructions = if text == settings::DEFAULT_HARNESS_INSTRUCTIONS {
            String::new()
        } else {
            text
        };
        let instructions = self.settings.harness_instructions.clone();
        settings::update_settings_in_background(move |s| s.harness_instructions = instructions);
        self.backend
            .set_harness_instructions(self.settings.effective_harness_instructions());
        self.prompt_notice = Some("Saved. New tasks use this prompt.".to_string());
        cx.notify();
    }

    fn reset_prompt(&mut self, cx: &mut Context<Self>) {
        self.prompt_editor.update(cx, |input, cx| {
            input.set_text(settings::DEFAULT_HARNESS_INSTRUCTIONS, cx);
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
            Ok(()) => "Shortcut settings applied.".to_string(),
            Err(message) => format!("Shortcut change failed: {message}"),
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
                self.shortcut_notice = Some("Shortcut recording cancelled.".to_string());
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
                    self.shortcut_notice =
                        Some("A shortcut can contain at most four strokes.".to_string());
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
            self.shortcut_notice = Some("Press at least one shortcut key.".to_string());
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
                self.shortcut_notice = Some(format!("Cannot use that shortcut: {message}"));
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
        if section != Section::Shortcuts {
            self.stop_shortcut_recording();
        }
        // A dropdown belongs to the pane that opened it.
        self.close_setting_menu(cx);
        self.section = section;
        if self.settings.application_vim_enabled {
            self.application_vim.section = section;
            self.reconcile_application_vim_target();
        }
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.stop_shortcut_recording();
        cx.emit(SettingsClosed(self.settings.clone()));
    }
}

fn merge_shortcut_overrides(settings: &mut AppSettings, shortcut_overrides: ShortcutOverrides) {
    settings.shortcut_overrides = shortcut_overrides;
}

impl Render for SettingsScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                            .child("← Back"),
                    )
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child("Settings"),
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
                            .child("Sign out"),
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
                            .child(self.render_pane(cx))
                            .child(crate::ui::scrollbar::scrollbar(
                                "settings-scrollbar",
                                self.pane_scroll.clone(),
                            )),
                    ),
            )
    }
}

impl SettingsScreen {
    fn render_nav(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        div()
            .id("settings-nav")
            .role(gpui::Role::TabList)
            .aria_label("Settings sections")
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
            .children(Section::ALL.iter().map(|section| {
                let selected = self.section == *section;
                let application_selected = self.application_vim_selects_section(*section);
                div()
                    .id(gpui::SharedString::from(format!(
                        "settings-nav-{}",
                        section.label()
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

    fn render_pane(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
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
            Section::General => {
                pane = pane
                    .child(section_title("Defaults"))
                    .child({
                        let mode = self.settings.default_permission_mode;
                        self.application_target(
                            || SettingsTarget::General(GeneralTarget::Permission),
                            self.setting_menu_row(
                                "Default permission mode",
                                mode.note(),
                                SettingMenu::Permission,
                                cx,
                            ),
                        )
                    })
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::Web),
                        toggle_row(
                            "New tasks can use the web",
                            "Offers web_search and open_url to the model. Each task can \
                             switch web access on or off from its composer.",
                            self.settings.default_web_enabled,
                            cx.listener(|this, _event, _window, cx| {
                                this.toggle_web_default(cx);
                            }),
                        ),
                    ))
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::Appearance),
                        self.setting_menu_row(
                            "Appearance",
                            "Follow the system theme, or force dark or light.",
                            SettingMenu::Appearance,
                            cx,
                        ),
                    ))
                    .child(section_title("Chat appearance"))
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::ChatFont),
                        self.setting_menu_row(
                            "Chat font",
                            crate::ui::typography::ChatFontFamily::parse(
                                &self.settings.chat_font_family,
                            )
                            .note(),
                            SettingMenu::ChatFont,
                            cx,
                        ),
                    ))
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::ChatSize),
                        self.setting_menu_row(
                            "Text size",
                            "Size of conversation and composer text. Sidebar and buttons stay Manrope.",
                            SettingMenu::ChatSize,
                            cx,
                        ),
                    ))
                    .child(self.chat_appearance_preview())
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::ToolDetails),
                        toggle_row(
                            "Show tool call details",
                            "Tool cards include their input and output payloads.",
                            self.settings.tool_details,
                            cx.listener(|this, _event, _window, cx| {
                                this.toggle_tool_details(cx);
                            }),
                        ),
                    ))
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::Notifications),
                        toggle_row(
                            "Desktop notifications",
                            "Notify when a task finishes or needs your input while \
                             the window is not focused.",
                            self.settings.desktop_notifications,
                            cx.listener(|this, _event, _window, cx| {
                                this.toggle_desktop_notifications(cx);
                            }),
                        ),
                    ))
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::ReduceMotion),
                        toggle_row(
                            "Reduce motion",
                            "Hold spinners and pulsing dots still and skip reveal \
                             animations.",
                            self.settings.reduce_motion,
                            cx.listener(|this, _event, _window, cx| {
                                this.toggle_reduce_motion(cx);
                            }),
                        ),
                    ))
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::ToolSummaries),
                        toggle_row(
                            "Summarize tool calls",
                            "Completed tool calls with long output get a one-line \
                             summary from the title model on their cards.",
                            self.settings.tool_summaries,
                            cx.listener(|this, _event, _window, cx| {
                                this.toggle_tool_summaries(cx);
                            }),
                        ),
                    ))
                    .child(section_title("Editing"))
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::ComposerVim),
                        toggle_row(
                            "Vim mode in composer",
                            "Use Normal, Insert, and Visual editing modes in the main chat composer. Other text fields stay unchanged.",
                            self.settings.composer_vim_enabled,
                            cx.listener(|this, _event, _window, cx| {
                                this.toggle_composer_vim(cx);
                            }),
                        ),
                    ))
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::ApplicationVim),
                        toggle_row(
                            "Vim navigation across the app",
                            "Navigate stable sidebar, transcript, dialog, and Settings targets. Ordinary text fields still edit normally.",
                            self.settings.application_vim_enabled,
                            cx.listener(|this, _event, _window, cx| {
                                this.toggle_application_vim(cx);
                            }),
                        ),
                    ))
                    .child(section_title("Voice"))
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::Voice),
                        self.setting_menu_row(
                            "Speech voice",
                            "The voice that reads messages aloud.",
                            SettingMenu::Voice,
                            cx,
                        ),
                    ))
                    .child(self.application_target(
                        || SettingsTarget::General(GeneralTarget::SpeechSpeed),
                        self.setting_menu_row(
                            "Speech speed",
                            "How fast messages are read aloud.",
                            SettingMenu::SpeechSpeed,
                            cx,
                        ),
                    ));
            }
            Section::Account => {
                pane = pane.child(self.render_account_pane(cx));
            }
            Section::Billing => {
                pane = pane.child(self.render_billing_pane(cx));
            }
            Section::ApiKeys => {
                pane = pane.child(self.render_api_keys_pane(cx));
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
            Section::Usage => {
                pane = pane.child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .child(section_title("Plan"))
                        .when_some(self.plan.as_ref(), |row, plan| {
                            row.child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(gpui::rgb(theme::accent()))
                                    .child(plan.plan_label.clone()),
                            )
                        }),
                );
                pane = pane.child(match &self.plan {
                    Some(plan) => plan_card(plan),
                    None => div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child("Plan usage unavailable"),
                });
                pane = pane.child(section_title("Usage"));
                if let Some(usage) = &self.usage {
                    pane = pane
                        .child(
                            div()
                                .flex()
                                .gap_6()
                                .child(stat("Turns", usage.totals.turns.to_string()))
                                .child(stat("Sessions", usage.totals.sessions.to_string()))
                                .child(stat(
                                    "Total tokens",
                                    format_tokens(usage.totals.total_tokens),
                                ))
                                .child(stat("Est. cost", format!("${:.2}", usage.totals.cost))),
                        )
                        .child(usage_table("By model", &usage.by_model))
                        .child(usage_table("Recent sessions", &usage.by_session));
                } else {
                    pane = pane.child(
                        div()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child("Loading usage…"),
                    );
                }
            }
            Section::About => {
                pane = pane
                    .child(section_title("About"))
                    .child(info_row(
                        "Version",
                        format!("v{}", crate::ui::titlebar::TitleBar::version()),
                    ))
                    .child(info_row(
                        "Update",
                        match crate::update::available() {
                            Some(info) => format!("v{} is available at {}", info.version, info.url),
                            None if crate::update::enabled() => "Up to date".to_string(),
                            None => "Update check disabled".to_string(),
                        },
                    ))
                    .child(info_row("Backend", self.backend.api_url().to_string()))
                    .child(info_row(
                        "Config directory",
                        crate::backend::app_config_root()
                            .to_string_lossy()
                            .to_string(),
                    ));
            }
        }
        pane
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
                    .child(section_title("Keyboard Shortcuts"))
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
                                    .child("Reset all"),
                            )
                        },
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(
                        "Customize the shortcuts Maple already ships. This page does not add commands or change what an action can do.",
                    ),
            )
            .child(widgets::input_frame().text_sm().child(self.shortcut_search.clone()))
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(format!(
                        "{} bindings · {modified_count} modified",
                        self.shortcut_snapshot.rows.len()
                    )),
            );

        if let Some(error) = &self.shortcut_snapshot.last_error {
            pane = pane.child(
                widgets::banner(theme::status_warning()).child(format!(
                    "Saved shortcut overrides were not applied; Maple kept the complete default map. {error}"
                )),
            );
        }
        if let Some(warning) = &self.shortcut_snapshot.compatibility_warning {
            pane = pane.child(widgets::banner(theme::status_warning()).child(warning.clone()));
        }
        if let Some(notice) = &self.shortcut_notice {
            pane = pane.child(widgets::banner(theme::status_warning()).child(notice.clone()));
        }
        if self.shortcut_reset_confirmation {
            pane = pane.child(
                widgets::banner(theme::status_warning())
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().child(
                        if self.shortcut_snapshot.last_error.is_some() && modified_count == 0 {
                            "Clear the invalid saved shortcut overrides?".to_string()
                        } else {
                            format!(
                                "Reset all {modified_count} shortcut change{}?",
                                if modified_count == 1 { "" } else { "s" }
                            )
                        },
                    ))
                    .child(
                        widgets::primary_button("shortcuts-reset-all-confirm")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.request_shortcut_change(ShortcutSettingsChange::ResetAll, cx);
                            }))
                            .child("Reset"),
                    )
                    .child(
                        widgets::ghost_button("shortcuts-reset-all-cancel")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.shortcut_reset_confirmation = false;
                                cx.notify();
                            }))
                            .child("Cancel"),
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
                    .child("No shortcuts match this search."),
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
            .map(|row| row.label.as_str())
            .unwrap_or(recorder.slot_id.as_str())
            .to_string();
        let sequence = if recorder.strokes.is_empty() {
            "Waiting for keys…".to_string()
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
                    .child(format!("Recording: {label}")),
            )
            .child(shortcut_keycap(sequence, true))
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(if recorder.reviewing_conflicts {
                        "Review the conflicts below before saving."
                    } else {
                        "Press up to four keys. Enter saves, Backspace removes the latest stroke, and Escape cancels."
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
                        .map(|row| row.label.as_str())
                        .unwrap_or(conflict.other_slot_id.as_str());
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::status_warning()))
                        .child(format!(
                            "{} with {} ({})",
                            shortcut_conflict_label(conflict),
                            other,
                            conflict.other_slot_id
                        ))
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
                                .child("Replace conflicts"),
                        )
                        .when(can_keep_both, |actions| {
                            actions.child(
                                widgets::ghost_button("shortcut-conflict-keep")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.save_recorded_shortcut(false, cx);
                                    }))
                                    .child("Keep both"),
                            )
                        })
                        .child(
                            widgets::ghost_button("shortcut-conflict-cancel")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.stop_shortcut_recording();
                                    cx.notify();
                                }))
                                .child("Cancel"),
                        ),
                );
        } else {
            card = card.child(
                widgets::ghost_button("shortcut-record-cancel")
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.stop_shortcut_recording();
                        cx.notify();
                    }))
                    .child("Cancel"),
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
            .unwrap_or_else(|| "Unbound".to_string());
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
                                    .child(row.label.clone()),
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
                                        .child("Modified"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(format!(
                                "{} · {}",
                                row.category,
                                shortcut_context_label(row.context.as_deref()),
                            )),
                    )
                    .when(!row.conflicts.is_empty(), |column| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(theme::status_warning()))
                                .child(format!(
                                    "{} shortcut conflict{}",
                                    row.conflicts.len(),
                                    if row.conflicts.len() == 1 { "" } else { "s" }
                                )),
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
                            .child(format!("Default: {}", row.default_sequence)),
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
                        .child("Record"),
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
                            .child("Disable"),
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
                            .child("Reset"),
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
            .child(section_title("System prompt"))
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(
                        "Opens every task's system prompt: who the agent is and how it \
                         behaves. Maple appends its tool and runtime guidance after this \
                         text. Changes apply to tasks started after you save.",
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
                            "Save",
                            true,
                            cx.listener(|this, _event, _window, cx| this.save_prompt(cx)),
                        ),
                    ))
                    .child(self.application_target(
                        || SettingsTarget::PromptReset,
                        pill_button(
                            "prompt-reset".to_string(),
                            "Reset to default",
                            false,
                            cx.listener(|this, _event, _window, cx| this.reset_prompt(cx)),
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
            .child(section_title("Integrations"))
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child("Manage integrations here. Enable coding agents to make them available in the composer, then select them for each task."),
            );

        if let Some(notice) = &self.integration_notice {
            pane = pane.child(widgets::notice(
                "integration-notice-close",
                gpui::SharedString::from(notice.clone()),
                cx.listener(|this, _event, _window, cx| {
                    this.integration_notice = None;
                    cx.notify();
                }),
            ));
        }

        match &self.integrations {
            None => {
                pane = pane.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child("Detecting integrations…"),
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
                            .child("No supported integrations detected on this device."),
                    );
                }
            }
        }

        pane = pane.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .mt_2()
                .pt_5()
                .border_t_1()
                .border_color(gpui::rgb(theme::border_subtle()))
                .child(subsection_title("Custom MCP servers"))
                .child(
                    self.application_target(
                        || SettingsTarget::McpAdd,
                        widgets::primary_button("mcp-add")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.open_mcp_editor(None, cx);
                            }))
                            .child(icon("plus", widgets::ROW_ICON, theme::on_accent()))
                            .child("Add server"),
                    ),
                ),
        );
        pane = pane.child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_muted()))
                .child("Connect additional tools over MCP."),
        );
        if let Some(notice) = &self.mcp_notice {
            pane = pane.child(widgets::banner(theme::status_warning()).child(notice.clone()));
        }
        if let Some(editor) = &self.mcp_editor {
            pane = pane.child(self.render_mcp_editor(editor, cx));
        }
        match &self.mcp_servers {
            None => {
                pane = pane.child(
                    div()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child("Loading custom MCP servers…"),
                );
            }
            Some(servers) if servers.is_empty() => {
                pane = pane.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child("No custom MCP servers configured."),
                );
            }
            Some(servers) => {
                pane = pane.children(servers.iter().map(|server| {
                    let name = server.name.clone();
                    let summary = match &server.transport {
                        AgentMcpTransport::Stdio { command, .. } => format!("STDIO · {command}"),
                        AgentMcpTransport::StreamableHttp { url, .. } => format!("HTTP · {url}"),
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
                                            .child(server.description.clone()),
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
                                "Enabled"
                            } else {
                                "Disabled"
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
                                "Edit",
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
                                "Remove",
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
        }
        pane
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
        let display_name = integration.name.clone();
        let description = integration.description.clone();
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
                    .child(integration_status_badge(availability, availability_color)),
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
            let mut metadata = div().flex().flex_wrap().items_center().gap_2();
            metadata = metadata.child(integration_metadata_badge(cua_backend_label(integration)));
            for permission in integration
                .permissions
                .iter()
                .flat_map(|permissions| permissions.required.iter())
            {
                metadata = metadata.child(cua_permission_badge(
                    permission.kind.label(),
                    permission.granted,
                ));
            }
            details = details.child(metadata);
        } else if let Some(version) = integration.version.as_deref() {
            details = details.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(format!("Version {version}")),
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
                    .child(detail.clone()),
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
                        if matches!(
                            &integration.backend,
                            Some(AgentIntegrationBackend::External)
                        ) || integration.standalone_version.is_some()
                        {
                            "Set up built-in"
                        } else {
                            "Set up"
                        },
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
                        if enabled { "Enabled" } else { "Enable" },
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
        let field = |label: &'static str, hint: &'static str, input: Entity<TextInput>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(gpui::rgb(theme::text_secondary()))
                        .child(label),
                )
                .child(widgets::input_frame().text_sm().child(input))
                .when(!hint.is_empty(), |col| {
                    col.child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(hint),
                    )
                })
        };
        let transport_segment = |id: &'static str, label: &'static str, active: bool| {
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
                        "Edit server"
                    } else {
                        "New server"
                    }),
            )
            .child(field("Name", "", editor.name.clone()))
            .child(field("Description", "", editor.description.clone()))
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
                            .child("Transport"),
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
                                    "Standard IO (STDIO)",
                                    !editor.http,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        if let Some(editor) = this.mcp_editor.as_mut() {
                                            editor.http = false;
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                transport_segment(
                                    "mcp-transport-http",
                                    "Streamable HTTP",
                                    editor.http,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        if let Some(editor) = this.mcp_editor.as_mut() {
                                            editor.http = true;
                                        }
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            .child(if editor.http {
                field("URL", "", editor.target.clone())
            } else {
                field("Command", "", editor.target.clone())
            })
            .child(field(
                "Environment",
                "KEY=value pairs separated by semicolons.",
                editor.environment.clone(),
            ))
            .when(editor.http, |col| {
                col.child(field(
                    "Headers",
                    "Name=value pairs separated by semicolons.",
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
                            .child("Save"),
                    )
                    .child(
                        widgets::ghost_button("mcp-cancel")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.mcp_editor = None;
                                this.mcp_notice = None;
                                cx.notify();
                            }))
                            .child("Cancel"),
                    ),
            )
    }
}

/// Plan usage meter: plan pill, percent used, reset date, progress bar.
/// Mirrors the sidebar card at a larger size.
fn plan_card(plan: &crate::billing::PlanUsage) -> Div {
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
                        .child(format!("{}% used", plan.percent_used)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .whitespace_nowrap()
                        .child(format!("· Resets {}", plan.resets_label)),
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

    match integration.backend {
        Some(AgentIntegrationBackend::Embedded) => {
            cua_permissions_ready(integration.permissions.as_ref())
        }
        Some(AgentIntegrationBackend::External) => integration.standalone_version.is_some(),
        None => {
            cua_permissions_ready(integration.permissions.as_ref())
                || integration.standalone_version.is_some()
        }
    }
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
        && (!cua_permissions_ready(integration.permissions.as_ref())
            || matches!(
                &integration.backend,
                Some(AgentIntegrationBackend::External)
            ))
}

fn integration_availability(integration: &AgentIntegration) -> (&'static str, u32) {
    if integration.enabled_for_new_tasks
        && is_cua_driver(&integration.id)
        && matches!(
            &integration.backend,
            Some(AgentIntegrationBackend::Embedded)
        )
        && !cua_permissions_ready(integration.permissions.as_ref())
    {
        return ("Needs setup", theme::status_warning());
    }

    match &integration.availability {
        AgentIntegrationAvailability::NotDetected => ("Not detected", theme::text_muted()),
        AgentIntegrationAvailability::Available if is_cua_driver(&integration.id) => {
            if cua_permissions_ready(integration.permissions.as_ref())
                || integration.standalone_version.is_some()
            {
                ("Ready", theme::status_success())
            } else {
                ("Available", theme::status_success())
            }
        }
        AgentIntegrationAvailability::Available => ("Detected", theme::status_success()),
        AgentIntegrationAvailability::SetupRequired => ("Setup required", theme::status_warning()),
    }
}

fn cua_permissions_ready(permissions: Option<&AgentIntegrationPermissions>) -> bool {
    permissions.is_some_and(AgentIntegrationPermissions::ready)
}

fn integration_setup_notice(permissions: &AgentIntegrationPermissions) -> Option<String> {
    let missing = permissions.first_missing()?;
    let guidance = missing.guidance()?;
    Some(format!("{}: {guidance}", missing.label()))
}

fn cua_backend_label(integration: &AgentIntegration) -> &'static str {
    match integration.backend {
        Some(AgentIntegrationBackend::External) => "Standalone driver",
        None if integration.standalone_version.is_some()
            && !cua_permissions_ready(integration.permissions.as_ref()) =>
        {
            "Standalone detected"
        }
        Some(AgentIntegrationBackend::Embedded) | None => "Built into Maple",
    }
}

fn integration_status_badge(label: &'static str, color: u32) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .bg(gpui::rgb(theme::bg_sidebar_pill()))
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(gpui::rgb(color))
        .child(label)
}

fn integration_metadata_badge(label: &'static str) -> Div {
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

fn cua_permission_badge(label: &'static str, granted: bool) -> Div {
    let (status, color) = if granted {
        ("on", theme::status_success())
    } else {
        ("needed", theme::status_warning())
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
        .child(format!("{label} {status}"))
}

fn is_cua_driver(id: &str) -> bool {
    id == "cua-driver"
}

/// Small on/off pill used in list rows.
fn pill_button(
    id: String,
    label: &'static str,
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

/// Whether saving a server as `name` would clash with another server.
/// Servers are matched by name, so a rename onto an existing name would
/// have added a second entry instead of replacing the original.
fn name_collides(servers: &[AgentMcpServer], original: Option<&str>, name: &str) -> bool {
    servers
        .iter()
        .any(|server| server.name == name && Some(server.name.as_str()) != original)
}

/// `KEY=value; KEY2=value` for the editor field.
fn pairs_to_text(pairs: &[AgentMcpKeyValue]) -> String {
    pairs
        .iter()
        .map(|pair| format!("{}={}", pair.key, pair.value))
        .collect::<Vec<_>>()
        .join("; ")
}

fn text_to_pairs(text: &str) -> Vec<AgentMcpKeyValue> {
    text.split(';')
        .filter_map(|entry| {
            let entry = entry.trim();
            let (key, value) = entry.split_once('=')?;
            let key = key.trim();
            (!key.is_empty()).then(|| AgentMcpKeyValue {
                key: key.to_string(),
                value: value.trim().to_string(),
            })
        })
        .collect()
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

fn shortcut_context_label(context: Option<&str>) -> &str {
    match context {
        None => "Global",
        Some("Chat") => "Chat",
        Some("Transcript") => "Transcript",
        Some(crate::ui::popup::MENU_CONTEXT) => "Menus",
        Some("TextInput") | Some(crate::keymap::STANDARD_TEXT_CONTEXT) => "Text fields",
        Some(crate::ui::text_input::vim_actions::NORMAL_CONTEXT) => "Composer — Normal",
        Some(crate::ui::text_input::vim_actions::VISUAL_CONTEXT) => "Composer — Visual",
        Some(crate::ui::text_input::vim_actions::INSERT_CONTEXT) => "Composer — Insert",
        Some(context) => context,
    }
}

fn shortcut_row_matches(row: &crate::shortcuts::ShortcutRow, query: &str) -> bool {
    query.is_empty()
        || row.label.to_lowercase().contains(query)
        || row.slot_id.to_lowercase().contains(query)
        || row.category.to_lowercase().contains(query)
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
        ShortcutConflictKind::Exact => "Exact collision",
        ShortcutConflictKind::Prefix => "Prefix collision",
    };
    let overlap = match conflict.overlap {
        ShortcutContextOverlap::Equivalent => "same context",
        ShortcutContextOverlap::Scoped => "overlapping scoped context",
        ShortcutContextOverlap::Possible => "possibly overlapping context",
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
fn setting_copy(title: &str, description: &str) -> Div {
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

fn setting_row_id(title: &str) -> gpui::SharedString {
    gpui::SharedString::from(format!(
        "setting-{}",
        title.to_lowercase().replace(' ', "-")
    ))
}

/// One dropdown choice as data: the label shown in the menu and whether
/// it is the saved value.
struct SettingOption {
    label: String,
    current: bool,
}

/// An on/off setting with a switch. The whole row is the click target.
fn toggle_row(
    title: &str,
    description: &str,
    on: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<Div> {
    widgets::card_row()
        .id(setting_row_id(title))
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
                title.to_lowercase().replace(' ', "-")
            )),
            on,
        ))
}

fn info_row(label: &str, value: String) -> Div {
    widgets::card_row()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(label.to_string()),
        )
        .child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_primary()))
                .child(value),
        )
}

fn stat(label: &str, value: String) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(label.to_string()),
        )
        .child(
            div()
                .text_xl()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(gpui::rgb(theme::text_primary()))
                .child(value),
        )
}

fn usage_table(title: &str, rows: &[crate::settings::UsageRow]) -> Div {
    let mut table = div().flex().flex_col().gap_2().child(
        div()
            .text_sm()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(gpui::rgb(theme::text_primary()))
            .child(title.to_string()),
    );
    for row in rows.iter().take(10) {
        table = table.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .px_3()
                .py_2()
                .rounded(theme::RADIUS_SM)
                .bg(gpui::rgb(theme::bg_elevated()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_primary()))
                        .line_clamp(1)
                        .child(row.label.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_secondary()))
                        .child(format!("{} turns", row.turns)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_secondary()))
                        .child(format_tokens(row.total_tokens)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(format!("${:.2}", row.cost)),
                ),
        );
    }
    table
}

fn format_tokens(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn server(name: &str) -> AgentMcpServer {
        AgentMcpServer {
            name: name.to_string(),
            description: String::new(),
            enabled: true,
            timeout_seconds: 300,
            transport: AgentMcpTransport::Stdio {
                command: "cmd".to_string(),
                environment: Vec::new(),
            },
        }
    }

    fn integration(
        availability: AgentIntegrationAvailability,
        enabled_for_new_tasks: bool,
    ) -> AgentIntegration {
        let ready = matches!(availability, AgentIntegrationAvailability::Available);

        AgentIntegration {
            id: "cua-driver".to_string(),
            name: "Computer use (CUA)".to_string(),
            description: "Computer use".to_string(),
            availability,
            version: Some("1.0.0".to_string()),
            enabled_for_new_tasks,
            detail: None,
            permissions: Some(macos_permissions(ready, ready)),
            setup_available: true,
            standalone_version: None,
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

        let mut external = integration(AgentIntegrationAvailability::Available, true);
        external.backend = Some(AgentIntegrationBackend::External);
        external.permissions = Some(macos_permissions(false, false));
        external.standalone_version = Some("0.23.2".to_string());
        assert!(integration_can_toggle(&external));
        assert!(integration_can_setup(&external));

        // A separately installed driver must not make an embedded selection
        // enableable after Maple's own grants have been revoked. The setup
        // action is the only route back to a ready embedded backend.
        let mut revoked_embedded = external.clone();
        revoked_embedded.backend = Some(AgentIntegrationBackend::Embedded);
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
        assert_eq!(integration_setup_notice(&permissions), None);
    }

    #[test]
    fn the_setup_notice_names_the_requirement_and_its_remedy() {
        let notice = integration_setup_notice(&macos_permissions(false, false))
            .expect("a missing grant produces a notice");
        assert!(notice.starts_with("Accessibility: "));
        assert!(notice.contains("System Settings"));

        let notice = integration_setup_notice(&macos_permissions(true, false))
            .expect("a missing grant produces a notice");
        assert!(notice.starts_with("Screen Recording: "));

        // A compositor helper's remedy changes as the user works through it,
        // so it is written once on the integration itself. A second copy here
        // could only go stale and tell the user to redo a finished step.
        let helper = AgentIntegrationPermissions::default().with(
            maple_agent::agent::AgentIntegrationPermissionKind::DesktopHelper,
            false,
        );
        assert_eq!(integration_setup_notice(&helper), None);
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
