//! The build-time contract shared by the app and runtime build scripts.
//! Public service configuration is checked in; packaged channels cannot be
//! selected or redirected by an environment inherited from a desktop launcher.

use std::collections::BTreeMap;

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseProfile {
    pub display_name: String,
    pub bundle_id: String,
    pub data_namespace: String,
    pub api_url: String,
    pub billing_api_url: String,
    pub web_url: String,
    pub auth_origin: Option<String>,
    pub client_id: String,
    pub pcr_environment: String,
    pub update_tag_prefix: String,
    pub prerelease: bool,
}

pub fn profiles() -> Result<BTreeMap<String, ReleaseProfile>, String> {
    let profiles: BTreeMap<String, ReleaseProfile> =
        serde_json::from_str(include_str!("release-profiles.json")).map_err(|e| e.to_string())?;
    if profiles.len() != 2 || !profiles.contains_key("dev") || !profiles.contains_key("prod") {
        return Err("release-profiles.json must contain exactly dev and prod".into());
    }
    for (channel, expected_id, expected_namespace, expected_pcr, expected_prefix) in [
        (
            "dev",
            "cloud.opensecret.maple.agent.dev",
            "maple-agent-dev",
            "development",
            "maple-agent-dev-v",
        ),
        (
            "prod",
            "cloud.opensecret.maple.agent",
            "maple-agent-prod",
            "production",
            "maple-agent-v",
        ),
    ] {
        let profile = &profiles[channel];
        if profile.bundle_id != expected_id
            || profile.data_namespace != expected_namespace
            || profile.pcr_environment != expected_pcr
            || profile.update_tag_prefix != expected_prefix
            || profile.prerelease != (channel == "dev")
        {
            return Err(format!(
                "{channel} profile has an invalid channel identity or trust root"
            ));
        }
        if profile.display_name.is_empty()
            || !profile.api_url.starts_with("https://")
            || !profile.billing_api_url.starts_with("https://")
            || profile.web_url
                != if channel == "dev" {
                    "https://app-dev.trymaple.ai"
                } else {
                    "https://trymaple.ai"
                }
            || profile.auth_origin.as_deref()
                != if channel == "dev" {
                    Some("https://auth-dev.maple.ai")
                } else {
                    None
                }
            || profile.client_id != "ba5a14b5-d915-47b1-b7b1-afda52bc5fc6"
        {
            return Err(format!(
                "{channel} profile has invalid public service configuration"
            ));
        }
    }
    if profiles["dev"].display_name == profiles["prod"].display_name
        || profiles["dev"].api_url == profiles["prod"].api_url
        || profiles["dev"].billing_api_url == profiles["prod"].billing_api_url
        || profiles["dev"].web_url == profiles["prod"].web_url
    {
        return Err("Dev and Prod must have distinct names and service endpoints".into());
    }
    Ok(profiles)
}

pub fn select_profile(
    channel: Option<&str>,
    configured_pcr: Option<&str>,
) -> Result<(&'static str, ReleaseProfile), String> {
    let profiles = profiles()?;
    let channel = match channel {
        None => "unpackaged",
        Some("dev") => "dev",
        Some("prod") => "prod",
        Some(_) => return Err("MAPLE_RELEASE_PROFILE must be dev or prod when set".into()),
    };
    let mut profile = profiles[if channel == "unpackaged" {
        "prod"
    } else {
        channel
    }]
    .clone();
    if channel == "unpackaged" {
        profile.display_name = "Maple Agent Debug".into();
        profile.bundle_id = "cloud.opensecret.maple.agent.debug".into();
        profile.data_namespace = "maple-agent".into();
        // Hosted return is admitted only by the packaged Dev profile. Local
        // environment overrides must not opt an unpackaged client into it.
        profile.auth_origin = None;
        // Local/MDE builds keep the existing explicit PCR override. Invalid
        // values still fail runtime validation rather than being normalized.
        if let Some(pcr) = configured_pcr {
            profile.pcr_environment = pcr.into();
        }
    } else if configured_pcr.is_some_and(|pcr| pcr != profile.pcr_environment) {
        return Err(format!(
            "{channel} requires VITE_OPEN_SECRET_PCR_ENVIRONMENT={}",
            profile.pcr_environment
        ));
    }
    Ok((channel, profile))
}

