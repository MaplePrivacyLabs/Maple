//! The Apps section of Integrations: the bundled catalog, by search and
//! category, one click from connecting.

use crate::ui::host_errors::failure_text;
use crate::ui::i18n::{UiText, tr};
use crate::ui::task::Failure;
use gpui::{App, Context, Div, Entity, Focusable, Window, div, prelude::*, px};
use maple_agent::agent::{
    AgentMcpServer, McpAppConnection, McpCatalogAddress, McpCatalogAuth, McpCatalogCategory,
    McpCatalogEntry, McpSignIn, mcp_catalog,
};
use maple_agent::store::records::{McpDefinition, McpTarget};

use super::{
    Section, SettingsScreen, SettingsTarget, TextInput, page_header, spinner, subsection_title,
    theme, widgets,
};

/// The pop-up that asks where an app is, for one only the person can find.
pub(super) struct McpAddressPrompt {
    entry: &'static McpCatalogEntry,
    pub input: Entity<TextInput>,
    error: Option<UiText>,
    focus_pending: bool,
}

/// Whether a saved server is this entry: by name, or by address under another.
fn is_connected(entry: &McpCatalogEntry, servers: &[McpDefinition]) -> bool {
    let key = AgentMcpServer::key_for_name(&entry.name);
    let address = entry
        .remote()
        .map(|remote| remote.url.trim_end_matches('/'));
    servers.iter().any(|server| {
        server.key == key
            || matches!(
                &server.target,
                McpTarget::StreamableHttp { url } if Some(url.trim_end_matches('/')) == address
            )
    })
}

/// Where an app's logo lives among the embedded assets.
fn logo_path(entry: &McpCatalogEntry) -> String {
    format!("icons/apps/{}.png", entry.id)
}

fn category_label(category: McpCatalogCategory) -> gpui::SharedString {
    tr(match category {
        McpCatalogCategory::Productivity => "settings-catalog-productivity",
        McpCatalogCategory::Teams => "settings-catalog-teams",
        McpCatalogCategory::Business => "settings-catalog-business",
        McpCatalogCategory::Developer => "settings-catalog-developer",
    })
}
fn summary(entry: &McpCatalogEntry) -> gpui::SharedString {
    match entry.id.as_str() {
        "notion" => tr("settings-catalog-summary-notion"),
        "todoist" => tr("settings-catalog-summary-todoist"),
        "dropbox" => tr("settings-catalog-summary-dropbox"),
        "airtable" => tr("settings-catalog-summary-airtable"),
        "evernote" => tr("settings-catalog-summary-evernote"),
        "miro" => tr("settings-catalog-summary-miro"),
        "zapier" => tr("settings-catalog-summary-zapier"),
        "vana" => tr("settings-catalog-summary-vana"),
        "home-assistant" => tr("settings-catalog-summary-home-assistant"),
        "atlassian" => tr("settings-catalog-summary-atlassian"),
        "linear" => tr("settings-catalog-summary-linear"),
        "monday" => tr("settings-catalog-summary-monday"),
        "clickup" => tr("settings-catalog-summary-clickup"),
        "stripe" => tr("settings-catalog-summary-stripe"),
        "paypal" => tr("settings-catalog-summary-paypal"),
        "square" => tr("settings-catalog-summary-square"),
        "intercom" => tr("settings-catalog-summary-intercom"),
        "wix" => tr("settings-catalog-summary-wix"),
        "wordpress" => tr("settings-catalog-summary-wordpress"),
        "gitlab" => tr("settings-catalog-summary-gitlab"),
        "vercel" => tr("settings-catalog-summary-vercel"),
        "cloudflare" => tr("settings-catalog-summary-cloudflare"),
        "supabase" => tr("settings-catalog-summary-supabase"),
        "sentry" => tr("settings-catalog-summary-sentry"),
        "playwright" => tr("settings-catalog-summary-playwright"),
        "xcode" => tr("settings-catalog-summary-xcode"),
        _ => entry.summary.clone().into(),
    }
}

/// What a saved server says about itself. One connected from the catalog holds
/// the catalog's words, which read in the person's language; anything the person
/// wrote stays as they wrote it.
pub(crate) fn saved_description(name: &str, description: &str) -> gpui::SharedString {
    mcp_catalog()
        .iter()
        .find(|entry| entry.name == name && entry.summary == description)
        .map_or_else(|| description.to_string().into(), summary)
}

/// What the address field is called, in the person's language.
fn address_label(entry: &McpCatalogEntry, address: &McpCatalogAddress) -> gpui::SharedString {
    match entry.id.as_str() {
        "vana" => tr("settings-catalog-address-label-vana"),
        "home-assistant" => tr("settings-catalog-address-label-home-assistant"),
        _ => address.label.clone().into(),
    }
}

