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

#[derive(serde::Serialize)]
pub struct BuildInfo {
    profile: &'static str,
    display_name: &'static str,
    bundle_id: &'static str,
    data_namespace: &'static str,
    api_url: &'static str,
    billing_api_url: &'static str,
    client_id: &'static str,
    pcr_environment: &'static str,
    version: &'static str,
    git_revision: &'static str,
    source_sha: &'static str,
    update_tag_prefix: &'static str,
    prerelease: bool,
}

pub fn build_info() -> BuildInfo {
    BuildInfo {
        profile: PROFILE,
        display_name: DISPLAY_NAME,
        bundle_id: BUNDLE_ID,
        data_namespace: DATA_NAMESPACE,
        api_url: API_URL,
        billing_api_url: BILLING_API_URL,
        client_id: CLIENT_ID,
        pcr_environment: PCR_ENVIRONMENT,
        version: env!("CARGO_PKG_VERSION"),
        git_revision: env!("MAPLE_GIT_HASH"),
        source_sha: env!("MAPLE_GIT_SOURCE_SHA"),
        update_tag_prefix: UPDATE_TAG_PREFIX,
        prerelease: PRERELEASE,
    }
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
    fn build_info_reports_the_baked_profile() {
        let info = serde_json::to_value(build_info()).unwrap();
        assert_eq!(info["profile"], PROFILE);
        assert_eq!(info["bundle_id"], BUNDLE_ID);
        assert_eq!(info["data_namespace"], DATA_NAMESPACE);
        assert_eq!(info["api_url"], API_URL);
        assert_eq!(info["pcr_environment"], PCR_ENVIRONMENT);
        assert_eq!(info["source_sha"], env!("MAPLE_GIT_SOURCE_SHA"));
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
