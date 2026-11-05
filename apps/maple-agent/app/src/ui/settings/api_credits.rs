//! Prepaid API credits on the Billing page: the balance and a card or
//! Bitcoin top-up, like the web app's API credits settings. Checkout opens
//! in the browser and returns to the web app; the billing service credits
//! the account from the payment provider, so this pane only re-reads the
//! balance.

use crate::ui::i18n::{UiText, tr, tr_args};
use gpui::{Context, Div, Entity, Focusable, div, prelude::*};

use super::billing::BillingTarget;
use super::{SettingsScreen, SettingsTarget, section_title};
use crate::billing::{
    CREDIT_PACKAGES_DOLLARS, CREDITS_PER_DOLLAR, MAX_CREDIT_PURCHASE_DOLLARS,
    MIN_CREDIT_PURCHASE_DOLLARS, PaymentMethod, format_count,
};
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::widgets;
use maple_agent::maple_api::MapleLoginMethod;

/// Credit controls that Application Vim can land on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CreditsTarget {
    Refresh,
    /// A preset purchase, in whole dollars.
    Package(u64),
    Custom,
    CustomAmount,
    Pay(PaymentMethod),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CreditChoice {
    Package(u64),
    Custom,
}

pub(super) struct CreditsState {
    messages: crate::ui::i18n::MessageCache,
    balance: Option<i64>,
    balance_error: Option<UiText>,
    choice: CreditChoice,
    pub(super) custom: Entity<TextInput>,
    /// Credits for the typed custom amount, parsed when the field changes.
    /// None while it is empty.
    custom_credits: Option<Result<u64, crate::billing::CreditAmountError>>,
    /// The checkout being minted, if any.
    purchasing: Option<PaymentMethod>,
    notice: Option<UiText>,
}

impl CreditsState {
    pub(super) fn new(
        application_vim_enabled: bool,
        application_focus: gpui::FocusHandle,
        cx: &mut Context<SettingsScreen>,
    ) -> Self {
        let custom = cx.new(move |cx| {
            TextInput::new(
                &tr_args(
                    "settings-credits-amount-placeholder",
                    [
                        (
                            "min-credit-purchase-dollars",
                            (MIN_CREDIT_PURCHASE_DOLLARS).to_string().into(),
                        ),
                        (
                            "arg1",
                            (format_count(MAX_CREDIT_PURCHASE_DOLLARS as i64))
                                .to_string()
                                .into(),
                        ),
                    ],
                ),
                cx,
            )
            .with_tab_index(16)
            .application_vim(application_vim_enabled)
            .on_application_escape(move |window, cx| window.focus(&application_focus, cx))
        });
        cx.observe(&custom, |this, input, cx| {
            let credits = crate::billing::parse_custom_dollars(input.read(cx).text_ref());
            if this.billing.credits.custom_credits != credits {
                this.billing.credits.custom_credits = credits;
                cx.notify();
            }
        })
        .detach();
        Self {
            messages: Default::default(),
            balance: None,
            balance_error: None,
            choice: CreditChoice::Package(CREDIT_PACKAGES_DOLLARS[0]),
            custom,
            custom_credits: None,
            purchasing: None,
            notice: None,
        }
    }

    fn selected_credits(&self) -> Result<u64, UiText> {
        selected_credits(self.choice, &self.custom_credits)
    }

    /// Compact label for the API keys strip.
    pub(super) fn balance_label(&self) -> String {
        balance_label(self.balance, self.balance_error.is_some(), &self.messages)
    }
}

fn balance_label(
    balance: Option<i64>,
    has_balance_error: bool,
    messages: &crate::ui::i18n::MessageCache,
) -> String {
    match (balance, has_balance_error) {
        (Some(balance), _) => messages
            .get(
                "settings-credits-balance",
                [("arg1", (format_count(balance)).to_string().into())],
            )
            .to_string(),
        (None, false) => tr("settings-api-credits-loading-balance").to_string(),
        (None, true) => tr("settings-api-credits-balance-unavailable").to_string(),
    }
}

/// Credits a choice buys, or why it cannot be bought.
fn selected_credits(
    choice: CreditChoice,
    custom_credits: &Option<Result<u64, crate::billing::CreditAmountError>>,
) -> Result<u64, UiText> {
    match choice {
        CreditChoice::Package(dollars) => Ok(dollars * CREDITS_PER_DOLLAR),
        CreditChoice::Custom => (*custom_credits)
            .map(|result| result.map_err(credit_error))
            .unwrap_or_else(|| Err(UiText::key("settings-api-credits-enter-an-amount"))),
    }
}

