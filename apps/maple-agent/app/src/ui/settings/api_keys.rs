//! The API keys section: keys for the OpenAI-compatible API and the local
//! proxy. Needs a Pro, Max, or Team plan, like the web app's API settings.

use gpui::{Context, Div, Entity, Focusable, div, prelude::*};

use super::{Section, SettingsScreen, SettingsTarget, section_title};
use crate::backend::{MapleApiKey, MapleApiKeyCreated};
use crate::billing::BillingStatusExt;
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::widgets;

/// Controls in the API keys pane that Application Vim can land on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ApiKeysTarget {
    SeePlans,
    Name,
    Create,
    DismissCreated,
    /// Delete (or confirm deleting) the key with this name.
    Delete(String),
    CancelDelete,
}

pub(super) struct ApiKeysState {
    pub(super) keys: Option<Vec<MapleApiKey>>,
    pub(super) error: Option<String>,
    pub(super) name: Entity<TextInput>,
    pub(super) creating: bool,
    /// The key just created, shown once until dismissed.
    pub(super) created: Option<MapleApiKeyCreated>,
    /// The key name awaiting delete confirmation.
    pub(super) confirm_delete: Option<String>,
    pub(super) deleting: Option<String>,
    pub(super) notice: Option<String>,
}

impl ApiKeysState {
    pub(super) fn new(
        application_vim_enabled: bool,
        application_focus: gpui::FocusHandle,
        cx: &mut Context<SettingsScreen>,
    ) -> Self {
        let name = cx.new(move |cx| {
            TextInput::new("Key name, for example “Cursor”", cx)
                .with_tab_index(15)
                .application_vim(application_vim_enabled)
                .on_application_escape(move |window, cx| window.focus(&application_focus, cx))
        });
        Self {
            keys: None,
            error: None,
            name,
            creating: false,
            created: None,
            confirm_delete: None,
            deleting: None,
            notice: None,
        }
    }
}

impl SettingsScreen {
    /// Whether the plan allows API keys. Unknown while billing loads, so
    /// the list is offered until the status says otherwise.
    fn api_access(&self) -> Option<bool> {
        self.billing
            .status
            .as_ref()
            .map(|status| status.tier().has_api_access())
    }

    pub(super) fn load_api_keys(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.list_api_keys(&user_id).await },
            cx,
            |this, result, cx| {
                match result {
                    Ok(keys) => {
                        this.api_keys.keys = Some(keys);
                        this.api_keys.error = None;
                    }
                    Err(message) => this.api_keys.error = Some(message),
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    pub(super) fn api_keys_targets(&self) -> Vec<SettingsTarget> {
        if self.api_access() == Some(false) {
            return vec![SettingsTarget::ApiKeys(ApiKeysTarget::SeePlans)];
        }
        let mut targets = vec![
            SettingsTarget::ApiKeys(ApiKeysTarget::Name),
            SettingsTarget::ApiKeys(ApiKeysTarget::Create),
        ];
        if self.api_keys.created.is_some() {
            targets.push(SettingsTarget::ApiKeys(ApiKeysTarget::DismissCreated));
        }
        for key in self.api_keys.keys.as_deref().unwrap_or_default() {
            targets.push(SettingsTarget::ApiKeys(ApiKeysTarget::Delete(
                key.name.clone(),
            )));
            if self.api_keys.confirm_delete.as_deref() == Some(key.name.as_str()) {
                targets.push(SettingsTarget::ApiKeys(ApiKeysTarget::CancelDelete));
            }
        }
        targets
    }

    pub(super) fn activate_api_keys_target(
        &mut self,
        target: ApiKeysTarget,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            ApiKeysTarget::SeePlans => self.select_section(Section::Billing, cx),
            ApiKeysTarget::Name => {
                let handle = self.api_keys.name.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            ApiKeysTarget::Create => self.create_api_key(cx),
            ApiKeysTarget::DismissCreated => {
                self.api_keys.created = None;
                self.reconcile_application_vim_target();
                cx.notify();
            }
            ApiKeysTarget::Delete(name) => self.delete_api_key(&name, cx),
            ApiKeysTarget::CancelDelete => {
                self.api_keys.confirm_delete = None;
                self.reconcile_application_vim_target();
                cx.notify();
            }
        }
    }

    pub(super) fn create_api_key(&mut self, cx: &mut Context<Self>) {
        if self.api_keys.creating {
            return;
        }
        let name = self.api_keys.name.read(cx).text();
        if let Err(message) = crate::backend::validate_api_key_name(&name) {
            self.api_keys.notice = Some(message);
            cx.notify();
            return;
        }
        self.api_keys.creating = true;
        self.api_keys.notice = None;
        self.api_keys.created = None;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.create_api_key(&user_id, name).await },
            cx,
            |this, result, cx| {
                this.api_keys.creating = false;
                match result {
                    Ok(created) => {
                        this.api_keys.name.update(cx, |input, cx| input.clear(cx));
                        let listed = MapleApiKey {
                            name: created.name.clone(),
                            created_at: created.created_at.clone(),
                        };
                        this.api_keys
                            .keys
                            .get_or_insert_with(Vec::new)
                            .insert(0, listed);
                        this.api_keys.created = Some(created);
                    }
                    Err(message) => this.api_keys.notice = Some(message),
                }
                this.reconcile_application_vim_target();
                cx.notify();
            },
        );
    }

