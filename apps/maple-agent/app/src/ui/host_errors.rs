//! Localize explicit host codes; unrecognized/legacy diagnostics stay verbatim.
use super::i18n::UiText;
use crate::backend::HostFailure;

pub(crate) fn failure_text(failure: &HostFailure) -> UiText {
    if failure.code == "key-memory" {
        return match failure
            .args
            .get("required_mib")
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(value) => {
                UiText::message("runtime-error-key-memory", [("required_mib", value.into())])
            }
            None => UiText::Raw(failure.message.clone()),
        };
    }
    let argument = match failure.code.as_str() {
        "key-name-length" => Some(("runtime-error-key-name-length", "count")),
        "account-password-length" => Some(("runtime-error-account-password-length", "count")),
        "credit-minimum" => Some(("runtime-error-credit-minimum", "amount")),
        "credit-maximum" => Some(("runtime-error-credit-maximum", "amount")),
        _ => None,
    };
    if let Some((key, name)) = argument {
        return match failure
            .args
            .get(name)
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(value) => UiText::message(key, [(name, value.into())]),
            None => UiText::Raw(failure.message.clone()),
        };
    }
    let key = match failure.code.as_str() {
        "settings-sync-unavailable" => "chat-settings-sync-unavailable",
        "settings-sync-timeout" => "chat-settings-sync-timeout",
        "sidebar-sync-state-changed" => "chat-sidebar-sync-state-changed",
        "sidebar-sync-timeout" => "chat-sidebar-sync-timeout",
        "settings-sync-stopped" => "chat-sidebar-order-save-failed",

        "email-required" => "runtime-error-email-required",
        "password-required" => "runtime-error-password-required",
        "account-id-required" => "runtime-error-account-id-required",
        "account-id-invalid" => "runtime-error-account-id",
        "api-authentication" => "runtime-error-api-authentication",
        "login-failed" => "runtime-error-login",
        "guest-login-failed" => "runtime-error-guest-login",
        "store-locked" => "runtime-error-store-locked",
        "store-setup" => "runtime-error-store-setup",
        "store-busy" => "runtime-error-store-busy",
        "store-damaged" => "runtime-error-store-damaged",
        "host-disconnected" => "runtime-error-host-disconnected",
        "host-disconnected-current" => "runtime-error-host-disconnected-current",
        "host-disconnected-before-send" => "runtime-error-host-disconnected-before-send",
        "host-disconnected-retry" => "runtime-error-host-disconnected-retry",
        "host-updated" => "runtime-error-host-updated",
        "local-desktop-required" => "runtime-error-local-desktop",
        "remote-permissions-unsupported" => "runtime-error-remote-permissions-unsupported",
        "remote-permissions-unconfirmed" => "runtime-error-remote-permissions-unconfirmed",
        "remote-tools-unsupported" => "runtime-error-remote-tools-unsupported",
        "remote-tools-unconfirmed" => "runtime-error-remote-tools-unconfirmed",
        "request-encoding" => "runtime-error-request-encoding",
        "request-parameters" => "runtime-error-request-parameters",
        "response-encoding" => "runtime-error-response-encoding",
        "updater-response-encoding" => "runtime-error-updater-response-encoding",
        "key-damaged" => "runtime-error-key-damaged",
        "key-mismatch" => "runtime-error-key-mismatch",
        "key-wrong-password" => "runtime-error-key-wrong-password",
        "key-invalid-password" => "runtime-error-key-invalid-password",
        "key-weak-password" => "runtime-error-key-weak-password",
        "key-memory-lock" => "runtime-error-key-memory-lock",
        "key-random" => "runtime-error-key-random",
        "key-unsafe-path" => "runtime-error-key-unsafe-path",
        "key-io" => "runtime-error-key-io",
        "key-sync-pending" => "runtime-error-key-sync-pending",
        "lifecycle-cancelled" => "runtime-error-lifecycle-cancelled",
        "lifecycle-confirmation" => "runtime-error-lifecycle-confirmation",
        "lifecycle-space" => "runtime-error-lifecycle-space",
        "lifecycle-verification" => "runtime-error-lifecycle-verification",
        "lifecycle-generation" => "runtime-error-lifecycle-generation",
        "lifecycle-recovery" => "runtime-error-lifecycle-recovery",
        "storage-locked" => "runtime-error-storage-locked",
        "storage-setup" => "runtime-error-storage-setup",
        "storage-busy" => "runtime-error-storage-busy",
        "storage-disk-full" => "runtime-error-storage-disk-full",
        "storage-damaged" => "runtime-error-storage-damaged",
        "storage-not-found" => "runtime-error-storage-not-found",
        "storage-conflict" => "runtime-error-storage-conflict",
        "storage-epoch" => "runtime-error-storage-epoch",
        "storage-input" => "runtime-error-storage-input",
        "storage-data" => "runtime-error-storage-data",
        "storage-unsafe-path" => "runtime-error-storage-unsafe-path",
        "storage-filesystem" => "runtime-error-storage-filesystem",
        "storage-schema" => "runtime-error-storage-schema",
        "storage-newer" => "runtime-error-storage-newer",
        "storage-epoch-schema" => "runtime-error-storage-epoch-schema",
        "storage-database" => "runtime-error-storage-database",
        "storage-io" => "runtime-error-storage-io",
        "refusal-newer" => "runtime-error-refusal-newer",
        "refusal-schema" => "runtime-error-refusal-schema",
        "refusal-filesystem" => "runtime-error-refusal-filesystem",
        "refusal-path" => "runtime-error-refusal-path",
        "refusal-operation" => "runtime-error-refusal-operation",
        "refusal-unavailable" => "runtime-error-refusal-unavailable",
        "passwords-already-unlocked" => "runtime-error-passwords-already-unlocked",
        "passwords-unlock-validation" => "runtime-error-passwords-unlock-validation",
        "passwords-reset-unknown" => "runtime-error-passwords-reset-unknown",
        "passwords-reset-worker" => "runtime-error-passwords-reset-worker",
        "passwords-request" => "runtime-error-passwords-request",
        "passwords-response" => "runtime-error-passwords-response",
        "passwords-setup-ack" => "runtime-error-passwords-setup-ack",
        "passwords-calibration" => "runtime-error-passwords-calibration",
        "passwords-generator" => "runtime-error-passwords-generator",
        "passwords-expired" => "runtime-error-passwords-expired",
        "passwords-generate-new" => "runtime-error-passwords-generate-new",
        "passwords-unlock-worker" => "runtime-error-passwords-unlock-worker",
        "passwords-change-worker" => "runtime-error-passwords-change-worker",
        "passwords-remove-worker" => "runtime-error-passwords-remove-worker",
        "passwords-setup-worker" => "runtime-error-passwords-setup-worker",
        "passwords-operation" => "runtime-error-passwords-operation",
        "repair-request" => "runtime-error-repair-request",
        "repair-response" => "runtime-error-repair-response",
        "repair-preview" => "runtime-error-repair-preview",
        "repair-damaged" => "runtime-error-repair-damaged",
        "repair-unavailable" => "runtime-error-repair-unavailable",
        "repair-busy" => "runtime-error-repair-busy",
        "repair-review" => "runtime-error-repair-review",
        "repair-native" => "runtime-error-repair-native",
        "repair-large" => "runtime-error-repair-large",
        "repair-operation" => "runtime-error-repair-operation",
        "repair-worker" => "runtime-error-repair-worker",
        "repair-page" => "runtime-error-repair-page",
        "repair-confirmation" => "runtime-error-repair-confirmation",
        "folder-browser-worker" => "runtime-error-folder-browser-worker",
        "folder-create-worker" => "runtime-error-folder-create-worker",
        "folder-unicode" => "runtime-error-folder-unicode",
        "folder-long" => "runtime-error-folder-long",
        "folder-path" => "runtime-error-folder-path",
        "folder-absolute" => "runtime-error-folder-absolute",
        "folder-missing" => "runtime-error-folder-missing",
        "folder-permission" => "runtime-error-folder-permission",
        "folder-exists" => "runtime-error-folder-exists",
        "folder-directory" => "runtime-error-folder-directory",
        "folder-access" => "runtime-error-folder-access",
        "folder-home" => "runtime-error-folder-home",
        "folder-cursor" => "runtime-error-folder-cursor",
        "folder-encode" => "runtime-error-folder-encode",
        "folder-large" => "runtime-error-folder-large",
        "folder-many" => "runtime-error-folder-many",
        "folder-name" => "runtime-error-folder-name",
        "attachment-open" => "runtime-error-attachment-open",
        "attachment-inspect" => "runtime-error-attachment-inspect",
        "attachment-regular" => "runtime-error-attachment-regular",
        "attachment-read" => "runtime-error-attachment-read",
        "attachment-large" => "runtime-error-attachment-large",
        "attachment-changed" => "runtime-error-attachment-changed",
        "attachment-worker" => "runtime-error-attachment-worker",
        "attachment-store" => "runtime-error-attachment-store",
        "attachment-request" => "runtime-error-attachment-request",
        "attachment-staged-read" => "runtime-error-attachment-staged-read",
        "attachment-encode" => "runtime-error-attachment-encode",
        "attachment-operation-request" => "runtime-error-attachment-operation-request",
        "attachment-operation" => "runtime-error-attachment-operation",
        "attachment-operation-encode" => "runtime-error-attachment-operation-encode",
        "paging-store" => "runtime-error-paging-store",
        "paging-worker" => "runtime-error-paging-worker",
        "paging-request" => "runtime-error-paging-request",
        "paging-encode" => "runtime-error-paging-encode",
        "paging-timestamp" => "runtime-error-paging-timestamp",
        "paging-queue" => "runtime-error-paging-queue",
        "paging-operation" => "runtime-error-paging-operation",
        "paging-navigation-budget" => "runtime-error-paging-navigation-budget",
        "paging-open-budget" => "runtime-error-paging-open-budget",
        "paging-row-limit" => "runtime-error-paging-row-limit",
        "paging-reply-budget" => "runtime-error-paging-reply-budget",
        "paging-changed" => "runtime-error-paging-changed",
        "paging-queue-budget" => "runtime-error-paging-queue-budget",
        "paging-bounded-encode" => "runtime-error-paging-bounded-encode",
        "mcp-no-address" => "runtime-error-mcp-no-address",
        "mcp-invalid-address" => "runtime-error-mcp-invalid-address",
        "mcp-unreachable" => "runtime-error-mcp-unreachable",
        "mcp-no-server" => "runtime-error-mcp-no-server",
        "deletion-prepare" => "runtime-error-deletion-prepare",
        "deletion-credentials" => "runtime-error-deletion-credentials",
        "deletion-status" => "runtime-error-deletion-status",
        "deletion-code" => "runtime-error-deletion-code",
        "deletion-invalid" => "runtime-error-deletion-invalid",
        "deletion-store" => "runtime-error-deletion-store",
        "deletion-spill" => "runtime-error-deletion-spill",
        "deletion-account-changed" => "runtime-error-deletion-account-changed",
        "deletion-save-intent" => "runtime-error-deletion-save-intent",
        "deletion-read-intent" => "runtime-error-deletion-read-intent",
        "deletion-read-credentials" => "runtime-error-deletion-read-credentials",
        "deletion-confirmation-save" => "runtime-error-deletion-confirmation-save",
        "deletion-interrupted" => "runtime-error-deletion-interrupted",
        "deletion-pending" => "runtime-error-deletion-pending",
        "deletion-wrong-code" => "runtime-error-deletion-wrong-code",
        "host-running-tasks" => "runtime-error-host-running-tasks",
        "host-stopping" => "runtime-error-host-stopping",
        "account-load-status" => "runtime-error-account-load-status",
        "account-load-connection" => "runtime-error-account-load-connection",
        "verification-status" => "runtime-error-verification-status",
        "verification-connection" => "runtime-error-verification-connection",
        "password-change-status" => "runtime-error-password-change-status",
        "password-change-connection" => "runtime-error-password-change-connection",
        "deletion-start-status" => "runtime-error-deletion-start-status",
        "deletion-start-connection" => "runtime-error-deletion-start-connection",
        "keys-load-status" => "runtime-error-keys-load-status",
        "keys-load-connection" => "runtime-error-keys-load-connection",
        "key-create-status" => "runtime-error-key-create-status",
        "key-create-connection" => "runtime-error-key-create-connection",
        "key-delete-status" => "runtime-error-key-delete-status",
        "key-delete-connection" => "runtime-error-key-delete-connection",
        "account-current-required" => "runtime-error-account-current-required",
        "account-current-wrong" => "runtime-error-account-current-wrong",
        "billing-balance" => "runtime-error-billing-balance",
        "billing-whole" => "runtime-error-billing-whole",
        "billing-guest" => "runtime-error-billing-guest",
        "billing-portal" => "runtime-error-billing-portal",
        "billing-checkout" => "runtime-error-billing-checkout",
        "billing-credit-checkout" => "runtime-error-billing-credit-checkout",
        "key-plan" => "runtime-error-key-plan",
        "key-exists" => "runtime-error-key-exists",
        "key-name-invalid" => "runtime-error-key-name-invalid",
        "key-missing" => "runtime-error-key-missing",
        "key-limit" => "runtime-error-key-limit",
        "browser-portal-link" => "runtime-error-browser-portal-link",
        "browser-portal-open" => "runtime-error-browser-portal-open",
        "browser-checkout-link" => "runtime-error-browser-checkout-link",
        "browser-checkout-open" => "runtime-error-browser-checkout-open",
        "browser-pricing-link" => "runtime-error-browser-pricing-link",
        "browser-pricing-open" => "runtime-error-browser-pricing-open",
        "billing-timeout" => "runtime-error-billing-timeout",
        "key-name-required" => "runtime-error-key-name-required",
        "credit-whole" => "runtime-error-credit-whole",
        "billing-token-rejected" => "runtime-error-billing-token-rejected",
        "account-auth-rejected" => "runtime-error-account-auth-rejected",
        "salvage-boundary" => "runtime-error-salvage-boundary",
        "salvage-review" => "runtime-error-salvage-review",
        "salvage-stale" => "runtime-error-salvage-stale",
        "salvage-cancelled" => "runtime-error-salvage-cancelled",
        "oauth-prepare" => "runtime-error-oauth-prepare",
        "oauth-start-github" => "runtime-error-oauth-start-github",
        "oauth-start-google" => "runtime-error-oauth-start-google",
        "oauth-start-apple" => "runtime-error-oauth-start-apple",
        "oauth-handoff" => "runtime-error-oauth-handoff",
        "oauth-complete-github" => "runtime-error-oauth-complete-github",
        "oauth-complete-google" => "runtime-error-oauth-complete-google",
        "oauth-complete-apple" => "runtime-error-oauth-complete-apple",
        "oauth-cancelled" => "runtime-error-oauth-cancelled",
        "oauth-callback" => "runtime-error-oauth-callback",
        "oauth-browser" => "runtime-error-oauth-browser",
        "oauth-url" => "runtime-error-oauth-url",
        "sign-in-publication" => "runtime-error-sign-in-publication",
        "account-already-signed-in" => "runtime-error-account-already-signed-in",
        "account-owner-read" => "runtime-error-account-owner-read",
        _ => return UiText::Raw(failure.message.clone()),
    };
    UiText::key(key)
}

