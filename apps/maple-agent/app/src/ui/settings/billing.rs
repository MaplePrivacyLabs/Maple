//! The Billing section: current plan, subscription management through the
//! Stripe customer portal, prepaid credits, and the plans on sale.

use gpui::{Context, Div, div, prelude::*};

use super::{SettingsScreen, SettingsTarget, plan_card, section_title};
use crate::billing::{BillingStatus, BillingStatusExt, PlanTier, Product, price_label};
use crate::ui::theme;
use crate::ui::widgets;

/// Controls in the Billing pane that Application Vim can land on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum BillingTarget {
    Refresh,
    ManageSubscription,
    /// Checkout, or the web pricing page for plans this app cannot start.
    Product(String),
    Credits(super::api_credits::CreditsTarget),
}

pub(super) struct BillingState {
    pub(super) status: Option<BillingStatus>,
    pub(super) status_error: Option<String>,
    pub(super) products: Option<Vec<Product>>,
    /// True while a portal or checkout link is being minted.
    pub(super) busy: bool,
    pub(super) notice: Option<String>,
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
    pub(super) fn load_billing(&self, cx: &mut Context<Self>) {
        self.load_credit_balance(cx);
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.billing_status(&user_id).await },
            cx,
            |this, result, cx| {
                match result {
                    Ok(status) => {
                        this.billing.status = Some(status);
                        this.billing.status_error = None;
                    }
                    Err(message) => this.billing.status_error = Some(message),
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
        let backend = self.backend.clone();
        self.call(
            async move { backend.billing_products().await },
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
        let mut targets = vec![SettingsTarget::Billing(BillingTarget::Refresh)];
        if let Some(status) = self.billing.status.as_ref() {
            if status.has_portal() {
                targets.push(SettingsTarget::Billing(BillingTarget::ManageSubscription));
            }
            for product in self.billing.products.as_deref().unwrap_or_default() {
                if product_action(status, product) != ProductAction::Current {
                    targets.push(SettingsTarget::Billing(BillingTarget::Product(
                        product.id.clone(),
                    )));
                }
            }
        }
        // Credits sit below Plans, which only render once status has loaded.
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
            BillingTarget::Refresh => self.refresh_billing(cx),
            BillingTarget::ManageSubscription => self.open_billing_portal(cx),
            BillingTarget::Product(id) => self.choose_product(&id, cx),
            BillingTarget::Credits(target) => self.activate_credits_target(target, window, cx),
        }
    }

    pub(super) fn refresh_billing(&mut self, cx: &mut Context<Self>) {
        self.billing.notice = None;
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
        self.call(
            async move { backend.open_billing_portal(&user_id).await },
            cx,
            |this, result, cx| {
                this.billing.busy = false;
                this.billing.notice = Some(match result {
                    Ok(()) => "Opened the subscription portal in your browser. Refresh \
                               after making changes."
                        .to_string(),
                    Err(message) => message,
                });
                cx.notify();
            },
        );
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
            ProductAction::Web => {
                self.billing.notice = Some(
                    match crate::backend::open_in_browser(crate::billing::PRICING_URL, "pricing") {
                        Ok(()) => "Opened the pricing page in your browser.".to_string(),
                        Err(message) => message,
                    },
                );
                cx.notify();
            }
            ProductAction::Checkout => {
                self.billing.busy = true;
                self.billing.notice = None;
                cx.notify();
                let backend = self.backend.clone();
                let user_id = self.user_id.clone();
                let product_id = product.id.clone();
                self.call(
                    async move { backend.start_checkout(&user_id, product_id).await },
                    cx,
                    |this, result, cx| {
                        this.billing.busy = false;
                        this.billing.notice = Some(match result {
                            Ok(()) => "Opened checkout in your browser. Refresh after \
                                       paying to see the new plan."
                                .to_string(),
                            Err(message) => message,
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
                .items_center()
                .justify_between()
                .child(section_title("Plan"))
                .child(
                    self.application_target(
                        || SettingsTarget::Billing(BillingTarget::Refresh),
                        widgets::ghost_button("billing-refresh")
                            .py_1p5()
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.refresh_billing(cx);
                            }))
                            .child("Refresh"),
                    ),
                ),
        );
        let Some(status) = self.billing.status.as_ref() else {
            return pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_faint()))
                    .child(match &self.billing.status_error {
                        Some(message) => message.clone(),
                        None => "Loading plan…".to_string(),
                    }),
            );
        };

