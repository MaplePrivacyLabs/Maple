//! Sidebar plan card data derived from the Maple billing status. The HTTP
//! client lives in the `maple-billing` crate.

// This module is the desktop frontend's boundary. A headless build (no
// `desktop` feature) uses only a few entry points, so the rest is unused
// there by design.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

pub use maple_billing::{BillingClient, BillingError, BillingStatus, CheckoutRequest, Product};

/// Where checkout and the customer portal send the browser afterwards.
/// The desktop app has no URL scheme yet, so these are the web app's
/// pages, the same targets the Tauri build uses.
pub const PORTAL_RETURN_URL: &str = "https://trymaple.ai";
pub const CHECKOUT_SUCCESS_URL: &str = "https://trymaple.ai/pricing?success=true";
pub const CHECKOUT_CANCEL_URL: &str = "https://trymaple.ai/pricing?canceled=true";
/// The web pricing page, for plans this app cannot start itself.
pub const PRICING_URL: &str = "https://trymaple.ai/pricing";

/// Plan tier derived from the product name, the way the web app gates
/// features (`hasProAccess`, `hasApiAccess`, tier ordering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlanTier {
    Free,
    Starter,
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
        } else if name.contains("starter") {
            Self::Starter
        } else {
            Self::Free
        }
    }

    /// API keys and credits need Pro or better (`hasApiAccess` on the web).
    pub fn has_api_access(self) -> bool {
        self >= Self::Pro
    }
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
#[derive(Debug, Clone, PartialEq)]
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
    fn tiers_come_from_the_product_name() {
        assert_eq!(PlanTier::from_product_name(None), PlanTier::Free);
        assert_eq!(
            PlanTier::from_product_name(Some("Maple Pro")),
            PlanTier::Pro
        );
        assert_eq!(PlanTier::from_product_name(Some("Max Plan")), PlanTier::Max);
        assert_eq!(PlanTier::from_product_name(Some("Team")), PlanTier::Team);
        assert!(PlanTier::Pro.has_api_access());
        assert!(!PlanTier::Starter.has_api_access());
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