pub(crate) fn persistence_text(
    failure: &maple_agent::store::PersistenceFailure,
) -> super::i18n::UiText {
    use maple_agent::store::PersistenceFailureReason;
    super::i18n::UiText::key(match failure.reason {
        PersistenceFailureReason::DiskFull => "runtime-persistence-disk-full",
        PersistenceFailureReason::SaveFailed => "runtime-persistence-save-failed",
    })
}

#[cfg(test)]
mod pricing_tests {
    use super::*;
    use crate::ui::i18n::{TestLocalizationGuard, apply_preference};

    #[test]
    fn pricing_failures_follow_locale_and_unknown_browser_errors_remain_raw() {
        let _locale = TestLocalizationGuard::enable();
        for code in ["browser-pricing-link", "browser-pricing-open"] {
            let text = failure_text(&HostFailure::new(code, "Legacy browser diagnostic"));
            apply_preference(Some("en"));
            let english = text.resolve();
            assert!(english.contains("pricing"));
            apply_preference(Some("es"));
            assert_ne!(text.resolve(), english);
            apply_preference(Some("en"));
            assert_eq!(text.resolve(), english);
        }
        let raw = failure_text(&HostFailure::new(
            "future-browser-failure",
            "Original diagnostic",
        ));
        for locale in ["en", "es", "en"] {
            apply_preference(Some(locale));
            assert_eq!(raw.resolve().as_ref(), "Original diagnostic");
        }
    }

    #[test]
    fn a_remote_mode_refusal_reads_in_the_interface_language() {
        let _locale = TestLocalizationGuard::enable();
        let notice = |code| {
            crate::ui::i18n::UiText::context(
                "chat-mode-failed",
                "message",
                failure_text(&HostFailure::new(code, "English diagnostic")),
            )
        };
        for (code, english, spanish) in [
            (
                "remote-permissions-unsupported",
                "Could not change the mode: Update Maple on the remote computer first.",
                "No se pudo cambiar el modo: Primero actualiza Maple en la computadora remota.",
            ),
            (
                "remote-permissions-unconfirmed",
                "Could not change the mode: Could not check the remote computer. Try again, or update Maple there.",
                "No se pudo cambiar el modo: No se pudo comprobar la computadora remota. Inténtalo de nuevo o actualiza Maple ahí.",
            ),
        ] {
            apply_preference(Some("en"));
            assert_eq!(notice(code).resolve().as_ref(), english);
            apply_preference(Some("es"));
            assert_eq!(notice(code).resolve().as_ref(), spanish);
        }
        apply_preference(Some("en"));
    }
}
