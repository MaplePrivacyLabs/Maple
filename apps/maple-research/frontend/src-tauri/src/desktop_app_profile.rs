const DEV_IDENTIFIER: &str = "cloud.opensecret.maple.dev";
#[cfg(any(desktop, test))]
const PRODUCTION_SCHEME: &str = "cloud.opensecret.maple";

/// The installed bundle identity owns native policy; renderer build variables
/// cannot enable the production updater or register the other app's scheme.
pub fn is_development(identifier: &str) -> bool {
    identifier == DEV_IDENTIFIER
}

#[cfg(any(desktop, test))]
pub fn callback_scheme(identifier: &str) -> &'static str {
    if is_development(identifier) {
        DEV_IDENTIFIER
    } else {
        PRODUCTION_SCHEME
    }
}

pub fn accepts_callback_scheme(identifier: &str, scheme: Option<&str>) -> bool {
    if is_development(identifier) {
        scheme == Some(DEV_IDENTIFIER)
    } else {
        // Preserve production's existing HTTPS/payment routing and downstream
        // validation while never consuming a return intended for Maple Dev.
        scheme != Some(DEV_IDENTIFIER)
    }
}

#[cfg(any(desktop, test))]
pub fn updates_enabled(identifier: &str) -> bool {
    !is_development(identifier)
}

#[cfg(any(desktop, test))]
pub fn legacy_cleanup_enabled(identifier: &str) -> bool {
    // The legacy cleanup resolves a historical production directory directly.
    // A Dev launch must not remove files from that separate installation.
    !is_development(identifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_profile_registers_only_its_own_scheme() {
        assert_eq!(callback_scheme(PRODUCTION_SCHEME), PRODUCTION_SCHEME);
        assert_eq!(callback_scheme(DEV_IDENTIFIER), DEV_IDENTIFIER);
    }

    #[test]
    fn dev_accepts_only_its_custom_scheme() {
        assert!(accepts_callback_scheme(
            DEV_IDENTIFIER,
            Some(DEV_IDENTIFIER)
        ));
        for scheme in [Some(PRODUCTION_SCHEME), Some("https"), Some("http"), None] {
            assert!(!accepts_callback_scheme(DEV_IDENTIFIER, scheme));
        }
    }

    #[test]
    fn production_rejects_dev_without_changing_payment_routing() {
        assert!(!accepts_callback_scheme(
            PRODUCTION_SCHEME,
            Some(DEV_IDENTIFIER)
        ));
        for scheme in [Some(PRODUCTION_SCHEME), Some("https")] {
            assert!(accepts_callback_scheme(PRODUCTION_SCHEME, scheme));
        }
    }

    #[test]
    fn dev_cannot_update_or_clean_production_state() {
        assert!(!updates_enabled(DEV_IDENTIFIER));
        assert!(!legacy_cleanup_enabled(DEV_IDENTIFIER));
        assert!(updates_enabled(PRODUCTION_SCHEME));
        assert!(legacy_cleanup_enabled(PRODUCTION_SCHEME));
    }
}
