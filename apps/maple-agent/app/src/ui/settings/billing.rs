//! The Billing section: current plan, subscription management through the
//! Stripe customer portal, prepaid credits, and the plans on sale.

use crate::ui::i18n::{UiText, tr};
use gpui::{Context, Div, div, prelude::*};

use super::{
    Section, SettingsScreen, SettingsTarget, info_row_content, page_header, plan_card,
    subsection_title,
};
use crate::billing::{BillingStatus, BillingStatusExt, PlanTier, Product};
use crate::ui::theme;
use crate::ui::widgets;

/// Controls in the Billing pane that Application Vim can land on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum BillingTarget {
    /// Change plans in the Stripe portal.
    ManageSubscription,
    /// The web pricing page, for subscriptions this app cannot change.
    ViewPlans,
    /// Usage → Plan & Billing.
    ManagePlan,
    /// Checkout, or the web pricing page for plans this app cannot start.
    Product(String),
    Credits(super::api_credits::CreditsTarget),
}

pub(super) struct BillingState {
    pub(super) status: Option<BillingStatus>,
    pub(super) status_error: Option<UiText>,
    pub(super) products: Option<Vec<Product>>,
    /// True while a portal or checkout link is being minted.
    pub(super) busy: bool,
    pub(super) notice: Option<UiText>,
    /// When a portal or checkout tab was opened. While recent, every return
    /// to the window refetches, since payment and webhooks take their time.
    pub(super) awaiting_browser: Option<std::time::Instant>,
    /// How many status loads have been issued; lets tests observe a fetch
    /// without depending on when the backend answers.
    pub(super) loads: u32,
    pub(super) credits: super::api_credits::CreditsState,
}

impl BillingState {
    pub(super) fn new(
        application_vim_enabled: bool,
        application_focus: gpui::FocusHandle,
        cx: &mut gpui::Context<SettingsScreen>,
    ) -> Self {
        Self {
            status: None,
            status_error: None,
            products: None,
            busy: false,
            notice: None,
            awaiting_browser: None,
            loads: 0,
            credits: super::api_credits::CreditsState::new(
                application_vim_enabled,
                application_focus,
                cx,
            ),
        }
    }

    /// Plans worth listing (active, sold to this build, priced, and Pro,
    /// Max, or Team), cheapest first. Computed on load, not in render.
    fn sort_products(products: &mut [Product]) {
        products.sort_by_key(|product| {
            product
                .default_price
                .as_ref()
                .map(|price| price.unit_amount)
                .unwrap_or(i64::MAX)
        });
    }
}

/// What the button next to a product does for the current subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProductAction {
    Current,
    /// Start a Stripe checkout from this app.
    Checkout,
    /// Change plans in the Stripe portal.
    Portal,
    /// Team seats and non-Stripe subscriptions are handled on the web.
    Web,
}

pub(super) fn product_action(status: &BillingStatus, product: &Product) -> ProductAction {
    if status.product_id.as_deref() == Some(product.id.as_str()) {
        return ProductAction::Current;
    }
    let tier = PlanTier::from_product_name(Some(&product.name));
    if tier == PlanTier::Team {
        return ProductAction::Web;
    }
    if !status.is_subscribed || status.tier() == PlanTier::Free {
        return ProductAction::Checkout;
    }
    if status.has_portal() {
        ProductAction::Portal
    } else {
        ProductAction::Web
    }
}

impl SettingsScreen {
    pub(super) fn load_billing(&mut self, cx: &mut Context<Self>) {
        self.billing.loads = self.billing.loads.wrapping_add(1);
        self.load_credit_balance(cx);
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move { backend.call_presented("billing_status", (&user_id,)).await },
            cx,
            |this, result, cx| {
                match result {
                    Ok(status) => {
                        this.billing.status = Some(status);
                        this.billing.status_error = None;
                    }
                    Err(message) => this.billing.status_error = Some(message.into()),
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
        let backend = self.backend.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented::<Vec<crate::billing::Product>>("billing_products", ())
                    .await
            },
            cx,
            |this, result, cx| {
                if let Ok(mut products) = result {
                    products.retain(|product| {
                        product.active
                            && product.is_available != Some(false)
                            && product.default_price.is_some()
                            && PlanTier::from_product_name(Some(&product.name)).is_for_sale()
                    });
                    BillingState::sort_products(&mut products);
                    this.billing.products = Some(products);
                    if this.settings.application_vim_enabled {
                        this.reconcile_application_vim_target();
                    }
                    cx.notify();
                }
            },
        );
    }

