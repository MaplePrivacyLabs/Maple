//! UI-owned descriptions of semantic runtime states. Raw host errors and custom
//! integration metadata remain untouched.
use crate::ui::i18n::{MessageCache, tr};
use gpui::SharedString;

pub(crate) fn update_label(update: &crate::update::Snapshot, messages: &MessageCache) -> String {
    use crate::update::State;
    let key = match update.state {
        State::Disabled => "settings-update-disabled",
        State::Idle => "settings-update-idle",
        State::Checking => "settings-update-checking",
        State::Current => "settings-update-current",
        State::Available => "settings-update-available",
        State::Downloading => "settings-update-downloading",
        State::Ready => "settings-update-ready",
        State::Installing => "settings-update-installing",
        State::RestartRequired => "settings-update-restart-required",
        State::Error => return update.label.clone(),
    };
    let label = tr(key);
    match update.version.as_ref() {
        Some(version) => messages
            .get(
                "settings-update-version-status",
                [
                    ("status", label.to_string().into()),
                    ("version", version.clone().into()),
                ],
            )
            .to_string(),
        None => label.to_string(),
    }
}

pub(super) fn update_action(action: crate::update::UpdateAction) -> SharedString {
    use crate::update::UpdateAction;
    tr(match action {
        UpdateAction::Check => "settings-settings-check-for-updates",
        UpdateAction::Download | UpdateAction::ManualDownload => "settings-update-download",
        UpdateAction::Restart => "settings-update-restart",
        UpdateAction::Relaunch => "settings-update-relaunch",
        UpdateAction::Later => "settings-settings-later",
        UpdateAction::AutomaticDownloads(_) => "settings-settings-automatically-download-updates",
    })
}

pub(super) fn login_method(method: crate::backend::MapleLoginMethod) -> String {
    use crate::backend::MapleLoginMethod;
    match method {
        MapleLoginMethod::Email => tr("settings-account-email").to_string(),
        MapleLoginMethod::Guest => tr("settings-account-anonymous").to_string(),
        other => other.label().to_string(),
    }
}

pub(super) fn permission(kind: maple_agent::agent::AgentIntegrationPermissionKind) -> SharedString {
    use maple_agent::agent::AgentIntegrationPermissionKind;
    tr(match kind {
        AgentIntegrationPermissionKind::Accessibility => "settings-permission-accessibility",
        AgentIntegrationPermissionKind::ScreenRecording => "settings-permission-screen-recording",
        AgentIntegrationPermissionKind::DesktopHelper => "settings-permission-desktop-helper",
    })
}

pub(super) fn integration_description(id: &str, fallback: &str) -> SharedString {
    match id {
        "cua-driver" => tr("settings-integration-cua-description"),
        "codex" => tr("settings-integration-codex-description"),
        "claude" => tr("settings-integration-claude-description"),
        _ => fallback.to_string().into(),
    }
}

pub(super) fn date(date: chrono::NaiveDate, messages: &MessageCache) -> SharedString {
    use chrono::Datelike;
    let month = tr(match date.month() {
        1 => "settings-month-january",
        2 => "settings-month-february",
        3 => "settings-month-march",
        4 => "settings-month-april",
        5 => "settings-month-may",
        6 => "settings-month-june",
        7 => "settings-month-july",
        8 => "settings-month-august",
        9 => "settings-month-september",
        10 => "settings-month-october",
        11 => "settings-month-november",
        _ => "settings-month-december",
    });
    messages.get(
        "settings-date",
        [
            ("month", month.to_string().into()),
            ("day", date.day().to_string().into()),
            ("year", date.year().to_string().into()),
        ],
    )
}

pub(super) fn period_end(
    status: &crate::billing::BillingStatus,
    messages: &MessageCache,
) -> Option<SharedString> {
    let end = chrono::DateTime::from_timestamp(status.current_period_end?, 0)?
        .with_timezone(&chrono::Local);
    let key = match status.payment_provider.as_deref() {
        Some("zaprite" | "subscription_pass") => "settings-billing-expires",
        _ => "settings-billing-renews",
    };
    Some(messages.get(
        key,
        [("date", date(end.date_naive(), messages).to_string().into())],
    ))
}

