//! Sidebar plan card data derived from the Maple billing status. The HTTP
//! client lives in the `maple-billing` crate.

// This module is the desktop frontend's boundary. A headless build (no
// `desktop` feature) uses only a few entry points, so the rest is unused
// there by design.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

pub use maple_billing::{
    BillingClient, BillingError, BillingStatus, CREDITS_PER_DOLLAR, CheckoutRequest,
    CreditPurchaseRequest, MAX_CREDIT_PURCHASE_DOLLARS, MIN_CREDIT_PURCHASE_DOLLARS, PaymentMethod,
    Product,
};

/// Where checkout and the customer portal send the browser afterwards.
/// The desktop app has no URL scheme yet, so these are the web app's
/// pages on the compiled channel's web host.
pub use crate::profile::{CHECKOUT_CANCEL_URL, CHECKOUT_SUCCESS_URL, WEB_URL as PORTAL_RETURN_URL};
/// Where a credit checkout sends the browser: the web app, which shows the
/// confirmation in its API settings. The desktop app needs no callback; the
/// billing service credits the account from the payment provider's webhook.
pub use crate::profile::{CREDITS_CANCEL_URL, CREDITS_SUCCESS_URL};
/// The channel's web pricing page, for plans this app cannot start itself.
pub const PRICING_URL: &str = crate::profile::PRICING_URL;

/// Plan tier derived from the product name, the way the web app gates
/// features (`hasProAccess`, `hasApiAccess`, tier ordering).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum PlanTier {
    Free,
    Pro,
    Max,
    Team,
}

impl PlanTier {
    pub fn from_product_name(name: Option<&str>) -> Self {
        let name = name.unwrap_or_default().to_ascii_lowercase();
        if name.contains("team") {
            Self::Team
        } else if name.contains("max") {
            Self::Max
        } else if name.contains("pro") {
            Self::Pro
        } else {
            Self::Free
        }
    }

    /// API keys and credits need Pro or better (`hasApiAccess` on the web).
    pub fn has_api_access(self) -> bool {
        self >= Self::Pro
    }

    /// Plans the app lists for sale: Pro, Max, and Team, like the web
    /// pricing page. Unknown names read as Free.
    pub fn is_for_sale(self) -> bool {
        self >= Self::Pro
    }
}

/// What one billing reading means for Maple Work, which is not available
/// on the Free plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAccess {
    /// The billing service names the Free plan.
    Blocked,
    /// Any other plan the billing service names, including one this build
    /// does not know.
    Allowed,
    /// No reading: offline, timed out, or an error. OpenSecret enforces
    /// access either way, so a billing outage never locks out a paid plan.
    Unknown,
}

impl PlanAccess {
    pub fn from_status(status: &Result<BillingStatus, String>) -> Self {
        match status {
            Ok(status) if names_free_plan(status) => Self::Blocked,
            Ok(_) => Self::Allowed,
            Err(_) => Self::Unknown,
        }
    }

    /// Whether the plan gate is up after this reading, given whether it
    /// was: only a Free reading raises it, and only a reading of another
    /// plan lowers it.
    pub fn gated(self, was_gated: bool) -> bool {
        match self {
            Self::Blocked => true,
            Self::Allowed => false,
            Self::Unknown => was_gated,
        }
    }
}

/// The billing service always names the Free plan "Free". Unlike
/// [`PlanTier::from_product_name`], a missing or unknown name is not Free
/// here: the gate needs a confirmed Free plan.
fn names_free_plan(status: &BillingStatus) -> bool {
    status
        .product_name
        .as_deref()
        .is_some_and(|name| name.trim().eq_ignore_ascii_case("free"))
}

/// Preset credit purchases in whole dollars, as on the web.
pub const CREDIT_PACKAGES_DOLLARS: [u64; 4] = [20, 50, 100, 500];

/// Credits bought by a whole-dollar amount, or why the amount is refused.
pub fn credits_for_dollars(dollars: u64) -> Result<u64, String> {
    if dollars < MIN_CREDIT_PURCHASE_DOLLARS {
        Err(format!("The minimum is ${MIN_CREDIT_PURCHASE_DOLLARS}"))
    } else if dollars > MAX_CREDIT_PURCHASE_DOLLARS {
        Err(format!(
            "The maximum is ${}",
            format_count(MAX_CREDIT_PURCHASE_DOLLARS as i64)
        ))
    } else {
        Ok(dollars * CREDITS_PER_DOLLAR)
    }
}

