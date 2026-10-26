//! Immutable packaged identity and service configuration. Local/MDE builds
//! keep their existing runtime overrides without sharing packaged state.

include!(concat!(env!("OUT_DIR"), "/release_profile.rs"));

pub fn runtime_value(name: &str, baked: &str) -> String {
    value_for(PROFILE, baked, crate::env::env_string(name))
}

fn value_for(profile: &str, baked: &str, configured: Option<String>) -> String {
    if profile == "unpackaged" {
        configured.unwrap_or_else(|| baked.to_string())
    } else {
        baked.to_string()
    }
}

// No code runs when the signing helper reads this section. Keep the exact
// JSON bytes used by --build-info, rather than a separate signing sidecar
// that could describe a different binary.
#[used]
#[unsafe(no_mangle)]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__TEXT,__maple_info"))]
#[cfg_attr(target_os = "windows", unsafe(link_section = ".maple"))]
static MAPLE_AGENT_BUILD_INFO: [u8; include_bytes!(concat!(env!("OUT_DIR"), "/build-info.json"))
    .len()] = *include_bytes!(concat!(env!("OUT_DIR"), "/build-info.json"));

pub fn build_info() -> serde_json::Value {
    let info: serde_json::Value =
        serde_json::from_slice(&MAPLE_AGENT_BUILD_INFO).expect("generated public build metadata");
    debug_assert_eq!(info["profile"] != "unpackaged", PACKAGED);
    debug_assert_eq!(info["display_name"], DISPLAY_NAME);
    debug_assert_eq!(info["bundle_id"], BUNDLE_ID);
    debug_assert_eq!(info["auth_origin"], serde_json::json!(AUTH_ORIGIN));
    debug_assert_eq!(info["pcr_environment"], PCR_ENVIRONMENT);
    debug_assert_eq!(info["update_tag_prefix"], UPDATE_TAG_PREFIX);
    debug_assert_eq!(info["update_feed_url"], serde_json::json!(UPDATE_FEED_URL));
    debug_assert_eq!(
        info["update_public_keys"],
        serde_json::json!(UPDATE_PUBLIC_KEYS)
    );
    debug_assert_eq!(info["prerelease"], PRERELEASE);
    info
}

#[cfg(test)]
#[allow(dead_code)]
#[path = "../../release_profile.rs"]
mod config;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_services_ignore_inherited_runtime_overrides() {
        for channel in ["dev", "prod"] {
            let (_, profile) = config::select_profile(Some(channel), None).unwrap();
            for baked in [
                &profile.api_url,
                &profile.billing_api_url,
                &profile.client_id,
            ] {
                assert_eq!(
                    value_for(channel, baked, Some("https://wrong.example".into())),
                    *baked
                );
            }
        }
        assert_eq!(
            value_for("unpackaged", API_URL, Some("http://localhost:3000".into())),
            "http://localhost:3000"
        );
    }

    #[test]
    fn build_info_json_is_canonical_across_feature_sets() {
        let info = build_info();
        let keys: Vec<_> = info.as_object().unwrap().keys().collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(serde_json::to_vec(&info).unwrap(), MAPLE_AGENT_BUILD_INFO);
    }

    #[test]
    fn build_info_reports_the_baked_profile() {
        let info = serde_json::to_value(build_info()).unwrap();
        assert_eq!(info["profile"], PROFILE);
        assert_eq!(info["bundle_id"], BUNDLE_ID);
        assert_eq!(info["data_namespace"], DATA_NAMESPACE);
        assert_eq!(info["api_url"], API_URL);
        assert_eq!(info["pcr_environment"], PCR_ENVIRONMENT);
        assert_eq!(info["source_sha"], env!("MAPLE_GIT_SOURCE_SHA"));
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(info["git_revision"], env!("MAPLE_GIT_HASH"));
        assert_eq!(info["display_name"], DISPLAY_NAME);
        assert_eq!(info["billing_api_url"], BILLING_API_URL);
        assert_eq!(info["web_url"], WEB_URL);
        assert_eq!(info["auth_origin"], serde_json::json!(AUTH_ORIGIN));
        assert_eq!(info["client_id"], CLIENT_ID);
        assert_eq!(info["update_tag_prefix"], UPDATE_TAG_PREFIX);
        assert_eq!(info["update_feed_url"], serde_json::json!(UPDATE_FEED_URL));
        assert_eq!(
            info["update_public_keys"],
            serde_json::json!(UPDATE_PUBLIC_KEYS)
        );
        assert_eq!(info["prerelease"], PRERELEASE);
    }

    #[test]
    fn prod_update_pins_are_accepted_and_reject_the_test_signer() {
        use maple_agent_updater::{Config, Target, Updater, manifest::verify_envelope};

        let (_, prod) = config::select_profile(Some("prod"), None).unwrap();
        let feed = prod.update_feed_url.unwrap().parse().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let installed = temporary.path().join("installed");
        std::fs::create_dir(&installed).unwrap();
        // Constructing the actual client parses every configured minisign key.
        // It writes only isolated temporary state and makes no network request.
        Updater::new(Config {
            feed_url: feed,
            public_keys: prod.update_public_keys.clone(),
            current_version: "0.0.1".parse().unwrap(),
            target: Target::DarwinAarch64,
            installed_path: installed,
            state_dir: temporary.path().join("updates"),
        })
        .unwrap();
        assert!(
            verify_envelope(
                include_bytes!(
                    "../../crates/maple-agent-updater/tests/fixtures/signed-envelope.json"
                ),
                &"https://updates.trymaple.ai/".parse().unwrap(),
                &prod.update_public_keys,
            )
            .is_err()
        );
    }

    #[test]
    fn runtime_dependency_bakes_the_same_pcr_trust_as_app_metadata() {
        let actual = maple_agent::open_secret_config::configured_pcr0_environment().unwrap();
        let expected = match PCR_ENVIRONMENT {
            "production" => "Production",
            "development" => "Development",
            _ => panic!("invalid compiled PCR environment"),
        };
        assert_eq!(format!("{actual:?}"), expected);
    }
}
