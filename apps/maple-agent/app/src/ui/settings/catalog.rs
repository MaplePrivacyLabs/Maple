//! The Apps section of Integrations: the bundled catalog, by search and
//! category, one click from connecting.

use gpui::{App, Context, Div, div, prelude::*, px};
use maple_agent::agent::{
    AgentMcpServer, McpAppConnection, McpCatalogAuth, McpCatalogCategory, McpCatalogEntry,
    McpSignIn, mcp_catalog,
};
use maple_agent::store::records::{McpDefinition, McpTarget};

use super::{
    SettingsScreen, SettingsTarget, icon, pill_button, spinner, subsection_title, theme, widgets,
};

/// A heading that starts a new part of the page.
pub(super) fn section_heading(heading: Div) -> Div {
    div()
        .mt_2()
        .pt_5()
        .border_t_1()
        .border_color(gpui::rgb(theme::border_subtle()))
        .child(heading)
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

fn matches_search(entry: &McpCatalogEntry, query: &str) -> bool {
    query.is_empty()
        || entry.name.to_lowercase().contains(query)
        || entry.summary.to_lowercase().contains(query)
        || entry.category.label().to_lowercase().contains(query)
}

impl SettingsScreen {
    /// Work out which entries show, when something they depend on changes.
    pub(super) fn refresh_catalog(&mut self) {
        let servers = self.mcp_servers.as_deref().unwrap_or_default();
        self.catalog_visible = mcp_catalog()
            .iter()
            .filter(|entry| {
                !is_connected(entry, servers)
                    && self
                        .catalog_category
                        .is_none_or(|category| category == entry.category)
                    && matches_search(entry, &self.catalog_query)
            })
            .collect();
    }

    pub(super) fn start_mcp_sign_in(&mut self, request: McpSignIn, cx: &mut Context<Self>) {
        if self.mcp_signing_in.is_some() {
            return;
        }
        let name = request.name.clone();
        self.mcp_signing_in = Some(name.clone());
        self.mcp_sign_in_error = None;
        self.mcp_app_missing = None;
        self.mcp_notice = None;
        cx.notify();
        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        self.call(
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
                        Err(message) => this.mcp_sign_in_error = Some((name, message)),
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
                        Err(message) => this.mcp_sign_in_error = Some((name, message)),
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
                    this.mcp_sign_in_error = Some((name, message));
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
    pub(super) fn sign_in_error_for(&self, name: &str) -> Option<&str> {
        self.mcp_sign_in_error
            .as_ref()
            .filter(|(failed, _)| failed == name)
            .map(|(_, message)| message.as_str())
    }

    pub(super) fn connect_catalog_entry(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(entry) = mcp_catalog().iter().find(|entry| entry.id == id) else {
            return;
        };
        match &entry.auth {
            McpCatalogAuth::Oauth => {
                if let Some(request) = entry.sign_in() {
                    self.start_mcp_sign_in(request, cx);
                }
            }
            McpCatalogAuth::None => self.start_mcp_app(entry, cx),
        }
    }

    fn set_catalog_category(
        &mut self,
        category: Option<McpCatalogCategory>,
        cx: &mut Context<Self>,
    ) {
        self.catalog_category = category;
        self.refresh_catalog();
        cx.notify();
    }

    fn render_catalog_card(
        &self,
        entry: &'static McpCatalogEntry,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let waiting = self.mcp_signing_in.as_deref() == Some(entry.name.as_str());
        let busy = self.mcp_signing_in.is_some() && !waiting;
        let button_id = gpui::SharedString::from(format!("catalog-connect-{}", entry.id));
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
                .child(missing.action())
        } else if waiting {
            widgets::ghost_button(button_id)
                .on_click(cx.listener(|this, _event, _window, cx| this.cancel_mcp_sign_in(cx)))
                .child(spinner(
                    &entry.id,
                    widgets::ROW_ICON,
                    theme::text_secondary(),
                ))
                .child("Cancel")
        } else {
            widgets::secondary_button(button_id)
                .when(busy, |button| button.opacity(0.5))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.connect_catalog_entry(&entry.id, cx);
                }))
                .child("Connect")
        };
        let (detail, detail_color) = match (missing, self.sign_in_error_for(&entry.name)) {
            (Some(missing), _) => (missing.message(), theme::status_error()),
            (None, Some(error)) => (error.to_string(), theme::status_error()),
            (None, None) => (entry.summary.clone(), theme::text_secondary()),
        };
        let card = widgets::card_row()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .size(px(40.))
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
                            .size(px(30.))
                            .rounded(theme::RADIUS_SM),
                    ),
            )
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
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(entry.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(detail_color))
                            .line_clamp(3)
                            .child(detail),
                    ),
            )
            .child(div().flex_none().child(button));
        self.application_target(|| SettingsTarget::McpCatalog(entry.id.clone()), card)
            .into_any_element()
    }

    pub(super) fn render_app_catalog(&self, cx: &mut Context<Self>) -> Div {
        let mut section = div().flex().flex_col().gap_3().child(section_heading(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(subsection_title("Apps"))
                .child(
                    self.application_target(
                        || SettingsTarget::McpAdd,
                        widgets::secondary_button("mcp-add")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.open_mcp_editor(None, cx);
                            }))
                            .child(icon("plus", widgets::ROW_ICON, theme::text_secondary()))
                            .child("Custom server"),
                    ),
                ),
        ));
        section = section.child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(
                    "New tasks can use the apps you connect. Your sign-ins stay on this computer.",
                ),
        );
        if let Some(notice) = &self.mcp_notice {
            section = section.child(widgets::banner(theme::status_warning()).child(notice.clone()));
        }
        if let Some(editor) = &self.mcp_editor {
            section = section.child(self.render_mcp_editor(editor, cx));
        }
        section = section
            .child(
                widgets::input_frame()
                    .text_sm()
                    .child(self.catalog_search.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(pill_button(
                        "catalog-category-all".to_string(),
                        "All",
                        self.catalog_category.is_none(),
                        cx.listener(|this, _event, _window, cx| {
                            this.set_catalog_category(None, cx);
                        }),
                    ))
                    .children(McpCatalogCategory::ALL.into_iter().map(|category| {
                        pill_button(
                            format!("catalog-category-{}", category.label()),
                            category.label(),
                            self.catalog_category == Some(category),
                            cx.listener(move |this, _event, _window, cx| {
                                this.set_catalog_category(Some(category), cx);
                            }),
                        )
                    })),
            );
        if self.catalog_visible.is_empty() {
            return section.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child("No apps match."),
            );
        }
        let cards: Vec<_> = self
            .catalog_visible
            .iter()
            .map(|entry| self.render_catalog_card(entry, cx))
            .collect();
        section.child(div().grid().grid_cols(2).gap_3().children(cards))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(screen.sign_in_error_for("Custom"), Some("Not approved"));
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
        // Loads started with the screen may hold it a moment longer.
        for _ in 0..500 {
            cx.run_until_parked();
            if released.get() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(released.get());
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