    pub(super) fn billing_targets(&self) -> Vec<SettingsTarget> {
        let mut targets = vec![SettingsTarget::PageRefresh];
        if let Some(status) = self.billing.status.as_ref() {
            match plan_action(status) {
                Some(PlanAction::Portal) => {
                    targets.push(SettingsTarget::Billing(BillingTarget::ManageSubscription))
                }
                Some(PlanAction::Web) => {
                    targets.push(SettingsTarget::Billing(BillingTarget::ViewPlans))
                }
                None => {}
            }
            if shows_plan_list(status) {
                for product in self.billing.products.as_deref().unwrap_or_default() {
                    if product_action(status, product) != ProductAction::Current {
                        targets.push(SettingsTarget::Billing(BillingTarget::Product(
                            product.id.clone(),
                        )));
                    }
                }
            }
        }
        // Credits sit below the plan, which only renders once status has loaded.
        if self.api_access() == Some(true) {
            targets.extend(self.credits_targets());
        }
        targets
    }

    pub(super) fn activate_billing_target(
        &mut self,
        target: BillingTarget,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            BillingTarget::ManageSubscription => self.open_billing_portal(cx),
            BillingTarget::ViewPlans => self.open_pricing_page(cx),
            BillingTarget::ManagePlan => self.navigate_to(Section::Billing, cx),
            BillingTarget::Product(id) => self.choose_product(&id, cx),
            BillingTarget::Credits(target) => self.activate_credits_target(target, window, cx),
        }
    }

    /// Usage: the plan meter and extra-credit balance at a glance; plan
    /// changes and purchases stay on Plan & Billing.
    pub(super) fn render_usage_pane(&self, cx: &mut Context<Self>) -> Div {
        let mut pane = div().flex().flex_col().gap_4().child(
            div()
                .flex()
                .items_start()
                .justify_between()
                .gap_4()
                .child(page_header(
                    &tr("settings-section-usage"),
                    &tr("settings-usage-subtitle"),
                ))
                .child(self.page_refresh_button("usage-refresh", cx)),
        );
        pane = pane.child(subsection_title(&tr("settings-billing-plan")));
        pane = pane.child(match &self.plan {
            Some(usage) => plan_card(usage, &self.messages).into_any_element(),
            None => div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(tr("settings-settings-plan-usage-unavailable"))
                .into_any_element(),
        });
        let rows = vec![
            info_row_content(
                &tr("settings-usage-extra-credits"),
                self.billing.credits.balance_label(),
            )
            .into_any_element(),
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(gpui::text!(tr("settings-usage-what-counts")))
                .into_any_element(),
            self.application_target(
                || SettingsTarget::Billing(BillingTarget::ManagePlan),
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(super::setting_copy(
                        tr("settings-usage-manage-plan"),
                        tr("settings-usage-manage-plan-description"),
                    ))
                    .child(
                        widgets::secondary_button("usage-manage-plan")
                            .flex_none()
                            .py_1p5()
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.navigate_to(Section::Billing, cx);
                            }))
                            .child(tr("settings-usage-manage-plan"))
                            .child(widgets::navigate_mark(theme::text_primary())),
                    ),
            ),
        ];
        pane.child(widgets::card_stack(rows))
    }

    pub(super) fn refresh_billing(&mut self, cx: &mut Context<Self>) {
        self.billing.notice = None;
        self.load_billing(cx);
        self.load_plan(cx);
        cx.notify();
    }

    /// How long after a portal or checkout handoff a return to the window
    /// still refetches billing.
    pub(super) const BROWSER_HANDOFF_WINDOW: std::time::Duration =
        std::time::Duration::from_secs(10 * 60);

    /// The window came back to the front. If a browser handoff is recent,
    /// refetch billing, keeping the notice that explains what was opened.
    pub(super) fn on_window_activated(&mut self, cx: &mut Context<Self>) {
        let Some(since) = self.billing.awaiting_browser else {
            return;
        };
        if since.elapsed() > Self::BROWSER_HANDOFF_WINDOW {
            self.billing.awaiting_browser = None;
            return;
        }
        self.load_billing(cx);
        self.load_plan(cx);
        cx.notify();
    }

    pub(super) fn open_billing_portal(&mut self, cx: &mut Context<Self>) {
        if self.billing.busy {
            return;
        }
        self.billing.busy = true;
        self.billing.notice = None;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move { backend.call_presented::<()>("open_billing_portal", (&user_id,)).await },
            cx,
            |this, result, cx| {
                this.billing.busy = false;
                this.billing.awaiting_browser = result.is_ok().then(std::time::Instant::now);
                this.billing.notice = Some(match result {
                    Ok(()) => UiText::key("settings-billing-opened-the-subscription-portal-in-your-browser-refresh-after-maki"),
                    Err(message) => message.into(),
                });
                cx.notify();
            },
        );
    }

    pub(super) fn open_pricing_page(&mut self, cx: &mut Context<Self>) {
        self.billing.notice = Some(match crate::backend::open_pricing_page_presented() {
            Ok(()) => UiText::key("settings-billing-opened-the-pricing-page-in-your-browser"),
            Err(message) => crate::ui::host_errors::failure_text(&message),
        });
        cx.notify();
    }

    pub(super) fn choose_product(&mut self, product_id: &str, cx: &mut Context<Self>) {
        if self.billing.busy {
            return;
        }
        let Some(status) = self.billing.status.as_ref() else {
            return;
        };
        let Some(product) = self
            .billing
            .products
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|product| product.id == product_id)
        else {
            return;
        };
        match product_action(status, product) {
            ProductAction::Current => {}
            ProductAction::Portal => self.open_billing_portal(cx),
            ProductAction::Web => self.open_pricing_page(cx),
            ProductAction::Checkout => {
                self.billing.busy = true;
                self.billing.notice = None;
                cx.notify();
                let backend = self.backend.clone();
                let user_id = self.user_id.clone();
                let product_id = product.id.clone();
                self.call_presented(
                    async move { backend.call_presented::<()>("start_checkout", (&user_id, product_id)).await },
                    cx,
                    |this, result, cx| {
                        this.billing.busy = false;
                        this.billing.awaiting_browser = result.is_ok().then(std::time::Instant::now);
                        this.billing.notice = Some(match result {
                            Ok(()) => UiText::key("settings-billing-opened-checkout-in-your-browser-refresh-after-paying-to-see-the-n"),
                            Err(message) => message.into(),
                        });
                        cx.notify();
                    },
                );
            }
        }
    }

    pub(super) fn render_billing_pane(&self, cx: &mut Context<Self>) -> Div {
        let busy = self.billing.busy;
        let mut pane = div().flex().flex_col().gap_4().child(
            div()
                .flex()
                .items_start()
                .justify_between()
                .gap_4()
                .child(page_header(
                    &tr("settings-section-billing"),
                    &tr("settings-billing-subtitle"),
                ))
                .child(self.page_refresh_button("billing-refresh", cx)),
        );
        let Some(status) = self.billing.status.as_ref() else {
            return pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_faint()))
                    .child(match &self.billing.status_error {
                        Some(message) => message.resolve(),
                        None => tr("settings-billing-loading-plan"),
                    }),
            );
        };

        let plan_name = status
            .product_name
            .clone()
            .unwrap_or_else(|| tr("settings-billing-free").to_string());
        let current_product = status.product_id.as_deref().and_then(|id| {
            self.billing
                .products
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|product| product.id == id)
        });
        let price =
            current_product.and_then(|product| super::presentation::price(product, &self.messages));
        let mut summary = div().flex().flex_col().gap_1().child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_lg()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(gpui::rgb(theme::text_primary()))
                        .child(plan_name),
                )
                .when_some(
                    status
                        .subscription_status
                        .as_deref()
                        .filter(|state| *state != "active"),
                    |row, state| row.child(subscription_badge(state)),
                ),
        );
        if let Some(price) = price {
            summary = summary.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(price),
            );
        }
        if let Some(label) = super::presentation::period_end(status, &self.messages) {
            summary = summary.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(label),
            );
        }
        if let Some(provider) = payment_provider_note(status) {
            summary = summary.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(provider),
            );
        }
        let plan_button = match plan_action(status) {
            Some(PlanAction::Portal) => Some(
                self.application_target(
                    || SettingsTarget::Billing(BillingTarget::ManageSubscription),
                    widgets::secondary_button("billing-portal")
                        .flex_none()
                        .py_1p5()
                        .when(busy, |el| el.opacity(0.6))
                        .when(!busy, |el| {
                            el.on_click(cx.listener(|this, _event, _window, cx| {
                                this.open_billing_portal(cx);
                            }))
                        })
                        .child(tr("settings-billing-change-plan"))
                        .child(widgets::external_mark(theme::text_primary())),
                ),
            ),
            Some(PlanAction::Web) => Some(
                self.application_target(
                    || SettingsTarget::Billing(BillingTarget::ViewPlans),
                    widgets::secondary_button("billing-view-plans")
                        .flex_none()
                        .py_1p5()
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.open_pricing_page(cx);
                        }))
                        .child(tr("settings-billing-view-plans"))
                        .child(widgets::external_mark(theme::text_primary())),
                ),
            ),
            None => None,
        };
        pane = pane.child(subsection_title(&tr("settings-billing-your-plan")));
        pane = pane.child(
            widgets::card_row()
                .flex()
                .items_center()
                .justify_between()
                .gap_4()
                .child(summary)
                .children(plan_button),
        );
        if let Some(notice) = &self.billing.notice {
            pane = pane.child(widgets::notice(
                "billing-notice-dismiss",
                notice.resolve(),
                cx.listener(|this, _event, _window, cx| {
                    this.billing.notice = None;
                    cx.notify();
                }),
            ));
        }

        // Subscribers change plans in the portal or on the web; only accounts
        // without a paid plan need the in-app checkout list.
        if shows_plan_list(status) {
            pane = pane.child(subsection_title(&tr("settings-billing-choose-a-plan")));
            match self.billing.products.as_deref() {
                Some(products) if !products.is_empty() => {
                    let rows: Vec<_> = products
                        .iter()
                        .filter(|product| product_action(status, product) != ProductAction::Current)
                        .map(|product| self.render_product(status, product, cx))
                        .collect();
                    pane = pane.child(widgets::card_stack(rows));
                }
                Some(_) => {
                    pane = pane.child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_faint()))
                            .child(tr("settings-billing-no-plans-are-on-sale-right-now")),
                    );
                }
                None => {
                    pane = pane.child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_faint()))
                            .child(tr("settings-billing-loading-plans")),
                    );
                }
            }
        }
        if self.api_access() == Some(false) {
            return pane;
        }
        pane.child(self.render_credits(cx))
    }

    fn render_product(
        &self,
        status: &BillingStatus,
        product: &Product,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let action = product_action(status, product);
        let busy = self.billing.busy;
        let label = match action {
            ProductAction::Current | ProductAction::Checkout | ProductAction::Portal => {
                tr("settings-billing-choose")
            }
            ProductAction::Web => tr("settings-billing-see-on-the-web"),
        };
        let id = product.id.clone();
        let button = widgets::secondary_button(gpui::SharedString::from(format!(
            "billing-product-{}",
            product.id
        )))
        .flex_none()
        .py_1p5()
        .when(busy, |el| el.opacity(0.6))
        .when(!busy, |el| {
            el.on_click(cx.listener(move |this, _event, _window, cx| {
                this.choose_product(&id, cx);
            }))
        })
        .child(label)
        .child(widgets::external_mark(theme::text_primary()));
        let row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(gpui::rgb(theme::text_primary()))
                                    .child(product.name.clone()),
                            )
                            .when_some(
                                super::presentation::price(product, &self.messages),
                                |row, price| {
                                    row.child(
                                        div()
                                            .text_sm()
                                            .text_color(gpui::rgb(theme::text_secondary()))
                                            .child(price),
                                    )
                                },
                            ),
                    )
                    .when_some(product.description.clone(), |column, description| {
                        column.child(
                            div()
                                .text_sm()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(description),
                        )
                    }),
            )
            .child(button);
        let id = product.id.clone();
        self.application_target(
            move || SettingsTarget::Billing(BillingTarget::Product(id)),
            row,
        )
    }
}