pub(super) fn price(
    product: &crate::billing::Product,
    messages: &MessageCache,
) -> Option<SharedString> {
    let price = product.default_price.as_ref()?;
    let original = crate::billing::price_label(product)?;
    let Some(recurring) = &price.recurring else {
        return Some(original.into());
    };
    let period = match recurring.interval.as_str() {
        "month" => tr("settings-billing-month"),
        "year" => tr("settings-billing-year"),
        "week" => tr("settings-billing-week"),
        "day" => tr("settings-billing-day"),
        _ => return Some(original.into()),
    };
    let amount = original
        .rsplit_once('/')
        .map_or(original.as_str(), |(amount, _)| amount);
    Some(messages.get(
        "settings-billing-price-period",
        [
            ("amount", amount.to_string().into()),
            ("period", period.to_string().into()),
        ],
    ))
}

pub(crate) fn plan_name(plan: &crate::billing::PlanUsage, messages: &MessageCache) -> SharedString {
    let Some(name) = &plan.product_name else {
        return plan.plan_label.clone().into();
    };
    if name
        .split(|ch: char| !ch.is_alphanumeric())
        .any(|word| word.eq_ignore_ascii_case("plan"))
    {
        return name.clone().into();
    }
    messages.get("settings-plan-name", [("name", name.clone().into())])
}

pub(crate) fn reset_date(
    plan: &crate::billing::PlanUsage,
    messages: &MessageCache,
) -> SharedString {
    let now = chrono::Local::now();
    let fingerprint = format!(
        "{}:{}",
        plan.reset_at.as_deref().unwrap_or_default(),
        now.timestamp() / 60
    );
    messages.computed("settings-reset-date", &fingerprint, || {
        let Some(reset) = plan
            .reset_at
            .as_deref()
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|date| date.with_timezone(&chrono::Local))
        else {
            return tr("settings-reset-monthly");
        };
        let remaining = reset - now;
        if remaining < chrono::Duration::zero() {
            return tr("settings-reset-monthly");
        }
        let days = (reset.date_naive() - now.date_naive()).num_days();
        match days {
            0 if remaining.num_hours() == 0 => tr("settings-reset-soon"),
            0 => messages.get(
                "settings-reset-hours",
                [("count", remaining.num_hours().into())],
            ),
            1 => tr("settings-reset-tomorrow"),
            2..=7 => messages.get("settings-reset-days", [("count", days.into())]),
            _ => date(reset.date_naive(), messages),
        }
    })
}

/// Missing CLI hints are owned by Maple, while detected tool problems stay raw.
pub(super) fn integration_detail(
    integration: &maple_agent::agent::AgentIntegration,
    fallback: &str,
) -> SharedString {
    use maple_agent::agent::AgentIntegrationAvailability;
    if integration.availability == AgentIntegrationAvailability::NotDetected {
        match integration.id.as_str() {
            "codex" => return tr("settings-integration-install-codex"),
            "claude" => return tr("settings-integration-install-claude"),
            _ => {}
        }
    }
    fallback.to_owned().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn billing_period_follows_language_and_payment_provider() {
        let _locale = crate::ui::i18n::TestLocalizationGuard::enable();
        let messages = MessageCache::default();
        let mut status = crate::billing::BillingStatus {
            current_period_end: Some(1_790_000_000),
            ..Default::default()
        };
        for (locale, renews, expires) in [
            ("en", "Renews ", "Expires "),
            ("es", "Se renueva el ", "Vence el "),
            ("en", "Renews ", "Expires "),
        ] {
            crate::ui::i18n::apply_preference(Some(locale));
            status.payment_provider = Some("stripe".into());
            assert!(period_end(&status, &messages).unwrap().starts_with(renews));
            status.payment_provider = Some("subscription_pass".into());
            assert!(period_end(&status, &messages).unwrap().starts_with(expires));
        }
        status.current_period_end = None;
        assert!(period_end(&status, &messages).is_none());
    }
}