pub fn emit(manifest_path: &str, helper_path: &str, app: bool) {
    println!("cargo:rerun-if-changed={manifest_path}");
    println!("cargo:rerun-if-changed={helper_path}");
    println!("cargo:rerun-if-env-changed=MAPLE_RELEASE_PROFILE");
    println!("cargo:rerun-if-env-changed=VITE_OPEN_SECRET_PCR_ENVIRONMENT");
    let channel = std::env::var("MAPLE_RELEASE_PROFILE").ok();
    let pcr = std::env::var("VITE_OPEN_SECRET_PCR_ENVIRONMENT").ok();
    let (channel, profile) = select_profile(channel.as_deref(), pcr.as_deref())
        .unwrap_or_else(|error| panic!("invalid Maple Agent build profile: {error}"));
    // Both app and runtime independently consume the same manifest and build
    // input. This sets the dependency's option_env!, not just the final binary.
    if channel != "unpackaged" {
        println!(
            "cargo:rustc-env=VITE_OPEN_SECRET_PCR_ENVIRONMENT={}",
            profile.pcr_environment
        );
    }
    if !app {
        return;
    }
    let mut constants = String::new();
    let pricing_url = format!("{}/pricing", profile.web_url);
    let checkout_success_url = format!("{pricing_url}?success=true");
    let checkout_cancel_url = format!("{pricing_url}?canceled=true");
    for (name, value) in [
        ("PROFILE", channel),
        ("DISPLAY_NAME", profile.display_name.as_str()),
        ("BUNDLE_ID", profile.bundle_id.as_str()),
        ("DATA_NAMESPACE", profile.data_namespace.as_str()),
        ("API_URL", profile.api_url.as_str()),
        ("BILLING_API_URL", profile.billing_api_url.as_str()),
        ("WEB_URL", profile.web_url.as_str()),
        ("PRICING_URL", pricing_url.as_str()),
        ("CHECKOUT_SUCCESS_URL", checkout_success_url.as_str()),
        ("CHECKOUT_CANCEL_URL", checkout_cancel_url.as_str()),
        ("CLIENT_ID", profile.client_id.as_str()),
        ("PCR_ENVIRONMENT", profile.pcr_environment.as_str()),
        ("UPDATE_TAG_PREFIX", profile.update_tag_prefix.as_str()),
    ] {
        // JSON string literals are also valid Rust literals for this ASCII
        // contract and safely quote configuration rather than interpolating it.
        let literal = serde_json::to_string(value).expect("profile string");
        constants.push_str(&format!("pub const {name}: &str = {literal};\n"));
    }
    let auth_origin = match profile.auth_origin.as_deref() {
        Some(origin) => format!(
            "Some({})",
            serde_json::to_string(origin).expect("profile auth origin")
        ),
        None => "None".to_string(),
    };
    constants.push_str(&format!(
        "pub const AUTH_ORIGIN: Option<&str> = {auth_origin};\n"
    ));
    constants.push_str(&format!(
        "pub const PRERELEASE: bool = {};\n",
        profile.prerelease
    ));
    constants.push_str(&format!(
        "pub const PACKAGED: bool = {};\n",
        channel != "unpackaged"
    ));
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    std::fs::write(out.join("release_profile.rs"), constants).expect("write build profile");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_profiles_have_distinct_identity_state_endpoints_and_trust() {
        let (_, dev) = select_profile(Some("dev"), None).unwrap();
        let (_, prod) = select_profile(Some("prod"), None).unwrap();
        assert_ne!(dev.bundle_id, prod.bundle_id);
        assert_ne!(dev.data_namespace, prod.data_namespace);
        assert_ne!(dev.api_url, prod.api_url);
        assert_ne!(dev.billing_api_url, prod.billing_api_url);
        assert_eq!(dev.web_url, "https://app-dev.trymaple.ai");
        assert_eq!(prod.web_url, "https://trymaple.ai");
        assert_eq!(
            dev.auth_origin.as_deref(),
            Some("https://auth-dev.maple.ai")
        );
        assert_eq!(prod.auth_origin, None);
        assert_ne!(dev.pcr_environment, prod.pcr_environment);
        assert_ne!(dev.update_tag_prefix, prod.update_tag_prefix);
        assert!(dev.prerelease);
        assert!(!prod.prerelease);
        for profile in [dev, prod] {
            assert_ne!(profile.data_namespace, "maple-agent");
            assert_ne!(profile.data_namespace, "maple-gpui");
            assert!(!profile.bundle_id.ends_with(".debug"));
        }
    }

    #[test]
    fn packaged_profile_rejects_mismatched_trust_and_unknown_channel() {
        assert!(select_profile(Some("dev"), Some("production")).is_err());
        assert!(select_profile(Some("prod"), Some("development")).is_err());
        for channel in ["", "production", "Dev", "staging"] {
            assert!(select_profile(Some(channel), None).is_err());
        }
        assert!(select_profile(Some("dev"), Some("development")).is_ok());
        assert!(select_profile(Some("prod"), Some("production")).is_ok());
    }

    #[test]
    fn unpackaged_build_keeps_local_namespace_and_explicit_trust_override() {
        let (channel, profile) = select_profile(None, Some("development")).unwrap();
        assert_eq!(channel, "unpackaged");
        assert_eq!(profile.data_namespace, "maple-agent");
        assert_eq!(profile.pcr_environment, "development");
        assert_eq!(profile.bundle_id, "cloud.opensecret.maple.agent.debug");
        assert_eq!(profile.auth_origin, None);
    }
}