/// The single action on the "Your plan" card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanAction {
    /// Change plans in the Stripe portal.
    Portal,
    /// Non-Stripe subscriptions change on the web pricing page.
    Web,
}

pub(super) fn plan_action(status: &BillingStatus) -> Option<PlanAction> {
    if shows_plan_list(status) {
        return None;
    }
    if status.has_portal() {
        Some(PlanAction::Portal)
    } else {
        Some(PlanAction::Web)
    }
}

/// Accounts without a paid plan pick one in the app; subscribers change
/// plans through the portal or the web instead of an inline list.
pub(super) fn shows_plan_list(status: &BillingStatus) -> bool {
    !status.is_subscribed || status.tier() == PlanTier::Free
}

fn subscription_badge(state: &str) -> Div {
    let (label, color) = match state {
        "active" => (tr("settings-billing-active"), theme::status_success()),
        "trialing" => (tr("settings-billing-trial"), theme::status_success()),
        "past_due" | "unpaid" => (tr("settings-billing-payment-due"), theme::status_error()),
        "canceled" => (tr("settings-billing-canceled"), theme::text_muted()),
        other => (other.to_string().into(), theme::text_muted()),
    };
    div()
        .px_1p5()
        .py_0p5()
        .rounded(theme::RADIUS_SM)
        .text_xs()
        .text_color(gpui::rgb(color))
        .bg(gpui::rgb(theme::bg_sidebar_pill()))
        .child(label.to_string())
}

