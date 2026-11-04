//! The API section: credits, this computer's Local API, and the account's
//! API keys. The backend only speaks Maple's encrypted transport, so other
//! applications reach it through the local endpoint with one of these keys.
//!
//! Credits and keys need a Pro, Max, or Team plan, like the web app's API
//! settings. The Local API card is on every plan: a key can belong to another
//! account, and a listener left on after a downgrade must still turn off.

use crate::ui::i18n::tr;
use gpui::{
    Context, Div, Entity, Focusable, ScrollAnchor, ScrollHandle, SharedString, Window, div,
    prelude::*,
};

use super::{Section, SettingsScreen, SettingsTarget, activates, section_title};
use crate::backend::{MapleApiKey, MapleApiKeyCreated};
use crate::billing::BillingStatusExt;
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::widgets;

const PROXY_DOCS_URL: &str = "https://maple.ai/docs/proxy";

/// Tab order down the pane. The Local API card's indexes sit between the
/// credits and the keys because that is where it is drawn.
#[cfg_attr(not(feature = "proxy"), allow(dead_code))]
pub(super) mod tab_order {
    pub const LEARN_MORE: isize = 15;
    pub const PLAN: isize = 16;
    pub const LOCAL_RETRY: isize = 17;
    pub const LOCAL_ENABLED: isize = 18;
    pub const LOCAL_REFRESH: isize = 19;
    pub const LOCAL_COPY_URL: isize = 20;
    pub const LOCAL_SAVE_PORT: isize = 21;
    pub const LOCAL_PORT: isize = 22;
    pub const KEY_NAME: isize = 23;
    pub const KEY_CREATE: isize = 24;
    pub const KEY_COPY: isize = 25;
    pub const KEY_DONE: isize = 26;
    /// Every key row: rows tab in the order they are drawn.
    pub const KEY_ROW: isize = 27;
}

/// Controls on the API pane, other than the Local API card's, that
/// Application Vim can land on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ApiKeysTarget {
    LearnMore,
    SeePlans,
    BuyMore,
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
    /// A key that can only be copied once must not appear below the fold, so
    /// creating one scrolls the keys card to the top of the pane.
    pub(super) card_anchor: ScrollAnchor,
    pub(super) reveal_card: bool,
    /// The key name awaiting delete confirmation.
    pub(super) confirm_delete: Option<String>,
    pub(super) deleting: Option<String>,
    pub(super) notice: Option<String>,
}

impl ApiKeysState {
    pub(super) fn new(
        application_vim_enabled: bool,
        application_focus: gpui::FocusHandle,
        pane_scroll: ScrollHandle,
        cx: &mut Context<SettingsScreen>,
    ) -> Self {
        let name = cx.new(move |cx| {
            TextInput::localized("settings-api-keys-key-name-for-example-cursor", cx)
                .with_tab_index(tab_order::KEY_NAME)
                .application_vim(application_vim_enabled)
                .on_application_escape(move |window, cx| window.focus(&application_focus, cx))
        });
        Self {
            keys: None,
            error: None,
            name,
            creating: false,
            created: None,
            card_anchor: ScrollAnchor::for_handle(pane_scroll),
            reveal_card: false,
            confirm_delete: None,
            deleting: None,
            notice: None,
        }
    }
}