/// Where to find the address, in the person's language.
fn address_help(entry: &McpCatalogEntry, address: &McpCatalogAddress) -> gpui::SharedString {
    match entry.id.as_str() {
        "vana" => tr("settings-catalog-address-help-vana"),
        "home-assistant" => tr("settings-catalog-address-help-home-assistant"),
        _ => address.help.clone().into(),
    }
}

/// Whether nothing useful is at the address, so a different one might work.
/// This reads the failure's code: its words depend on the language.
fn is_address_failure(failure: &Failure) -> bool {
    matches!(
        failure,
        Failure::Host(failure) if matches!(failure.code.as_str(), "mcp-unreachable" | "mcp-no-server")
    )
}

fn matches_search(entry: &McpCatalogEntry, query: &str) -> bool {
    query.is_empty()
        || entry.name.to_lowercase().contains(query)
        || summary(entry).to_lowercase().contains(query)
        || category_label(entry.category)
            .to_lowercase()
            .contains(query)
}

/// The apps promoted at the top of Integrations, in display order. Connected
/// ones drop out; the strip is not backfilled, so it only ever shows these.
pub(super) const FEATURED_APPS: [&str; 5] = ["stripe", "cloudflare", "vercel", "notion", "dropbox"];

impl SettingsScreen {
    /// Work out which entries show, when something they depend on changes.
    pub(super) fn refresh_catalog(&mut self) {
        let servers = self.mcp_servers.as_deref().unwrap_or_default();
        self.catalog_featured = FEATURED_APPS
            .iter()
            .filter_map(|id| mcp_catalog().iter().find(|entry| entry.id == *id))
            .filter(|entry| !is_connected(entry, servers))
            .collect();
        self.catalog_visible = mcp_catalog()
            .iter()
            .filter(|entry| {
                !is_connected(entry, servers) && matches_search(entry, &self.catalog_query)
            })
            .collect();
    }