/// Whole dollars typed for a custom credit purchase, with an optional `$`.
/// None while the field is empty.
pub fn parse_custom_dollars(text: &str) -> Option<Result<u64, String>> {
    let text = text.trim();
    let digits = text.strip_prefix('$').unwrap_or(text).trim();
    if digits.is_empty() {
        return None;
    }
    Some(if digits.bytes().all(|byte| byte.is_ascii_digit()) {
        // Too many digits to parse is over the maximum either way.
        credits_for_dollars(digits.parse().unwrap_or(u64::MAX))
    } else {
        Err("Enter a whole-dollar amount".to_string())
    })
}

/// Plan-level readings of a billing status. An extension trait because
/// the status type lives in the `maple-billing` crate.
pub trait BillingStatusExt {
    fn tier(&self) -> PlanTier;
    fn has_portal(&self) -> bool;
    fn period_end_label(&self) -> Option<String>;
}

impl BillingStatusExt for BillingStatus {
    fn tier(&self) -> PlanTier {
        PlanTier::from_product_name(self.product_name.as_deref())
    }

    /// Whether the Stripe customer portal can manage this subscription:
    /// paid through Stripe with a customer record. Bitcoin and pass
    /// subscriptions have no portal.
    fn has_portal(&self) -> bool {
        self.stripe_customer_id.is_some()
            && self.tier() >= PlanTier::Pro
            && !matches!(
                self.payment_provider.as_deref(),
                Some("zaprite") | Some("subscription_pass")
            )
    }

    /// "Renews September 30, 2026" or "Expires …" for passes and Bitcoin
    /// subscriptions, which do not auto-renew; None without a period end.
    fn period_end_label(&self) -> Option<String> {
        let end = chrono::DateTime::from_timestamp(self.current_period_end?, 0)?
            .with_timezone(&chrono::Local)
            .format("%B %-d, %Y");
        let verb = match self.payment_provider.as_deref() {
            Some("zaprite") | Some("subscription_pass") => "Expires",
            _ => "Renews",
        };
        Some(format!("{verb} {end}"))
    }
}

/// "100,000" with thousands separators, for exact balances.
pub fn format_count(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if value < 0 {
        out.insert(0, '-');
    }
    out
}

/// "$20/month" from a product's default price; None when it has no price.
pub fn price_label(product: &Product) -> Option<String> {
    let price = product.default_price.as_ref()?;
    let amount = if price.currency.eq_ignore_ascii_case("usd") {
        if price.unit_amount % 100 == 0 {
            format!("${}", price.unit_amount / 100)
        } else {
            format!("${:.2}", price.unit_amount as f64 / 100.0)
        }
    } else {
        format!(
            "{:.2} {}",
            price.unit_amount as f64 / 100.0,
            price.currency.to_ascii_uppercase()
        )
    };
    Some(
        match price.recurring.as_ref().map(|r| r.interval.as_str()) {
            Some(interval) => format!("{amount}/{interval}"),
            None => amount,
        },
    )
}

/// Billing API base URL. Local builds allow `MAPLE_BILLING_API_URL`;
/// packaged builds use the compiled channel's public service.
pub fn configured_billing_api_url() -> String {
    let configured =
        crate::profile::runtime_value("MAPLE_BILLING_API_URL", crate::profile::BILLING_API_URL);
    let configured = configured.trim_end_matches('/');
    if configured.is_empty() {
        crate::profile::BILLING_API_URL.to_string()
    } else {
        configured.to_string()
    }
}

/// Plan quota shown in the sidebar footer: plan name, percent of the
/// quota used in the current period, and the date the period resets.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanUsage {
    pub plan_label: String,
    pub percent_used: u8,
    pub resets_label: String,
}