    /// First press asks for confirmation on the row; the second deletes.
    pub(super) fn delete_api_key(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.api_keys.deleting.is_some() {
            return;
        }
        if self.api_keys.confirm_delete.as_deref() != Some(name) {
            self.api_keys.confirm_delete = Some(name.to_string());
            self.reconcile_application_vim_target();
            cx.notify();
            return;
        }
        self.api_keys.confirm_delete = None;
        self.api_keys.deleting = Some(name.to_string());
        self.api_keys.notice = None;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let name = name.to_string();
        self.call(
            async move { backend.delete_api_key(&user_id, &name).await.map(|()| name) },
            cx,
            |this, result, cx| {
                this.api_keys.deleting = None;
                match result {
                    Ok(name) => {
                        if let Some(keys) = this.api_keys.keys.as_mut() {
                            keys.retain(|key| key.name != name);
                        }
                        if this
                            .api_keys
                            .created
                            .as_ref()
                            .is_some_and(|created| created.name == name)
                        {
                            this.api_keys.created = None;
                        }
                    }
                    Err(message) => this.api_keys.notice = Some(message),
                }
                this.reconcile_application_vim_target();
                cx.notify();
            },
        );
    }

    pub(super) fn render_api_keys_pane(&self, cx: &mut Context<Self>) -> Div {
        let mut pane = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title("API keys"));
        if self.api_access() == Some(false) {
            return pane.child(
                widgets::card_row()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(super::setting_copy(
                        "Unlock API access",
                        "API keys and credits come with the Pro, Max, and Team plans.",
                    ))
                    .child(
                        self.application_target(
                            || SettingsTarget::ApiKeys(ApiKeysTarget::SeePlans),
                            widgets::primary_button("api-keys-see-plans")
                                .py_1p5()
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.select_section(Section::Billing, cx);
                                }))
                                .child("See plans"),
                        ),
                    ),
            );
        }

        pane = pane.child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(format!(
                    "Keys work with any OpenAI-compatible client at {}/v1 and with \
                     `maple-agent proxy --api-key`. A key is shown once, when created.",
                    self.backend.api_url()
                )),
        );

        let creating = self.api_keys.creating;
        pane = pane.child(
            widgets::card_row()
                .flex()
                .items_center()
                .gap_3()
                .child(div().flex_1().child(self.application_target(
                    || SettingsTarget::ApiKeys(ApiKeysTarget::Name),
                    widgets::input_frame().child(self.api_keys.name.clone()),
                )))
                .child(
                    self.application_target(
                        || SettingsTarget::ApiKeys(ApiKeysTarget::Create),
                        widgets::primary_button("api-keys-create")
                            .flex_none()
                            .py_1p5()
                            .when(creating, |el| el.opacity(0.6))
                            .when(!creating, |el| {
                                el.on_click(cx.listener(|this, _event, _window, cx| {
                                    this.create_api_key(cx);
                                }))
                            })
                            .child(if creating {
                                "Creating…"
                            } else {
                                "Create key"
                            }),
                    ),
                ),
        );
        if let Some(notice) = &self.api_keys.notice {
            pane = pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::status_error()))
                    .child(notice.clone()),
            );
        }
        if let Some(created) = &self.api_keys.created {
            pane = pane.child(self.render_created_key(created, cx));
        }

        match self.api_keys.keys.as_deref() {
            Some([]) => {
                pane = pane.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_faint()))
                        .child("No API keys yet."),
                );
            }
            Some(keys) => {
                for key in keys {
                    pane = pane.child(self.render_key_row(key, cx));
                }
            }
            None => {
                pane = pane.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_faint()))
                        .child(match &self.api_keys.error {
                            Some(message) => message.clone(),
                            None => "Loading keys…".to_string(),
                        }),
                );
            }
        }
        pane
    }

    fn render_created_key(&self, created: &MapleApiKeyCreated, cx: &mut Context<Self>) -> Div {
        widgets::card_row()
            .flex()
            .flex_col()
            .gap_2()
            .border_color(gpui::rgb(theme::accent()))
            .child(super::setting_copy(
                &format!("“{}” is ready", created.name),
                "Copy the key now. For your privacy it is not stored anywhere you can \
                 read it back.",
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1p5()
                            .rounded(theme::RADIUS_SM)
                            .bg(gpui::rgb(theme::bg_code_block()))
                            .text_sm()
                            .font_family(crate::assets::FONT_MONO)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(created.key.clone()),
                    )
                    .child(widgets::copy_button(
                        "api-keys-copy-created",
                        gpui::SharedString::from(created.key.clone()),
                        None,
                        Some(cx.entity_id()),
                    ))
                    .child(
                        self.application_target(
                            || SettingsTarget::ApiKeys(ApiKeysTarget::DismissCreated),
                            widgets::secondary_button("api-keys-dismiss-created")
                                .py_1p5()
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.api_keys.created = None;
                                    this.reconcile_application_vim_target();
                                    cx.notify();
                                }))
                                .child("Done"),
                        ),
                    ),
            )
    }

    fn render_key_row(&self, key: &MapleApiKey, cx: &mut Context<Self>) -> gpui::AnyElement {
        let confirming = self.api_keys.confirm_delete.as_deref() == Some(key.name.as_str());
        let deleting = self.api_keys.deleting.as_deref() == Some(key.name.as_str());
        let name = key.name.clone();
        let delete_id = gpui::SharedString::from(format!("api-key-delete-{}", key.name));
        let delete_button = self.application_target(
            {
                let name = name.clone();
                move || SettingsTarget::ApiKeys(ApiKeysTarget::Delete(name))
            },
            if confirming {
                widgets::danger_button(delete_id)
            } else {
                widgets::secondary_button(delete_id)
            }
            .py_1p5()
            .when(deleting, |el| el.opacity(0.6))
            .when(!deleting, |el| {
                let name = name.clone();
                el.on_click(cx.listener(move |this, _event, _window, cx| {
                    this.delete_api_key(&name, cx);
                }))
            })
            .child(match (confirming, deleting) {
                (_, true) => "Deleting…",
                (true, _) => "Confirm delete",
                _ => "Delete",
            }),
        );
        let mut actions = div().flex().items_center().gap_2();
        if confirming {
            actions = actions.child(
                self.application_target(
                    || SettingsTarget::ApiKeys(ApiKeysTarget::CancelDelete),
                    widgets::ghost_button("api-key-cancel-delete")
                        .py_1p5()
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.api_keys.confirm_delete = None;
                            this.reconcile_application_vim_target();
                            cx.notify();
                        }))
                        .child("Cancel"),
                ),
            );
        }
        actions = actions.child(delete_button);
        widgets::card_row()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(super::setting_copy(
                &key.name,
                &format!("Created {}", super::account::member_since(&key.created_at)),
            ))
            .child(actions)
            .into_any_element()
    }
}
