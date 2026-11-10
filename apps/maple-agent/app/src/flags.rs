//! Per-account feature flags from the OpenSecret flag service, read the way
//! the web app's `services/flags.ts` reads them. A flag only reveals an
//! optional feature: callers keep the feature hidden when this fails.

use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The full Usage page: per-day, per-model and per-app history.
pub const USAGE_PAGE: &str = "usage_page";

const PROD_FLAGS_URL: &str = "https://flags.opensecret.cloud";
const DEV_FLAGS_URL: &str = "https://flags-dev.opensecret.cloud";

static CLIENT: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| format!("failed to build the flags HTTP client: {error}"))
});

/// Flag service base URL for the compiled channel. Local builds allow
/// `MAPLE_FLAGS_URL`, and `MAPLE_FORCE_FEATURE_FLAGS` (a comma-separated
/// list of keys) turns flags on without the service, like the web app's
/// `VITE_FORCE_FEATURE_FLAGS`.
fn configured_flags_url() -> String {
    let baked = if crate::profile::PCR_ENVIRONMENT == "development" {
        DEV_FLAGS_URL
    } else {
        PROD_FLAGS_URL
    };
    let configured = crate::profile::runtime_value("MAPLE_FLAGS_URL", baked);
    let configured = configured.trim().trim_end_matches('/');
    if configured.is_empty() {
        baked.to_string()
    } else {
        configured.to_string()
    }
}

fn forced_on(key: &str) -> bool {
    crate::profile::runtime_value("MAPLE_FORCE_FEATURE_FLAGS", "")
        .split(',')
        .any(|forced| forced.trim() == key)
}

/// Whether `key` is on for `user_id`.
pub async fn is_enabled(user_id: &str, key: &str) -> Result<bool, String> {
    if forced_on(key) {
        return Ok(true);
    }
    let mut url = reqwest::Url::parse(&format!("{}/v1/users/", configured_flags_url()))
        .map_err(|error| format!("invalid flags URL: {error}"))?;
    url.path_segments_mut()
        .map_err(|()| "invalid flags URL".to_string())?
        .pop_if_empty()
        .extend([user_id, "flags"]);
    url.query_pairs_mut().append_pair("keys", key);
    let response = CLIENT
        .as_ref()
        .map_err(Clone::clone)?
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| format!("flags request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("flags request failed ({})", response.status()));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("flags decode failed: {error}"))?;
    flag_value(&body, user_id, key)
}

/// Reads `key` from a flag response, rejecting an answer for another account
/// or a non-boolean value, as the web client does.
fn flag_value(body: &serde_json::Value, user_id: &str, key: &str) -> Result<bool, String> {
    #[derive(serde::Deserialize)]
    struct Response {
        user_uuid: String,
        flags: BTreeMap<String, serde_json::Value>,
    }
    let response = <Response as serde::Deserialize>::deserialize(body)
        .map_err(|_| "flag service returned an invalid response".to_string())?;
    if !response.user_uuid.eq_ignore_ascii_case(user_id.trim()) {
        return Err("flag service answered for another account".into());
    }
    match response.flags.get(key) {
        None => Ok(false),
        Some(serde_json::Value::Bool(enabled)) => Ok(*enabled),
        Some(_) => Err("flag service returned a non-boolean flag".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flag_is_read_only_from_this_accounts_boolean_answer() {
        let body = serde_json::json!({
            "user_uuid": "ABC-1",
            "flags": { "usage_page": true, "other": "yes" }
        });
        assert_eq!(flag_value(&body, "abc-1", USAGE_PAGE), Ok(true));
        assert_eq!(flag_value(&body, "abc-1", "missing"), Ok(false));
        assert!(flag_value(&body, "abc-1", "other").is_err());
        assert!(flag_value(&body, "someone-else", USAGE_PAGE).is_err());
        assert!(flag_value(&serde_json::json!({ "flags": {} }), "abc-1", USAGE_PAGE).is_err());
    }
}