impl PlanUsage {
    /// Build the card data the same way the Maple web sidebar does. Returns
    /// `None` when the status has no usage meter.
    pub fn from_status(
        status: &BillingStatus,
        now: chrono::DateTime<chrono::Local>,
    ) -> Option<Self> {
        let total = status.total_tokens.filter(|total| *total > 0)?;
        let used = status.used_tokens?.max(0);
        let percent = ((used as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
        Some(Self {
            plan_label: plan_name_label(status.product_name.as_deref()),
            percent_used: percent.round() as u8,
            resets_label: reset_label(status.usage_reset_date.as_deref(), now),
        })
    }
}

fn plan_name_label(raw: Option<&str>) -> String {
    let cleaned = raw
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("Pro");
    let has_plan_suffix = cleaned
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.eq_ignore_ascii_case("plan"));
    if has_plan_suffix {
        cleaned.to_string()
    } else {
        format!("{cleaned} Plan")
    }
}

/// Text after "Resets" in the card, matching `formatResetDate` in the web app.
fn reset_label(iso: Option<&str>, now: chrono::DateTime<chrono::Local>) -> String {
    let Some(reset) = iso
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&chrono::Local))
    else {
        return "Monthly".to_string();
    };
    let remaining = reset - now;
    if remaining < chrono::Duration::zero() {
        return "Monthly".to_string();
    }
    let days = (reset.date_naive() - now.date_naive()).num_days();
    match days {
        0 => match remaining.num_hours() {
            0 => "in less than 1 hour".to_string(),
            1 => "in 1 hour".to_string(),
            hours => format!("in {hours} hours"),
        },
        1 => "Tomorrow".to_string(),
        2..=7 => format!("in {days} days"),
        _ => reset.format("%b %-d").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_billing_destinations_use_the_compiled_web_profile() {
        assert_eq!(PORTAL_RETURN_URL, crate::profile::WEB_URL);
        assert_eq!(PRICING_URL, format!("{PORTAL_RETURN_URL}/pricing"));
        assert_eq!(CHECKOUT_SUCCESS_URL, format!("{PRICING_URL}?success=true"));
        assert_eq!(CHECKOUT_CANCEL_URL, format!("{PRICING_URL}?canceled=true"));
        if crate::profile::PROFILE == "dev" {
            assert_eq!(PORTAL_RETURN_URL, "https://app-dev.maple.ai");
        } else {
            assert_eq!(PORTAL_RETURN_URL, "https://app.maple.ai");
        }
    }

    #[test]
    fn credit_purchases_return_to_the_web_app() {
        assert_eq!(
            CREDITS_SUCCESS_URL,
            format!("{PORTAL_RETURN_URL}/?credits_success=true")
        );
        assert_eq!(CREDITS_CANCEL_URL, format!("{PORTAL_RETURN_URL}/"));
    }

    #[test]
    fn credit_amounts_are_whole_dollars_in_range() {
        assert_eq!(credits_for_dollars(20), Ok(20_000));
        assert_eq!(credits_for_dollars(1_000), Ok(1_000_000));
        assert!(credits_for_dollars(9).is_err());
        assert!(credits_for_dollars(1_001).is_err());
        assert_eq!(parse_custom_dollars("  "), None);
        assert_eq!(parse_custom_dollars("$ 25"), Some(Ok(25_000)));
        assert_eq!(parse_custom_dollars("10"), Some(Ok(10_000)));
        assert!(matches!(parse_custom_dollars("12.5"), Some(Err(_))));
        assert!(matches!(parse_custom_dollars("-5"), Some(Err(_))));
        assert!(matches!(
            parse_custom_dollars("99999999999999999999999"),
            Some(Err(_))
        ));
        for dollars in CREDIT_PACKAGES_DOLLARS {
            assert!(credits_for_dollars(dollars).is_ok());
        }
    }

    #[test]
    fn only_pro_max_and_team_are_for_sale() {
        for (name, for_sale) in [
            ("Free", false),
            ("Unexpected", false),
            ("Pro", true),
            ("Max", true),
            ("Team", true),
        ] {
            assert_eq!(
                PlanTier::from_product_name(Some(name)).is_for_sale(),
                for_sale,
                "{name}"
            );
        }
    }

    fn reading(product: Option<&str>) -> Result<BillingStatus, String> {
        Ok(BillingStatus {
            is_subscribed: true,
            product_name: product.map(str::to_string),
            ..BillingStatus::default()
        })
    }

    #[test]
    fn only_a_free_reading_blocks_maple_work() {
        // Guests start on Free and get the same reading as any account.
        for name in ["Free", "free", " Free "] {
            assert_eq!(
                PlanAccess::from_status(&reading(Some(name))),
                PlanAccess::Blocked,
                "{name}"
            );
        }
        for name in ["Pro", "Max", "Team", "Team Plan", "Unexpected"] {
            assert_eq!(
                PlanAccess::from_status(&reading(Some(name))),
                PlanAccess::Allowed,
                "{name}"
            );
        }
        // No plan name is not a confirmed Free plan.
        assert_eq!(PlanAccess::from_status(&reading(None)), PlanAccess::Allowed);
        assert_eq!(
            PlanAccess::from_status(&Err("billing request timed out".into())),
            PlanAccess::Unknown
        );
    }

    #[test]
    fn the_gate_moves_only_on_a_definite_reading() {
        let free = PlanAccess::from_status(&reading(Some("Free")));
        let pro = PlanAccess::from_status(&reading(Some("Pro")));
        let offline = PlanAccess::from_status(&Err("offline".into()));
        assert!(free.gated(false));
        assert!(free.gated(true));
        assert!(!pro.gated(true));
        assert!(!pro.gated(false));
        // An error never blocks, and never reopens a gated account either.
        assert!(!offline.gated(false));
        assert!(offline.gated(true));
    }

    #[test]
    fn tiers_come_from_the_product_name() {
        assert_eq!(PlanTier::from_product_name(None), PlanTier::Free);
        assert_eq!(
            PlanTier::from_product_name(Some("Maple Pro")),
            PlanTier::Pro
        );
        assert_eq!(PlanTier::from_product_name(Some("Max Plan")), PlanTier::Max);
        assert_eq!(PlanTier::from_product_name(Some("Team")), PlanTier::Team);
        assert!(PlanTier::Pro.has_api_access());
        assert!(!PlanTier::Free.has_api_access());
    }

    #[test]
    fn portal_needs_a_stripe_customer_on_a_paid_plan() {
        let mut status = BillingStatus {
            product_name: Some("Pro".into()),
            stripe_customer_id: Some("cus".into()),
            payment_provider: Some("stripe".into()),
            ..BillingStatus::default()
        };
        assert!(status.has_portal());
        status.payment_provider = Some("zaprite".into());
        assert!(!status.has_portal());
        status.payment_provider = Some("stripe".into());
        status.product_name = Some("Free".into());
        assert!(!status.has_portal());
    }

    #[test]
    fn period_end_label_names_renewal_or_expiry() {
        let mut status = BillingStatus {
            current_period_end: Some(1_790_000_000),
            ..BillingStatus::default()
        };
        assert!(status.period_end_label().unwrap().starts_with("Renews "));
        status.payment_provider = Some("subscription_pass".into());
        assert!(status.period_end_label().unwrap().starts_with("Expires "));
        status.current_period_end = None;
        assert_eq!(status.period_end_label(), None);
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(100_000), "100,000");
        assert_eq!(format_count(-1_234_567), "-1,234,567");
    }

    #[test]
    fn price_labels_are_compact() {
        let product = |amount, interval: Option<&str>| Product {
            id: "p".into(),
            name: "Pro".into(),
            description: None,
            active: true,
            default_price: Some(maple_billing::Price {
                id: "price".into(),
                currency: "usd".into(),
                unit_amount: amount,
                recurring: interval.map(|interval| maple_billing::Recurring {
                    interval: interval.into(),
                    interval_count: None,
                }),
            }),
            is_available: None,
        };
        assert_eq!(
            price_label(&product(2000, Some("month"))).unwrap(),
            "$20/month"
        );
        assert_eq!(price_label(&product(1999, None)).unwrap(), "$19.99");
    }
    use chrono::TimeZone;

    fn now() -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2026, 8, 26, 12, 0, 0)
            .unwrap()
    }

    fn status(product: &str, used: i64, total: i64, reset: &str) -> BillingStatus {
        BillingStatus {
            product_name: Some(product.to_string()),
            total_tokens: Some(total),
            used_tokens: Some(used),
            usage_reset_date: Some(reset.to_string()),
            ..BillingStatus::default()
        }
    }

    #[test]
    fn builds_card_from_status() {
        let reset = (now() + chrono::Duration::days(11)).to_rfc3339();
        let plan = PlanUsage::from_status(&status("Max", 17, 100, &reset), now()).unwrap();
        assert_eq!(plan.plan_label, "Max Plan");
        assert_eq!(plan.percent_used, 17);
        assert_eq!(plan.resets_label, "Sep 6");
    }

    #[test]
    fn keeps_existing_plan_suffix_and_clamps() {
        let plan =
            PlanUsage::from_status(&status("Pro Plan", 500, 100, "2026-08-28T12:00:00Z"), now())
                .unwrap();
        assert_eq!(plan.plan_label, "Pro Plan");
        assert_eq!(plan.percent_used, 100);
        assert_eq!(plan.resets_label, "in 2 days");
    }

    #[test]
    fn relative_reset_labels() {
        let label =
            |reset: chrono::DateTime<chrono::Local>| reset_label(Some(&reset.to_rfc3339()), now());
        assert_eq!(
            label(now() + chrono::Duration::minutes(30)),
            "in less than 1 hour"
        );
        assert_eq!(label(now() + chrono::Duration::hours(1)), "in 1 hour");
        assert_eq!(label(now() + chrono::Duration::hours(3)), "in 3 hours");
        assert_eq!(label(now() + chrono::Duration::days(1)), "Tomorrow");
        assert_eq!(label(now() + chrono::Duration::days(5)), "in 5 days");
        assert_eq!(label(now() + chrono::Duration::days(20)), "Sep 15");
        assert_eq!(label(now() - chrono::Duration::days(1)), "Monthly");
        assert_eq!(reset_label(None, now()), "Monthly");
        assert_eq!(reset_label(Some("garbage"), now()), "Monthly");
    }

    #[test]
    fn no_meter_without_totals() {
        let mut s = status("Max", 1, 0, "2026-09-06T00:00:00Z");
        assert!(PlanUsage::from_status(&s, now()).is_none());
        s.total_tokens = Some(10);
        s.used_tokens = None;
        assert!(PlanUsage::from_status(&s, now()).is_none());
    }
}