impl SettingsScreen {
    /// Whether the plan allows API keys. Unknown while billing loads, so
    /// the list is offered until the status says otherwise.
    pub(super) fn api_access(&self) -> Option<bool> {
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

    /// Everything on the pane, in the order it is drawn.
    pub(super) fn api_targets(&self) -> Vec<SettingsTarget> {
        let target = SettingsTarget::ApiKeys;
        let unlocked = self.api_access() != Some(false);
        let mut targets = vec![
            target(ApiKeysTarget::LearnMore),
            target(if unlocked {
                ApiKeysTarget::BuyMore
            } else {
                ApiKeysTarget::SeePlans
            }),
        ];
        #[cfg(feature = "proxy")]
        targets.extend(self.local_api_targets());
        if !unlocked {
            return targets;
        }
        targets.extend([target(ApiKeysTarget::Name), target(ApiKeysTarget::Create)]);
        if self.api_keys.created.is_some() {
            targets.push(target(ApiKeysTarget::DismissCreated));
        }
        for key in self.api_keys.keys.as_deref().unwrap_or_default() {
            if self.api_keys.confirm_delete.as_deref() == Some(key.name.as_str()) {
                targets.push(target(ApiKeysTarget::CancelDelete));
            }
            targets.push(target(ApiKeysTarget::Delete(key.name.clone())));
        }
        targets
    }

    pub(super) fn activate_api_keys_target(
        &mut self,
        target: ApiKeysTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            ApiKeysTarget::LearnMore => crate::ui::rich_text::open_link(PROXY_DOCS_URL),
            ApiKeysTarget::SeePlans | ApiKeysTarget::BuyMore => {
                self.select_section(Section::Billing, cx)
            }
            ApiKeysTarget::Name => {
                let handle = self.api_keys.name.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            ApiKeysTarget::Create => self.create_api_key(cx),
            ApiKeysTarget::DismissCreated => self.dismiss_created_key(cx),
            ApiKeysTarget::Delete(name) => self.delete_api_key(&name, cx),
            ApiKeysTarget::CancelDelete => self.cancel_delete_key(cx),
        }
    }

    fn dismiss_created_key(&mut self, cx: &mut Context<Self>) {
        self.api_keys.created = None;
        self.reconcile_application_vim_target();
        cx.notify();
    }

    fn cancel_delete_key(&mut self, cx: &mut Context<Self>) {
        self.api_keys.confirm_delete = None;
        self.reconcile_application_vim_target();
        cx.notify();
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
                    Ok(created) => this.show_created_key(created, cx),
                    Err(message) => this.api_keys.notice = Some(message),
                }
                this.reconcile_application_vim_target();
                cx.notify();
            },
        );
    }

    fn show_created_key(&mut self, created: MapleApiKeyCreated, cx: &mut Context<Self>) {
        self.api_keys.name.update(cx, |input, cx| input.clear(cx));
        let listed = MapleApiKey {
            name: created.name.clone(),
            created_at: created.created_at.clone(),
        };
        self.api_keys
            .keys
            .get_or_insert_with(Vec::new)
            .insert(0, listed);
        self.api_keys.created = Some(created);
        self.api_keys.reveal_card = true;
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

    /// A key handler that presses a tab stop on Enter or Space, as a click
    /// does.
    pub(super) fn on_press_key(
        press: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl Fn(&gpui::KeyDownEvent, &mut Window, &mut gpui::App) + 'static {
        cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
            if activates(event) {
                cx.stop_propagation();
                press(this, cx);
            }
        })
    }

    pub(super) fn render_api_pane(&self, cx: &mut Context<Self>) -> Div {
        let unlocked = self.api_access() != Some(false);
        let pane = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title(&tr("settings-section-api")))
            .child(self.render_api_intro(cx))
            .child(if unlocked {
                self.render_credits_strip(cx)
            } else {
                self.render_api_upgrade(cx)
            });
        #[cfg(feature = "proxy")]
        let pane = pane.child(self.render_local_api_card(cx));
        pane.when(unlocked, |pane| pane.child(self.render_api_keys_card(cx)))
    }

    fn render_api_intro(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_wrap()
            .items_baseline()
            .gap_x_1p5()
            .text_sm()
            .text_color(gpui::rgb(theme::text_secondary()))
            .child(tr("settings-api-intro"))
            .child(
                self.application_target(
                    || SettingsTarget::ApiKeys(ApiKeysTarget::LearnMore),
                    div()
                        .id("api-learn-more")
                        .role(gpui::Role::Link)
                        .aria_label(tr("settings-api-learn-more"))
                        .text_color(gpui::rgb(theme::link()))
                        .focus_visible(widgets::focus_ring)
                        .hover(|style| style.underline().cursor_pointer())
                        .tab_index(tab_order::LEARN_MORE)
                        .on_click(|_, _, _| crate::ui::rich_text::open_link(PROXY_DOCS_URL))
                        .on_key_down(Self::on_press_key(
                            |_, _| crate::ui::rich_text::open_link(PROXY_DOCS_URL),
                            cx,
                        ))
                        .child(tr("settings-local-api-learn-more")),
                ),
            )
    }

    fn render_api_upgrade(&self, cx: &mut Context<Self>) -> Div {
        let see_plans = |this: &mut Self, cx: &mut Context<Self>| {
            this.select_section(Section::Billing, cx);
        };
        widgets::card_row()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(super::setting_copy(
                tr("settings-api-keys-unlock-api-access"),
                tr("settings-api-keys-api-keys-and-credits-come-with-the-pro-max-and-team-plans"),
            ))
            .child(
                self.application_target(
                    || SettingsTarget::ApiKeys(ApiKeysTarget::SeePlans),
                    widgets::primary_button("api-keys-see-plans")
                        .py_1p5()
                        .tab_index(tab_order::PLAN)
                        .on_click(cx.listener(move |this, _, _, cx| see_plans(this, cx)))
                        .on_key_down(Self::on_press_key(see_plans, cx))
                        .child(tr("settings-api-keys-see-plans")),
                ),
            )
    }

    fn render_credits_strip(&self, cx: &mut Context<Self>) -> Div {
        let buy_more = |this: &mut Self, cx: &mut Context<Self>| {
            this.select_section(Section::Billing, cx);
        };
        widgets::card_row()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(super::setting_copy(
                tr("settings-api-keys-api-credits"),
                self.billing.credits.balance_label(),
            ))
            .child(
                self.application_target(
                    || SettingsTarget::ApiKeys(ApiKeysTarget::BuyMore),
                    widgets::secondary_button("api-keys-buy-more")
                        .py_1p5()
                        .tab_index(tab_order::PLAN)
                        .on_click(cx.listener(move |this, _, _, cx| buy_more(this, cx)))
                        .on_key_down(Self::on_press_key(buy_more, cx))
                        .child(tr("settings-api-keys-buy-more")),
                ),
            )
    }

    fn render_api_keys_card(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let mut rows = vec![self.render_key_form(cx).into_any_element()];
        if let Some(created) = &self.api_keys.created {
            rows.push(self.render_created_key(created, cx).into_any_element());
        }
        let placeholder = |text: String| {
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_faint()))
                .child(text)
                .into_any_element()
        };
        match self.api_keys.keys.as_deref() {
            Some([]) => rows.push(placeholder(
                tr("settings-api-keys-no-api-keys-yet").to_string(),
            )),
            Some(keys) => rows.extend(keys.iter().map(|key| self.render_key_row(key, cx))),
            None => rows.push(placeholder(match &self.api_keys.error {
                Some(message) => message.clone(),
                None => tr("settings-api-keys-loading-keys").to_string(),
            })),
        }
        widgets::card_stack(rows)
            .id("api-keys-card")
            .anchor_scroll(Some(self.api_keys.card_anchor.clone()))
    }

    fn render_key_form(&self, cx: &mut Context<Self>) -> Div {
        let creating = self.api_keys.creating;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(super::setting_copy(
                tr("settings-api-keys-api-keys"),
                tr("settings-api-key-per-app-help"),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().min_w_0().child(self.application_target(
                        || SettingsTarget::ApiKeys(ApiKeysTarget::Name),
                        widgets::input_frame().child(self.api_keys.name.clone()),
                    )))
                    .child(
                        self.application_target(
                            || SettingsTarget::ApiKeys(ApiKeysTarget::Create),
                            widgets::primary_button("api-keys-create")
                                .flex_none()
                                .py_1p5()
                                .tab_index(tab_order::KEY_CREATE)
                                .when(creating, |el| el.opacity(0.6))
                                .when(!creating, |el| {
                                    el.on_click(cx.listener(|this, _, _, cx| {
                                        this.create_api_key(cx);
                                    }))
                                })
                                .on_key_down(Self::on_press_key(Self::create_api_key, cx))
                                .child(if creating {
                                    tr("settings-api-keys-creating")
                                } else {
                                    tr("settings-api-keys-create-key")
                                }),
                        ),
                    ),
            )
            .when_some(self.api_keys.notice.clone(), |form, notice| {
                form.child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::status_error()))
                        .child(notice),
                )
            })
    }

    fn render_created_key(&self, created: &MapleApiKeyCreated, cx: &mut Context<Self>) -> Div {
        let key = SharedString::from(created.key.clone());
        let clipboard_key = key.clone();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(super::setting_copy(
                self.messages.get("settings-api-key-ready", [("arg1", created.name.clone().into())]),
                tr("settings-api-keys-copy-the-key-now-for-your-privacy-it-is-not-stored-anywhere-you-c"),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1p5()
                            .rounded(theme::RADIUS_SM)
                            .bg(gpui::rgb(theme::bg_code_block()))
                            .border_1()
                            .border_color(gpui::rgb(theme::accent()))
                            .text_sm()
                            .font_family(crate::assets::FONT_MONO)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(key.clone()),
                    )
                    .child(
                        widgets::copy_button(
                            "api-keys-copy-created",
                            key,
                            None,
                            Some(cx.entity_id()),
                        )
                        .tab_index(tab_order::KEY_COPY)
                        .on_key_down(Self::on_press_key(
                            move |_, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    clipboard_key.to_string(),
                                ));
                            },
                            cx,
                        )),
                    )
                    .child(
                        self.application_target(
                            || SettingsTarget::ApiKeys(ApiKeysTarget::DismissCreated),
                            widgets::secondary_button("api-keys-dismiss-created")
                                .py_1p5()
                                .tab_index(tab_order::KEY_DONE)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.dismiss_created_key(cx);
                                }))
                                .on_key_down(Self::on_press_key(Self::dismiss_created_key, cx))
                                .child(tr("settings-api-keys-done")),
                        ),
                    ),
            )
    }

    fn render_key_row(&self, key: &MapleApiKey, cx: &mut Context<Self>) -> gpui::AnyElement {
        let confirming = self.api_keys.confirm_delete.as_deref() == Some(key.name.as_str());
        let deleting = self.api_keys.deleting.as_deref() == Some(key.name.as_str());
        let delete_id = SharedString::from(format!("api-key-delete-{}", key.name));
        let delete = {
            let name = key.name.clone();
            move |this: &mut Self, cx: &mut Context<Self>| this.delete_api_key(&name, cx)
        };
        // A quiet icon at rest; the pill only appears once it asks to confirm.
        let delete_button = if confirming || deleting {
            widgets::danger_button(delete_id)
                .py_1p5()
                .when(deleting, |el| el.opacity(0.6))
                .child(if deleting {
                    tr("settings-api-keys-deleting")
                } else {
                    tr("settings-api-keys-confirm-delete")
                })
        } else {
            widgets::icon_button(
                delete_id,
                "trash-2",
                tr("settings-api-delete-key"),
                widgets::ROW_ICON,
                theme::text_muted(),
            )
            .aria_label(self.messages.get(
                "settings-api-delete-key-name",
                [("name", key.name.clone().into())],
            ))
        };
        let delete_button = self.application_target(
            {
                let name = key.name.clone();
                move || SettingsTarget::ApiKeys(ApiKeysTarget::Delete(name))
            },
            delete_button
                .tab_index(tab_order::KEY_ROW)
                .when(!deleting, |el| {
                    let press = delete.clone();
                    el.on_click(cx.listener(move |this, _, _, cx| press(this, cx)))
                        .on_key_down(Self::on_press_key(delete, cx))
                }),
        );
        let actions = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .when(confirming, |actions| {
                actions.child(
                    self.application_target(
                        || SettingsTarget::ApiKeys(ApiKeysTarget::CancelDelete),
                        widgets::ghost_button("api-key-cancel-delete")
                            .py_1p5()
                            .tab_index(tab_order::KEY_ROW)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cancel_delete_key(cx);
                            }))
                            .on_key_down(Self::on_press_key(Self::cancel_delete_key, cx))
                            .child(tr("settings-password-cancel")),
                    ),
                )
            })
            .child(delete_button);
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(super::setting_copy(
                &key.name,
                self.messages.get(
                    "settings-api-key-created",
                    [(
                        "arg1",
                        super::account::member_since(&key.created_at, &self.messages)
                            .to_string()
                            .into(),
                    )],
                ),
            ))
            .child(actions)
            .into_any_element()
    }
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
                crate::settings::AppSettings::default(),
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

    #[gpui::test]
    fn a_new_key_is_listed_first_and_scrolled_into_view(cx: &mut TestAppContext) {
        let settings = screen(cx);
        settings.update(cx, |this, cx| {
            this.api_keys.keys = Some(vec![MapleApiKey {
                name: "older".into(),
                created_at: "2026-10-07T12:00:00Z".into(),
            }]);
            this.api_keys
                .name
                .update(cx, |input, cx| input.set_text("Cursor", cx));
            this.show_created_key(
                MapleApiKeyCreated {
                    name: "Cursor".into(),
                    key: "key".into(),
                    created_at: "2026-11-04T12:00:00Z".into(),
                },
                cx,
            );
            assert_eq!(this.api_keys.keys.as_ref().unwrap()[0].name, "Cursor");
            assert!(this.api_keys.reveal_card);
            assert_eq!(this.api_keys.name.read(cx).text_ref(), "");
        });
    }

    #[gpui::test]
    fn targets_run_down_the_page_with_the_local_api_before_the_keys(cx: &mut TestAppContext) {
        let settings = screen(cx);
        settings.update(cx, |this, _| {
            this.api_keys.keys = Some(vec![MapleApiKey {
                name: "Cursor".into(),
                created_at: "2026-11-04T12:00:00Z".into(),
            }]);
            this.api_keys.confirm_delete = Some("Cursor".into());
            let target = SettingsTarget::ApiKeys;
            let targets = this.api_targets();
            let position = |wanted: SettingsTarget| {
                targets
                    .iter()
                    .position(|target| *target == wanted)
                    .expect("target on the page")
            };
            assert_eq!(targets[0], target(ApiKeysTarget::LearnMore));
            assert_eq!(targets[1], target(ApiKeysTarget::BuyMore));
            assert!(position(target(ApiKeysTarget::Name)) > 1);
            assert!(
                position(target(ApiKeysTarget::CancelDelete))
                    < position(target(ApiKeysTarget::Delete("Cursor".into())))
            );
            assert_eq!(
                targets.last(),
                Some(&target(ApiKeysTarget::Delete("Cursor".into())))
            );
        });
    }
}