        let plan_name = status
            .product_name
            .clone()
            .unwrap_or_else(|| "Free".to_string());
        let mut plan = widgets::card_row().flex().flex_col().gap_2().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_4()
                .child(
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
                        .when_some(status.subscription_status.as_deref(), |row, state| {
                            row.child(subscription_badge(state))
                        }),
                )
                .when(status.has_portal(), |row| {
                    row.child(
                        self.application_target(
                            || SettingsTarget::Billing(BillingTarget::ManageSubscription),
                            widgets::secondary_button("billing-portal")
                                .py_1p5()
                                .when(busy, |el| el.opacity(0.6))
                                .when(!busy, |el| {
                                    el.on_click(cx.listener(|this, _event, _window, cx| {
                                        this.open_billing_portal(cx);
                                    }))
                                })
                                .child("Manage subscription"),
                        ),
                    )
                }),
        );
        if let Some(label) = status.period_end_label() {
            plan = plan.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(label),
            );
        }
        if let Some(provider) = payment_provider_note(status) {
            plan = plan.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(provider),
            );
        }
        pane = pane.child(plan);
        if let Some(usage) = self.plan.as_ref() {
            pane = pane.child(plan_card(usage));
        }
        if let Some(notice) = &self.billing.notice {
            pane = pane.child(
                widgets::banner(theme::bg_sidebar_pill())
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(notice.clone()),
            );
        }

        pane = pane.child(section_title("Plans"));
        match self.billing.products.as_deref() {
            Some(products) if !products.is_empty() => {
                for product in products {
                    pane = pane.child(self.render_product(status, product, cx));
                }
            }
            Some(_) => {
                pane = pane.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_faint()))
                        .child("No plans are on sale right now."),
                );
            }
            None => {
                pane = pane.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_faint()))
                        .child("Loading plans…"),
                );
            }
        }
        pane = pane.child(
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(
                    "Payments open in your browser. Bitcoin payments and team seats are \
                     set up on the web pricing page.",
                ),
        );
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
            ProductAction::Current => "Current plan",
            ProductAction::Checkout => "Choose",
            ProductAction::Portal => "Change plan",
            ProductAction::Web => "See on the web",
        };
        let id = product.id.clone();
        let button = match action {
            ProductAction::Current => widgets::secondary_button(gpui::SharedString::from(format!(
                "billing-product-{}",
                product.id
            )))
            .py_1p5()
            .opacity(0.6)
            .child(label),
            _ => widgets::primary_button(gpui::SharedString::from(format!(
                "billing-product-{}",
                product.id
            )))
            .py_1p5()
            .when(busy, |el| el.opacity(0.6))
            .when(!busy, |el| {
                el.on_click(cx.listener(move |this, _event, _window, cx| {
                    this.choose_product(&id, cx);
                }))
            })
            .child(label),
        };
        let row = widgets::card_row()
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
                            .when_some(price_label(product), |row, price| {
                                row.child(
                                    div()
                                        .text_sm()
                                        .text_color(gpui::rgb(theme::text_secondary()))
                                        .child(price),
                                )
                            }),
                    )
                    .when_some(product.description.clone(), |column, description| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(description),
                        )
                    }),
            )
            .child(button);
        if action == ProductAction::Current {
            row.into_any_element()
        } else {
            let id = product.id.clone();
            self.application_target(
                move || SettingsTarget::Billing(BillingTarget::Product(id)),
                row,
            )
        }
    }
}

fn subscription_badge(state: &str) -> Div {
    let (label, color) = match state {
        "active" => ("Active", theme::status_success()),
        "trialing" => ("Trial", theme::status_success()),
        "past_due" | "unpaid" => ("Payment due", theme::status_error()),
        "canceled" => ("Canceled", theme::text_muted()),
        other => (other, theme::text_muted()),
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
            Some("Paid with Bitcoin. To change plans, contact support@trymaple.ai.".to_string())
        }
        "subscription_pass" => Some(
            "Activated with a subscription pass. It ends on the date above without \
             renewing."
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
