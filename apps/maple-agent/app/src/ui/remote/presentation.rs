//! Localized presentation of typed remote update outcomes.
use crate::{
    remote::update::{BlockedReason, InstallRefusal},
    ui::i18n::{UiText, tr},
};
pub(super) fn blocked(reason: BlockedReason) -> gpui::SharedString {
    tr(match reason {
        BlockedReason::PasswordProtected => "settings-remote-blocked-password-protected",
        BlockedReason::UnsupportedInstallation => {
            "settings-remote-blocked-unsupported-installation"
        }
        BlockedReason::UpdatesDisabled => "settings-remote-blocked-updates-disabled",
        BlockedReason::UpdateNotReady => "settings-remote-blocked-update-not-ready",
        BlockedReason::Busy => "settings-remote-blocked-busy",
        BlockedReason::DesktopUnavailable => "settings-remote-blocked-desktop-unavailable",
        BlockedReason::CoordinatorUnavailable => "settings-remote-blocked-coordinator-unavailable",
        BlockedReason::PairingConfirmationRequired => {
            "settings-remote-blocked-pairing-confirmation-required"
        }
        BlockedReason::Unknown => "settings-remote-blocked-unknown",
    })
}
pub(super) fn refused(reason: InstallRefusal) -> UiText {
    UiText::key(match reason {
        InstallRefusal::AuthorizationChanged => "settings-remote-refused-authorization-changed",
        InstallRefusal::UpdateChanged => "settings-remote-refused-update-changed",
        InstallRefusal::NotReady => "settings-remote-refused-not-ready",
        InstallRefusal::Unsupported => "settings-remote-refused-unsupported",
        InstallRefusal::AccountChanged => "settings-remote-refused-account-changed",
        InstallRefusal::Busy => "settings-remote-refused-busy",
        InstallRefusal::Unavailable => "settings-remote-refused-unavailable",
        InstallRefusal::DownloadRequired => "settings-remote-refused-download-required",
    })
}