fn credit_error(error: crate::billing::CreditAmountError) -> UiText {
    use crate::billing::CreditAmountError;
    match error {
        CreditAmountError::WholeDollarsRequired => UiText::key("settings-credits-whole-dollars"),
        CreditAmountError::BelowMinimum => UiText::message(
            "settings-credits-minimum",
            [("amount", MIN_CREDIT_PURCHASE_DOLLARS.to_string().into())],
        ),
        CreditAmountError::AboveMaximum => UiText::message(
            "settings-credits-maximum",
            [(
                "amount",
                format_count(MAX_CREDIT_PURCHASE_DOLLARS as i64).into(),
            )],
        ),
    }
}

impl SettingsScreen {
    pub(super) fn refresh_credit_placeholder(&self, cx: &mut Context<Self>) {
        let label = tr_args(
            "settings-credits-amount-placeholder",
            [
                (
                    "min-credit-purchase-dollars",
                    MIN_CREDIT_PURCHASE_DOLLARS.to_string().into(),
                ),
                (
                    "arg1",
                    format_count(MAX_CREDIT_PURCHASE_DOLLARS as i64).into(),
                ),
            ],
        );
        self.billing
            .credits
            .custom
            .update(cx, |input, cx| input.set_placeholder(&label, cx));
    }

    /// Guests have no email for card checkout and pay with Bitcoin, as on
    /// the web. Unknown until the account loads, when card stays offered
    /// and the backend still refuses it for a guest.
    fn guest_account(&self) -> bool {
        self.account
            .info
            .as_ref()
            .is_some_and(|account| account.login_method == MapleLoginMethod::Guest)
    }