    /// The directory in the order the Apps page draws it: by category while
    /// nothing is searched, otherwise one flat list of matches. Application
    /// Vim follows the same order.
    pub(super) fn catalog_display_order(&self) -> Vec<&'static McpCatalogEntry> {
        if !self.catalog_query.is_empty() {
            return self.catalog_visible.clone();
        }
        McpCatalogCategory::ALL
            .into_iter()
            .flat_map(|category| {
                self.catalog_visible
                    .iter()
                    .copied()
                    .filter(move |entry| entry.category == category)
            })
            .collect()
    }

    pub(super) fn start_mcp_sign_in(&mut self, request: McpSignIn, cx: &mut Context<Self>) {
        if self.mcp_signing_in.is_some() {
            return;
        }
        let name = request.name.clone();
        self.mcp_signing_in = Some(name.clone());
        self.mcp_usual_address = None;
        self.mcp_sign_in_error = None;
        self.mcp_app_missing = None;
        self.mcp_notice = None;
        cx.notify();
        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move { backend.sign_in_mcp_server(&user_id, request).await },
            cx,
            move |this, result, cx| {
                // A sign-in the person stopped has nothing left to report.
                if this.mcp_signing_in.as_deref() == Some(name.as_str()) {
                    this.mcp_signing_in = None;
                    match result {
                        Ok(servers) => {
                            this.mcp_servers = Some(servers);
                            if this
                                .mcp_editor
                                .as_ref()
                                .is_some_and(|editor| editor.name.read(cx).text().trim() == name)
                            {
                                this.mcp_editor = None;
                            }
                            this.refresh_catalog();
                        }
                        Err(failure) => this.mcp_sign_in_failed(name, failure, cx),
                    }
                    if this.settings.application_vim_enabled {
                        this.reconcile_application_vim_target();
                    }
                }
                cx.notify();
            },
        );
    }

    /// Connect an app that runs on this computer, with the same button states
    /// as a sign-in. The host checks what it needs before anything starts.
    fn start_mcp_app(&mut self, entry: &'static McpCatalogEntry, cx: &mut Context<Self>) {
        if self.mcp_signing_in.is_some() {
            return;
        }
        let name = entry.name.clone();
        self.mcp_signing_in = Some(name.clone());
        self.mcp_sign_in_error = None;
        self.mcp_app_missing = None;
        self.mcp_notice = None;
        cx.notify();
        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        let id = entry.id.clone();
        self.call(
            async move { backend.connect_mcp_app(&user_id, &id).await },
            cx,
            move |this, result, cx| {
                // A connection the person stopped has nothing left to report.
                if this.mcp_signing_in.as_deref() == Some(name.as_str()) {
                    this.mcp_signing_in = None;
                    match result {
                        Ok(McpAppConnection::Connected(servers)) => {
                            this.mcp_servers = Some(servers);
                            this.refresh_catalog();
                        }
                        Ok(McpAppConnection::Missing(missing)) => {
                            this.mcp_app_missing = Some((name, missing));
                        }
                        Err(message) => this.mcp_sign_in_error = Some((name, message.into())),
                    }
                    if this.settings.application_vim_enabled {
                        this.reconcile_application_vim_target();
                    }
                }
                cx.notify();
            },
        );
    }

    /// A sign-in nobody can see or stop any more ends with Settings, which
    /// also closes its callback listener.
    pub(super) fn abandon_mcp_sign_in(&mut self) {
        let Some(name) = self.mcp_signing_in.take() else {
            return;
        };
        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        drop(
            self.backend
                .spawn(async move { backend.cancel_mcp_sign_in(&user_id, &name).await }),
        );
    }

    pub(super) fn cancel_mcp_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.mcp_signing_in.take() else {
            return;
        };
        cx.notify();
        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        let stopping = name.clone();
        self.call(
            async move { backend.cancel_mcp_sign_in(&user_id, &stopping).await },
            cx,
            move |this, result, cx| {
                if let Err(message) = result {
                    this.mcp_sign_in_error = Some((name, message.into()));
                    cx.notify();
                }
            },
        );
    }

    /// Whether the sign-in in progress is the one the server form started.
    pub(super) fn mcp_editor_signing_in(&self, cx: &App) -> bool {
        self.mcp_editor
            .as_ref()
            .zip(self.mcp_signing_in.as_deref())
            .is_some_and(|(editor, name)| editor.name.read(cx).text().trim() == name)
    }

    /// Why the last sign-in to this server failed, shown where it started.
    pub(super) fn sign_in_error_for(&self, name: &str) -> Option<gpui::SharedString> {
        self.mcp_sign_in_error
            .as_ref()
            .filter(|(failed, _)| failed == name)
            .map(|(_, message)| message.resolve())
    }

    pub(super) fn connect_catalog_entry(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(entry) = mcp_catalog().iter().find(|entry| entry.id == id) else {
            return;
        };
        match &entry.auth {
            McpCatalogAuth::Oauth if entry.address().is_some() => {
                self.connect_mcp_address(entry, cx);
            }
            McpCatalogAuth::Oauth => {
                if let Some(request) = entry.sign_in() {
                    self.start_mcp_sign_in(request, cx);
                }
            }
            McpCatalogAuth::None => self.start_mcp_app(entry, cx),
        }
    }

    /// Try where an app usually is, so most people are never asked. The pop-up
    /// is for an app that is somewhere else.
    fn connect_mcp_address(&mut self, entry: &'static McpCatalogEntry, cx: &mut Context<Self>) {
        if self.mcp_signing_in.is_some() {
            return;
        }
        let Some(address) = entry.address() else {
            return;
        };
        match entry.sign_in_at(&address.default) {
            Ok(request) => {
                self.start_mcp_sign_in(request, cx);
                self.mcp_usual_address = Some(entry);
            }
            Err(error) => self.open_mcp_address(entry, Some(failure_text(&error.into())), cx),
        }
    }

    fn mcp_sign_in_failed(&mut self, name: String, failure: Failure, cx: &mut Context<Self>) {
        let ask = self
            .mcp_usual_address
            .take()
            .filter(|_| is_address_failure(&failure));
        let notice = UiText::from(failure);
        match ask {
            Some(entry) => self.open_mcp_address(entry, Some(notice), cx),
            None => self.mcp_sign_in_error = Some((name, notice)),
        }
    }

    fn open_mcp_address(
        &mut self,
        entry: &'static McpCatalogEntry,
        error: Option<UiText>,
        cx: &mut Context<Self>,
    ) {
        let Some(address) = entry.address() else {
            return;
        };
        self.mcp_sign_in_error = None;
        let application_vim = self.settings.application_vim_enabled;
        let screen = cx.entity().downgrade();
        let submit = screen.clone();
        let input = cx.new(move |cx| {
            let mut input = TextInput::localized("settings-catalog-address", cx)
                .with_tab_index(1)
                .application_vim(application_vim)
                .on_key(move |event, _text, _window, cx| {
                    let cancelled = event.keystroke.key == "escape";
                    if cancelled {
                        let _ = screen.update(cx, |screen, cx| screen.close_mcp_address(cx));
                    }
                    cancelled
                })
                .on_enter(move |text, _window, cx| {
                    let _ = submit.update(cx, |screen, cx| screen.submit_mcp_address(&text, cx));
                });
            input.set_text(&address.default, cx);
            input
        });
        self.mcp_address = Some(McpAddressPrompt {
            entry,
            input,
            error,
            focus_pending: true,
        });
        cx.notify();
    }

    fn close_mcp_address(&mut self, cx: &mut Context<Self>) {
        self.mcp_address = None;
        self.application_focus_pending = true;
        cx.notify();
    }

    fn submit_mcp_address(&mut self, given: &str, cx: &mut Context<Self>) {
        let Some(prompt) = self.mcp_address.as_mut() else {
            return;
        };
        match prompt.entry.sign_in_at(given) {
            Ok(request) => {
                self.close_mcp_address(cx);
                self.start_mcp_sign_in(request, cx);
            }
            Err(error) => {
                prompt.error = Some(failure_text(&error.into()));
                cx.notify();
            }
        }
    }

    /// Focus the pop-up's field the first time it draws.
    pub(super) fn focus_mcp_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(prompt) = self.mcp_address.as_mut()
            && std::mem::take(&mut prompt.focus_pending)
        {
            window.focus(&prompt.input.read(cx).focus_handle(cx), cx);
        }
    }

    pub(super) fn render_mcp_address(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let prompt = self.mcp_address.as_ref()?;
        let address = prompt.entry.address()?;
        let title = self.messages.get(
            "settings-catalog-address-title",
            [("name", prompt.entry.name.clone().into())],
        );
        let get_app = self.messages.get(
            "settings-catalog-get-app",
            [("name", address.app_name.clone().into())],
        );
        let get_url = address.get_url.clone();
        let width = (window.viewport_size().width - px(48.)).clamp(px(0.), px(480.));
        Some(
            div()
                .id("mcp-address-backdrop")
                .absolute()
                .inset_0()
                .size_full()
                .occlude()
                .bg(theme::scrim())
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("mcp-address")
                        .debug_selector(|| "mcp-address".to_string())
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
                        .child(div().text_lg().child(title))
                        .child(
                            div()
                                .text_sm()
                                .text_color(gpui::rgb(theme::text_secondary()))
                                .child(gpui::text!(address_help(prompt.entry, address))),
                        )
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
                                        .child(gpui::text!(address_label(prompt.entry, address))),
                                )
                                .child(widgets::input_frame().text_sm().child(prompt.input.clone()))
                                .when_some(prompt.error.as_ref(), |col, error| {
                                    col.child(
                                        div()
                                            .text_xs()
                                            .text_color(gpui::rgb(theme::status_error()))
                                            .child(gpui::text!(error.resolve())),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    div()
                                        .id("mcp-address-get")
                                        .role(gpui::Role::Link)
                                        .aria_label(get_app.clone())
                                        .text_xs()
                                        .text_color(gpui::rgb(theme::link()))
                                        .hover(|style| style.underline().cursor_pointer())
                                        .on_click(move |_, _, _| {
                                            crate::ui::rich_text::open_link(&get_url)
                                        })
                                        .child(get_app),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            widgets::ghost_button("mcp-address-cancel")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.close_mcp_address(cx);
                                                }))
                                                .child(gpui::text!(tr("settings-catalog-cancel"))),
                                        )
                                        .child(
                                            widgets::default_button("mcp-address-connect")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    let given = this
                                                        .mcp_address
                                                        .as_ref()
                                                        .map(|prompt| prompt.input.read(cx).text())
                                                        .unwrap_or_default();
                                                    this.submit_mcp_address(&given, cx);
                                                }))
                                                .child(gpui::text!(tr("settings-catalog-connect"))),
                                        ),
                                ),
                        ),
                ),
        )
    }

    /// One directory row: logo, name, a one-line summary (or the connect
    /// feedback), and a `+`. `featured` rows sit in the Popular section and
    /// get their own target so the same app lower down stays distinct.
    fn render_catalog_card(
        &self,
        entry: &'static McpCatalogEntry,
        featured: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let waiting = self.mcp_signing_in.as_deref() == Some(entry.name.as_str());
        let busy = self.mcp_signing_in.is_some() && !waiting;
        let prefix = if featured {
            "catalog-popular"
        } else {
            "catalog"
        };
        let button_id = gpui::SharedString::from(format!("{prefix}-connect-{}", entry.id));
        let missing = self
            .mcp_app_missing
            .as_ref()
            .filter(|(name, _)| *name == entry.name)
            .map(|(_, missing)| missing);
        let button = if let Some(missing) = missing {
            let url = missing.get_url.clone();
            widgets::secondary_button(button_id)
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    cx.open_url(&url);
                    this.mcp_app_missing = None;
                    cx.notify();
                }))
                .child(self.messages.get(
                    "settings-catalog-get-app",
                    [("name", missing.name.clone().into())],
                ))
        } else if waiting {
            widgets::ghost_button(button_id)
                .on_click(cx.listener(|this, _event, _window, cx| this.cancel_mcp_sign_in(cx)))
                .child(spinner(
                    &format!("{prefix}-spinner-{}", entry.id),
                    widgets::ROW_ICON,
                    theme::text_secondary(),
                ))
                .child(tr("settings-catalog-cancel"))
        } else {
            widgets::icon_button(
                button_id,
                "plus",
                tr("settings-catalog-connect"),
                widgets::ROW_ICON,
                theme::text_secondary(),
            )
            .when(busy, |button| button.opacity(0.5))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.connect_catalog_entry(&entry.id, cx);
            }))
        };
        let (detail, detail_color) = match (missing, self.sign_in_error_for(&entry.name)) {
            (Some(missing), _) => {
                let text = match &missing.min_version {
                    Some(version) => self.messages.get(
                        "settings-catalog-needs-version",
                        [
                            ("name", missing.name.clone().into()),
                            ("version", version.clone().into()),
                        ],
                    ),
                    None => self.messages.get(
                        "settings-catalog-needs-app",
                        [("name", missing.name.clone().into())],
                    ),
                };
                (text.to_string(), theme::status_error())
            }
            (None, Some(error)) => (error.to_string(), theme::status_error()),
            (None, None) => (summary(entry).to_string(), theme::text_secondary()),
        };
        let card = div()
            .debug_selector(move || format!("{prefix}-card-{}", entry.id))
            .flex()
            .items_center()
            .gap_3()
            .px_2()
            .py_1p5()
            .rounded(theme::RADIUS_MD)
            .hover(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_hover())))
            .child(
                div()
                    .size(px(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    // White in both themes: these marks are made for it.
                    .bg(gpui::white())
                    .border_1()
                    .border_color(gpui::rgb(theme::border_subtle()))
                    .child(
                        gpui::img(logo_path(entry))
                            .size(px(24.))
                            .rounded(theme::RADIUS_SM),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .truncate()
                            .child(entry.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(detail_color))
                            .line_clamp(2)
                            .child(detail),
                    ),
            )
            .child(div().flex_none().child(button));
        let id = entry.id.clone();
        self.application_target(
            move || {
                if featured {
                    SettingsTarget::McpFeatured(id.clone())
                } else {
                    SettingsTarget::McpCatalog(id.clone())
                }
            },
            card,
        )
    }

    /// Open the Apps page with the search cleared, so the person sees the
    /// whole directory.
    pub(super) fn open_apps_directory(&mut self, cx: &mut Context<Self>) {
        self.catalog_search
            .update(cx, |input, cx| input.set_text("", cx));
        self.catalog_query.clear();
        self.refresh_catalog();
        self.select_section(Section::Apps, cx);
    }

    /// "Popular apps": a short strip of well-known apps above the built-ins,
    /// so the directory is visible before anyone scrolls. `None` when every
    /// featured app is connected.
    pub(super) fn render_featured_apps(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.catalog_featured.is_empty() {
            return None;
        }
        let total = mcp_catalog().len();
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(subsection_title(&tr("settings-catalog-popular")))
            .child(
                self.application_target(
                    || SettingsTarget::McpCatalogSeeAll,
                    widgets::ghost_button("catalog-see-all")
                        .debug_selector(|| "catalog-see-all".into())
                        .py_1()
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.open_apps_directory(cx);
                        }))
                        .child(self.messages.get(
                            "settings-catalog-see-all",
                            [("count", total.to_string().into())],
                        )),
                ),
            );
        let tiles = div().flex().flex_wrap().gap_2().children(
            self.catalog_featured
                .iter()
                .map(|entry| self.render_featured_tile(entry, cx)),
        );
        Some(div().flex().flex_col().gap_2().child(header).child(tiles))
    }

    fn render_featured_tile(
        &self,
        entry: &'static McpCatalogEntry,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let waiting = self.mcp_signing_in.as_deref() == Some(entry.name.as_str());
        let busy = self.mcp_signing_in.is_some() && !waiting;
        let missing = self
            .mcp_app_missing
            .as_ref()
            .filter(|(name, _)| *name == entry.name)
            .map(|(_, missing)| missing.clone());
        // The card below the fold carries the same detail; repeat it here so
        // a failed connect from the strip is not a dead click.
        let (detail, detail_color): (gpui::SharedString, u32) =
            match (&missing, self.sign_in_error_for(&entry.name)) {
                (Some(missing), _) => (
                    self.messages.get(
                        "settings-catalog-get-app",
                        [("name", missing.name.clone().into())],
                    ),
                    theme::status_error(),
                ),
                (None, Some(error)) => (error, theme::status_error()),
                (None, None) => (
                    tr(if waiting {
                        "settings-catalog-cancel"
                    } else {
                        "settings-catalog-connect"
                    }),
                    theme::text_muted(),
                ),
            };
        let tile = div()
            .id(gpui::SharedString::from(format!(
                "catalog-featured-{}",
                entry.id
            )))
            .debug_selector(move || format!("catalog-featured-{}", entry.id))
            .flex_1()
            .min_w(px(150.))
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded(theme::RADIUS_LG)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border_subtle()))
            .when(busy, |tile| tile.opacity(0.5))
            .when(!busy, |tile| {
                tile.cursor_pointer()
                    .hover(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_hover())))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        if this.mcp_signing_in.as_deref() == Some(entry.name.as_str()) {
                            this.cancel_mcp_sign_in(cx);
                        } else if let Some(url) = this
                            .mcp_app_missing
                            .as_ref()
                            .filter(|(name, _)| *name == entry.name)
                            .map(|(_, missing)| missing.get_url.clone())
                        {
                            cx.open_url(&url);
                            this.mcp_app_missing = None;
                            cx.notify();
                        } else {
                            this.connect_catalog_entry(&entry.id, cx);
                        }
                    }))
            })
            .child(
                div()
                    .size(px(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    .bg(gpui::white())
                    .border_1()
                    .border_color(gpui::rgb(theme::border_subtle()))
                    .child(
                        gpui::img(logo_path(entry))
                            .size(px(24.))
                            .rounded(theme::RADIUS_SM),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .truncate()
                            .child(entry.name.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .text_color(gpui::rgb(detail_color))
                            .when(waiting, |row| {
                                row.child(spinner(
                                    &format!("catalog-tile-spinner-{}", entry.id),
                                    px(10.),
                                    theme::text_muted(),
                                ))
                            })
                            .child(div().truncate().child(detail)),
                    ),
            );
        self.application_target(move || SettingsTarget::McpFeatured(entry.id.clone()), tile)
    }

    /// The Apps page: the directory grouped by category, Popular first, with
    /// a search that flattens it into one list of matches.
    pub(super) fn render_apps_pane(&self, cx: &mut Context<Self>) -> Div {
        let mut pane = div().flex().flex_col().gap_4().child(
            div()
                .flex()
                .items_start()
                .justify_between()
                .gap_4()
                .child(page_header(
                    &tr("settings-section-apps"),
                    &tr("settings-catalog-intro"),
                ))
                .child(
                    widgets::input_frame()
                        .debug_selector(|| "catalog-search".into())
                        .flex_none()
                        .w(px(260.))
                        .text_sm()
                        .child(self.catalog_search.clone()),
                ),
        );
        if !self.catalog_query.is_empty() {
            if self.catalog_visible.is_empty() {
                return pane.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(tr("settings-catalog-empty")),
                );
            }
            return pane.child(self.render_catalog_grid(&self.catalog_visible, false, cx));
        }
        if !self.catalog_featured.is_empty() {
            pane = pane
                .child(subsection_title(&tr("settings-catalog-popular")))
                .child(self.render_catalog_grid(&self.catalog_featured, true, cx));
        }
        for category in McpCatalogCategory::ALL {
            let entries: Vec<&'static McpCatalogEntry> = self
                .catalog_visible
                .iter()
                .copied()
                .filter(|entry| entry.category == category)
                .collect();
            if entries.is_empty() {
                continue;
            }
            pane = pane
                .child(subsection_title(&category_label(category)))
                .child(self.render_catalog_grid(&entries, false, cx));
        }
        if self.catalog_visible.is_empty() {
            pane = pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(tr("settings-catalog-all-connected")),
            );
        }
        pane
    }

    fn render_catalog_grid(
        &self,
        entries: &[&'static McpCatalogEntry],
        featured: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        div().grid().grid_cols(2).gap_x_6().gap_y_1().children(
            entries
                .iter()
                .map(|entry| self.render_catalog_card(entry, featured, cx)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maple_agent::agent::McpSignInError;

    fn address_error(screen: &SettingsScreen) -> String {
        let prompt = screen.mcp_address.as_ref().unwrap();
        prompt.error.as_ref().unwrap().resolve().to_string()
    }

    #[test]
    fn translated_catalog_search_follows_locale() {
        use crate::ui::i18n::{TestLocalizationGuard, apply_preference};
        let _locale = TestLocalizationGuard::enable();
        for locale in ["en", "es", "en"] {
            apply_preference(Some(locale));
            assert!(matches_search(notion(), "notion"));
            assert!(matches_search(
                notion(),
                if locale == "es" {
                    "bases de datos"
                } else {
                    "databases"
                }
            ));
            assert!(matches_search(
                notion(),
                if locale == "es" {
                    "productividad"
                } else {
                    "productivity"
                }
            ));
        }
    }

    fn saved(key: &str, url: &str) -> McpDefinition {
        McpDefinition {
            key: key.into(),
            name: key.into(),
            description: String::new(),
            enabled: false,
            timeout_seconds: 300,
            target: McpTarget::StreamableHttp { url: url.into() },
            environment: Vec::new(),
            headers: Vec::new(),
        }
    }

    fn notion() -> &'static McpCatalogEntry {
        mcp_catalog()
            .iter()
            .find(|entry| entry.id == "notion")
            .unwrap()
    }

    #[test]
    fn an_app_is_connected_by_its_name_or_its_address() {
        assert!(is_connected(notion(), &[saved("notion", "https://other")]));
        assert!(is_connected(
            notion(),
            &[saved("our-notes", "https://mcp.notion.com/mcp/")]
        ));
        assert!(!is_connected(notion(), &[saved("other", "https://other")]));
    }

    fn screen(cx: &mut Context<SettingsScreen>) -> SettingsScreen {
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                .expect("test backend"),
        );
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
            super::super::Section::Integrations,
            cx,
        )
    }

    #[gpui::test]
    fn a_sign_in_belongs_to_where_it_started(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let screen = cx.new(screen);
        screen.update(cx, |screen, cx| {
            screen.open_mcp_editor(None, cx);
            let name = screen.mcp_editor.as_ref().unwrap().name.clone();
            name.update(cx, |input, cx| input.set_text("Custom", cx));

            screen.mcp_signing_in = Some("Notion".into());
            assert!(!screen.mcp_editor_signing_in(cx));
            screen.mcp_signing_in = Some("Custom".into());
            assert!(screen.mcp_editor_signing_in(cx));
            screen.mcp_signing_in = None;

            screen.mcp_sign_in_error = Some(("Custom".into(), "Not approved".into()));
            assert_eq!(
                screen.sign_in_error_for("Custom").as_deref(),
                Some("Not approved")
            );
            assert_eq!(screen.sign_in_error_for("Notion"), None);
        });
    }

    #[gpui::test]
    fn closing_settings_ends_a_sign_in_in_progress(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let released = std::rc::Rc::new(std::cell::Cell::new(false));
        let screen = cx.new(|cx| {
            let mut screen = screen(cx);
            screen.mcp_signing_in = Some("Vercel".into());
            let released = released.clone();
            cx.on_release(move |screen, _| released.set(screen.mcp_signing_in.is_none()))
                .detach();
            screen
        });
        drop(screen);
        // Entities are released when effects flush, which parking never does.
        cx.update(|_| {});
        assert!(released.get());
    }

    #[gpui::test]
    fn the_address_pop_up_checks_what_it_is_given(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let screen = cx.new(screen);
        let vana = mcp_catalog()
            .iter()
            .find(|entry| entry.id == "vana")
            .unwrap();
        screen.update(cx, |screen, cx| {
            screen.open_mcp_address(vana, None, cx);
            assert_eq!(
                screen.mcp_signing_in, None,
                "nothing starts until it is given"
            );

            screen.submit_mcp_address("", cx);
            assert_eq!(address_error(screen), "Enter an address first.");

            screen.submit_mcp_address("ftp://example.com/mcp", cx);
            assert_eq!(
                address_error(screen),
                "Sign-in needs an http:// or https:// address."
            );
            assert_eq!(screen.mcp_signing_in, None);

            screen.close_mcp_address(cx);
            assert!(screen.mcp_address.is_none());

            screen.open_mcp_address(vana, None, cx);
            screen.submit_mcp_address("http://127.0.0.1:47321/mcp", cx);
            assert!(screen.mcp_address.is_none());
            assert_eq!(screen.mcp_signing_in.as_deref(), Some("Vana"));
            screen.mcp_signing_in = None;
        });
    }

    #[gpui::test]
    fn an_apps_usual_address_is_tried_before_asking(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let screen = cx.new(screen);
        screen.update(cx, |screen, cx| {
            screen.connect_catalog_entry("home-assistant", cx);
            assert!(screen.mcp_address.is_none(), "nothing to ask yet");
            assert_eq!(screen.mcp_signing_in.as_deref(), Some("Home Assistant"));
            screen.mcp_signing_in = None;

            // Not there: ask, starting from where it looked and saying why.
            screen.mcp_sign_in_failed(
                "Home Assistant".into(),
                Failure::Host(McpSignInError::Unreachable.into()),
                cx,
            );
            let prompt = screen.mcp_address.as_ref().unwrap();
            assert_eq!(
                prompt.input.read(cx).text(),
                "http://homeassistant.local:8123"
            );
            assert!(address_error(screen).contains("could not reach"));
            assert_eq!(screen.sign_in_error_for("Home Assistant"), None);
            screen.close_mcp_address(cx);

            // A typed address reports on the card, and does not ask again.
            screen.submit_mcp_address("http://192.168.1.20:8123", cx);
            assert_eq!(screen.mcp_usual_address.map(|entry| &entry.id), None);
            screen.mcp_signing_in = None;
        });
    }

    #[gpui::test]
    fn only_a_missing_server_asks_for_another_address(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let screen = cx.new(screen);
        screen.update(cx, |screen, cx| {
            screen.connect_catalog_entry("home-assistant", cx);
            screen.mcp_signing_in = None;
            screen.mcp_sign_in_failed(
                "Home Assistant".into(),
                "Access was not approved.".into(),
                cx,
            );
            assert!(screen.mcp_address.is_none());
            assert_eq!(
                screen.sign_in_error_for("Home Assistant").as_deref(),
                Some("Access was not approved.")
            );

            // The code decides, never the words, which change with the language.
            screen.connect_catalog_entry("home-assistant", cx);
            screen.mcp_signing_in = None;
            screen.mcp_sign_in_failed(
                "Home Assistant".into(),
                McpSignInError::Unreachable.to_string().into(),
                cx,
            );
            assert!(screen.mcp_address.is_none());
        });
    }

    #[gpui::test]
    fn sign_in_failures_follow_a_language_change(cx: &mut gpui::TestAppContext) {
        use crate::ui::i18n::{TestLocalizationGuard, apply_preference};
        let _locale = TestLocalizationGuard::enable();
        cx.executor().allow_parking();
        let screen = cx.new(screen);
        screen.update(cx, |screen, cx| {
            apply_preference(Some("en"));
            screen.connect_catalog_entry("home-assistant", cx);
            screen.mcp_signing_in = None;
            screen.mcp_sign_in_failed(
                "Home Assistant".into(),
                Failure::Host(McpSignInError::NoServerHere.into()),
                cx,
            );
            screen.mcp_sign_in_error = Some((
                "Vana".into(),
                failure_text(&McpSignInError::Unreachable.into()),
            ));
            assert_eq!(
                address_error(screen),
                "Maple reached this address, but found no MCP server there."
            );
            apply_preference(Some("es"));
            assert_eq!(
                address_error(screen),
                "Maple se conectó con esta dirección, pero no encontró ningún servidor MCP."
            );
            assert_eq!(
                screen.sign_in_error_for("Vana").as_deref(),
                Some("Maple no pudo conectarse con esta dirección. Revísala e inténtalo de nuevo.")
            );
            apply_preference(Some("en"));
            screen.mcp_address = None;
        });
    }

    #[test]
    fn a_connected_app_reads_in_each_language_but_a_persons_own_words_do_not() {
        use crate::ui::i18n::{TestLocalizationGuard, apply_preference};
        let _locale = TestLocalizationGuard::enable();
        let (name, words) = (&notion().name, &notion().summary);
        apply_preference(Some("es"));
        assert_eq!(
            saved_description(name, words).as_ref(),
            "Busca y actualiza tus páginas y bases de datos"
        );
        assert_eq!(
            saved_description(name, "Our team wiki").as_ref(),
            "Our team wiki"
        );
        assert_eq!(saved_description("Our wiki", words).as_ref(), words);
        apply_preference(Some("en"));
        assert_eq!(saved_description(name, words).as_ref(), words);
    }

    #[test]
    fn every_app_reads_in_each_language() {
        use crate::ui::i18n::{TestLocalizationGuard, apply_preference};
        let _locale = TestLocalizationGuard::enable();
        for entry in mcp_catalog() {
            apply_preference(Some("en"));
            assert_eq!(summary(entry).as_ref(), entry.summary, "{}", entry.id);
            if let Some(address) = entry.address() {
                assert_eq!(address_label(entry, address).as_ref(), address.label);
                assert_eq!(address_help(entry, address).as_ref(), address.help);
            }
            apply_preference(Some("es"));
            assert_ne!(summary(entry).as_ref(), entry.summary, "{}", entry.id);
            if let Some(address) = entry.address() {
                assert_ne!(address_label(entry, address).as_ref(), address.label);
                assert_ne!(address_help(entry, address).as_ref(), address.help);
            }
        }
    }

    #[test]
    fn every_app_has_a_logo() {
        use gpui::AssetSource as _;
        for entry in mcp_catalog() {
            let logo = crate::assets::Assets.load(&logo_path(entry)).unwrap();
            assert!(
                logo.is_some_and(|bytes| !bytes.is_empty()),
                "{}",
                entry.name
            );
        }
    }

    #[test]
    fn search_reads_the_name_summary_and_category() {
        assert!(matches_search(notion(), ""));
        assert!(matches_search(notion(), "notion"));
        assert!(matches_search(notion(), "databases"));
        assert!(matches_search(notion(), "productivity"));
        assert!(!matches_search(notion(), "payments"));
    }
}