fn payment_provider_note(status: &BillingStatus) -> Option<String> {
    match status.payment_provider.as_deref()? {
        "zaprite" => {
            Some(tr("settings-billing-paid-with-bitcoin-to-change-plans-contact-support-trymaple-ai").to_string())
        }
        "subscription_pass" => Some(
            tr("settings-billing-activated-with-a-subscription-pass-it-ends-on-the-date-above-with")
                .to_string(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(id: &str, name: &str, cents: i64) -> Product {
        Product {
            id: id.into(),
            name: name.into(),
            description: None,
            active: true,
            default_price: Some(maple_billing::Price {
                id: format!("price_{id}"),
                currency: "usd".into(),
                unit_amount: cents,
                recurring: None,
            }),
            is_available: None,
        }
    }

    #[test]
    fn free_users_check_out_and_paid_users_use_the_portal() {
        let free = BillingStatus::default();
        assert_eq!(
            product_action(&free, &product("pro", "Pro", 2000)),
            ProductAction::Checkout
        );
        assert_eq!(
            product_action(&free, &product("team", "Team", 4000)),
            ProductAction::Web
        );
        let pro = BillingStatus {
            is_subscribed: true,
            product_id: Some("pro".into()),
            product_name: Some("Pro".into()),
            stripe_customer_id: Some("cus".into()),
            payment_provider: Some("stripe".into()),
            ..BillingStatus::default()
        };
        assert_eq!(
            product_action(&pro, &product("pro", "Pro", 2000)),
            ProductAction::Current
        );
        assert_eq!(
            product_action(&pro, &product("max", "Max", 10000)),
            ProductAction::Portal
        );
        let bitcoin = BillingStatus {
            stripe_customer_id: None,
            payment_provider: Some("zaprite".into()),
            ..pro
        };
        assert_eq!(
            product_action(&bitcoin, &product("max", "Max", 10000)),
            ProductAction::Web
        );
    }

    #[test]
    fn products_sort_cheapest_first() {
        let mut products = vec![product("max", "Max", 10000), product("pro", "Pro", 2000)];
        BillingState::sort_products(&mut products);
        assert_eq!(products[0].id, "pro");
    }
}