    pub(super) fn load_credit_balance(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented("api_credit_balance", (&user_id,))
                    .await
            },
            cx,
            |this, result, cx| {
                let credits = &mut this.billing.credits;
                match result {
                    Ok(balance) => {
                        credits.balance = Some(balance);
                        credits.balance_error = None;
                    }
                    Err(message) => credits.balance_error = Some(message.into()),
                }
                cx.notify();
            },
        );
    }

    pub(super) fn credits_targets(&self) -> Vec<SettingsTarget> {
        let target = |target| SettingsTarget::Billing(BillingTarget::Credits(target));
        let mut targets = vec![target(CreditsTarget::Refresh)];
        targets.extend(
            CREDIT_PACKAGES_DOLLARS
                .iter()
                .map(|dollars| target(CreditsTarget::Package(*dollars))),
        );
        targets.push(target(CreditsTarget::Custom));
        if self.billing.credits.choice == CreditChoice::Custom {
            targets.push(target(CreditsTarget::CustomAmount));
        }
        if !self.guest_account() {
            targets.push(target(CreditsTarget::Pay(PaymentMethod::Stripe)));
        }
        targets.push(target(CreditsTarget::Pay(PaymentMethod::Zaprite)));
        targets
    }

    pub(super) fn activate_credits_target(
        &mut self,
        target: CreditsTarget,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            CreditsTarget::Refresh => self.refresh_credit_balance(cx),
            CreditsTarget::Package(dollars) => {
                self.choose_credits(CreditChoice::Package(dollars), cx)
            }
            CreditsTarget::Custom => self.choose_credits(CreditChoice::Custom, cx),
            CreditsTarget::CustomAmount => {
                let handle = self.billing.credits.custom.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            CreditsTarget::Pay(method) => self.purchase_credits(method, cx),
        }
    }

    fn refresh_credit_balance(&mut self, cx: &mut Context<Self>) {
        self.billing.credits.notice = None;
        self.load_credit_balance(cx);
        cx.notify();
    }

    fn choose_credits(&mut self, choice: CreditChoice, cx: &mut Context<Self>) {
        let credits = &mut self.billing.credits;
        if credits.choice == choice {
            return;
        }
        credits.choice = choice;
        credits.notice = None;
        if self.settings.application_vim_enabled {
            self.reconcile_application_vim_target();
        }
        cx.notify();
    }

    pub(super) fn purchase_credits(&mut self, method: PaymentMethod, cx: &mut Context<Self>) {
        if self.billing.credits.purchasing.is_some() {
            return;
        }
        if method == PaymentMethod::Stripe && self.guest_account() {
            return;
        }
        let amount = match self.billing.credits.selected_credits() {
            Ok(amount) => amount,
            Err(message) => {
                self.billing.credits.notice = Some(message);
                cx.notify();
                return;
            }
        };
        self.billing.credits.purchasing = Some(method);
        self.billing.credits.notice = None;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move { backend.call_presented::<()>("purchase_api_credits", (&user_id, method, amount)).await },
            cx,
            move |this, result, cx| {
                this.billing.credits.purchasing = None;
                this.billing.credits.notice = Some(match result {
                    Ok(()) => match method {
                        PaymentMethod::Stripe => UiText::key("settings-api-credits-opened-checkout-in-your-browser-refresh-after-paying-to-see-the-n"),
                        PaymentMethod::Zaprite => UiText::key("settings-api-credits-opened-bitcoin-checkout-in-your-browser-credits-arrive-once-the-p"),
                    },
                    Err(message) => message.into(),
                });
                cx.notify();
            },
        );
    }

    pub(super) fn render_credits(&self, cx: &mut Context<Self>) -> Div {
        let credits = &self.billing.credits;
        let target = |target| move || SettingsTarget::Billing(BillingTarget::Credits(target));
        let mut section = div().flex().flex_col().gap_3().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(section_title(&tr("settings-api-credits-api-credits")))
                .child(
                    self.application_target(
                        target(CreditsTarget::Refresh),
                        widgets::ghost_button("credits-refresh")
                            .py_1p5()
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.refresh_credit_balance(cx);
                            }))
                            .child(tr("settings-api-credits-refresh")),
                    ),
                ),
        );

        // A failed load keeps the last balance, if any, and says so below it.
        let balance = match (credits.balance, &credits.balance_error) {
            (Some(balance), _) => Some(
                self.messages
                    .get(
                        "settings-credits-balance",
                        [("arg1", (format_count(balance)).to_string().into())],
                    )
                    .to_string(),
            ),
            (None, None) => Some(tr("settings-api-credits-loading-balance").to_string()),
            (None, Some(_)) => None,
        };
        section = section.child(
            widgets::card_row()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(tr("settings-api-credits-extra-credit-balance")),
                )
                .when_some(balance, |card, balance| {
                    card.child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(balance),
                    )
                })
                .when_some(credits.balance_error.clone(), |card, message| {
                    card.child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::status_error()))
                            .child(message),
                    )
                })
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(
                            tr("settings-api-credits-1-per-1-000-credits-extends-your-plan-when-its-included-usage-run"),
                        ),
                ),
        );

        let mut packages = div().flex().flex_wrap().gap_2();
        for dollars in CREDIT_PACKAGES_DOLLARS {
            packages = packages.child(
                self.application_target(
                    target(CreditsTarget::Package(dollars)),
                    self.credit_choice_button(
                        CreditChoice::Package(dollars),
                        self.messages
                            .get(
                                "settings-credits-balance",
                                [(
                                    "arg1",
                                    (format_count((dollars * CREDITS_PER_DOLLAR) as i64))
                                        .to_string()
                                        .into(),
                                )],
                            )
                            .to_string(),
                        format!("${dollars}"),
                        cx,
                    ),
                ),
            );
        }
        packages = packages.child(self.application_target(
            target(CreditsTarget::Custom),
            self.credit_choice_button(
                CreditChoice::Custom,
                tr("settings-api-credits-custom-amount").to_string(),
                format!(
                    "${MIN_CREDIT_PURCHASE_DOLLARS} – ${}",
                    format_count(MAX_CREDIT_PURCHASE_DOLLARS as i64)
                ),
                cx,
            ),
        ));
        let mut purchase = widgets::card_row()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(tr("settings-api-credits-buy-credits")),
            )
            .child(packages);
        if credits.choice == CreditChoice::Custom {
            let hint = match &credits.custom_credits {
                None => None,
                Some(Ok(amount)) => Some((
                    self.messages
                        .get(
                            "settings-credits-cost",
                            [
                                ("arg1", (format_count(*amount as i64)).to_string().into()),
                                ("arg2", (amount / CREDITS_PER_DOLLAR).to_string().into()),
                            ],
                        )
                        .to_string(),
                    theme::text_secondary(),
                )),
                Some(Err(error)) => Some((
                    credit_error(*error).resolve().to_string(),
                    theme::status_error(),
                )),
            };
            purchase = purchase.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.application_target(
                        target(CreditsTarget::CustomAmount),
                        widgets::input_frame().child(credits.custom.clone()),
                    ))
                    .when_some(hint, |column, (hint, color)| {
                        column.child(div().text_xs().text_color(gpui::rgb(color)).child(hint))
                    }),
            );
        }

        let ready = credits.purchasing.is_none() && credits.selected_credits().is_ok();
        let guest = self.guest_account();
        let pay_button = |method: PaymentMethod, enabled: bool| {
            let (id, label) = match method {
                PaymentMethod::Stripe => {
                    ("credits-pay-card", tr("settings-api-credits-pay-with-card"))
                }
                PaymentMethod::Zaprite => (
                    "credits-pay-bitcoin",
                    tr("settings-api-credits-pay-with-bitcoin"),
                ),
            };
            let label = if credits.purchasing == Some(method) {
                tr("settings-api-credits-opening-checkout")
            } else {
                label
            };
            let button = match method {
                PaymentMethod::Stripe => widgets::primary_button(id),
                PaymentMethod::Zaprite => widgets::secondary_button(id),
            };
            button
                .flex_1()
                .justify_center()
                .py_1p5()
                .when(!enabled, |el| el.opacity(0.6))
                .when(enabled, |el| {
                    el.on_click(cx.listener(move |this, _event, _window, cx| {
                        this.purchase_credits(method, cx);
                    }))
                })
                .child(label)
        };
        // Guests see card checkout disabled, as on the web.
        let card = pay_button(PaymentMethod::Stripe, ready && !guest).when(guest, |el| {
            el.tooltip(widgets::tooltip(
                tr("settings-api-credits-anonymous-accounts-pay-with-bitcoin"),
                None,
            ))
        });
        let buttons = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().child(
                self.application_target(target(CreditsTarget::Pay(PaymentMethod::Stripe)), card),
            ))
            .child(div().flex_1().child(self.application_target(
                target(CreditsTarget::Pay(PaymentMethod::Zaprite)),
                pay_button(PaymentMethod::Zaprite, ready),
            )));
        purchase = purchase.child(buttons);
        // Feedback goes right under the buttons, which are in view when
        // pressed; below the card it can fall past the window's edge.
        purchase = match &credits.notice {
            Some(notice) => purchase.child(
                widgets::banner(theme::bg_sidebar_pill())
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(notice.clone()),
            ),
            None => purchase.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(if guest {
                        tr("settings-api-credits-checkout-opens-in-your-browser-anonymous-accounts-pay-with-bitcoi")
                    } else {
                        tr("settings-api-credits-checkout-opens-in-your-browser")
                    }),
            ),
        };
        section.child(purchase)
    }

    fn credit_choice_button(
        &self,
        choice: CreditChoice,
        title: String,
        detail: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.billing.credits.choice == choice;
        let id = match choice {
            CreditChoice::Package(dollars) => {
                gpui::SharedString::from(format!("credits-package-{dollars}"))
            }
            CreditChoice::Custom => gpui::SharedString::from("credits-package-custom"),
        };
        div()
            .id(id)
            .min_w(gpui::px(132.))
            .px_3()
            .py_2()
            .rounded(theme::RADIUS_MD)
            .border_1()
            .border_color(gpui::rgb(if selected {
                theme::accent()
            } else {
                theme::border_subtle()
            }))
            .when(!selected, |el| {
                el.hover(|style| {
                    style
                        .border_color(gpui::rgb(theme::border()))
                        .cursor_pointer()
                })
            })
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.choose_credits(choice, cx);
            }))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(title),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(detail),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_reports_credits_or_why_not() {
        assert_eq!(
            selected_credits(CreditChoice::Package(50), &None),
            Ok(50_000)
        );
        assert!(selected_credits(CreditChoice::Custom, &None).is_err());
        assert_eq!(
            selected_credits(CreditChoice::Custom, &Some(Ok(12_000))),
            Ok(12_000)
        );
        assert!(
            selected_credits(
                CreditChoice::Custom,
                &Some(Err(crate::billing::CreditAmountError::AboveMaximum))
            )
            .is_err()
        );
    }

    #[test]
    fn balance_label_covers_loaded_loading_and_error() {
        assert_eq!(
            balance_label(Some(12_345), false, &Default::default()),
            "\u{2068}12,345\u{2069} credits"
        );
        assert_eq!(
            balance_label(None, false, &Default::default()),
            "Loading balance…"
        );
        assert_eq!(
            balance_label(None, true, &Default::default()),
            "Balance unavailable"
        );
    }
}
