#[path = "src/ios_app_variant.rs"]
mod ios_app_variant;

fn main() {
    validate_desktop_dev_profile();
    println!("cargo:rerun-if-env-changed=VITE_OPEN_SECRET_PCR_ENVIRONMENT");
    println!("cargo:rerun-if-env-changed=MAPLE_IOS_VARIANT");
    println!("cargo:rerun-if-env-changed=VITE_MAPLE_APP_VARIANT");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "ios" {
        println!("cargo:rustc-link-lib=c++");
    }

    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_arch == "x86_64" && target_env == "msvc" {
        const WINDOWS_STACK_RESERVE_BYTES: usize = 8 * 1024 * 1024;
        println!("cargo:rustc-link-arg-bin=maple=/STACK:{WINDOWS_STACK_RESERVE_BYTES}");
    }

    tauri_build::build();

    // The deep-link plugin's build.rs overwrites CFBundleURLTypes in Info.plist
    // based on the mobile config, stripping our custom URL scheme.
    // Re-add it after all plugin build scripts have run.
    if target_os == "ios" {
        ensure_ios_custom_url_scheme();
    }
}

// A standalone Dev package must not silently mix the production native identity
// with a Dev renderer (or inherit an operator's iOS/TestFlight origin).
fn validate_desktop_dev_profile() {
    println!("cargo:rerun-if-env-changed=MAPLE_DESKTOP_VARIANT");
    println!("cargo:rerun-if-env-changed=VITE_MAPLE_DESKTOP_VARIANT");
    println!("cargo:rerun-if-env-changed=TAURI_CONFIG");
    println!("cargo:rerun-if-changed=desktop-dev-profile.json");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let config: serde_json::Value = std::env::var("TAURI_CONFIG")
        .ok()
        .map(|value| serde_json::from_str(&value).expect("invalid Tauri configuration overlay"))
        .unwrap_or_default();
    if std::env::var("MAPLE_DESKTOP_VARIANT").is_err() {
        assert!(
            std::env::var("VITE_MAPLE_DESKTOP_VARIANT").is_err(),
            "use the Research Dev desktop build script"
        );
        if target_os == "macos" {
            assert_ne!(
                config["identifier"], "cloud.opensecret.maple.dev",
                "use the Research Dev desktop build script for this bundle"
            );
        }
        return;
    }
    assert_eq!(
        target_os, "macos",
        "Research Dev desktop packaging currently supports macOS"
    );
    let profile: serde_json::Value =
        serde_json::from_str(include_str!("desktop-dev-profile.json")).unwrap();
    for (key, expected) in profile["environment"].as_object().unwrap() {
        println!("cargo:rerun-if-env-changed={key}");
        assert_eq!(
            std::env::var(key).ok().as_deref(),
            expected.as_str(),
            "Research Dev desktop environment mismatch: {key}"
        );
    }
    assert_eq!(config["identifier"], profile["identifier"]);
    assert_eq!(config["productName"], profile["productName"]);
    assert_eq!(
        config["plugins"]["deep-link"]["desktop"]["schemes"],
        serde_json::json!([profile["scheme"]])
    );
    assert_eq!(
        config["plugins"]["updater"]["endpoints"],
        serde_json::json!([])
    );
    assert_eq!(config["bundle"]["createUpdaterArtifacts"], false);
}

#[allow(dead_code)]
fn ensure_ios_custom_url_scheme() {
    let plist_path = std::path::Path::new("gen/apple/maple_iOS/Info.plist");
    if !plist_path.exists() {
        return;
    }

    let mut plist: plist::Value = plist::from_file(plist_path).expect("failed to read Info.plist");
    let dict = plist
        .as_dictionary_mut()
        .expect("Info.plist is not a dictionary");

    let ios_variant = std::env::var("MAPLE_IOS_VARIANT").ok();
    let frontend_variant = std::env::var("VITE_MAPLE_APP_VARIANT").ok();
    let scheme =
        ios_app_variant::custom_url_scheme(ios_variant.as_deref(), frontend_variant.as_deref())
            .expect("invalid Maple iOS app variant");

    // Switching build variants must not keep the other app's callback registered.
    let mut changed = false;
    let other_scheme = if scheme == "cloud.opensecret.maple.dev" {
        "cloud.opensecret.maple"
    } else {
        "cloud.opensecret.maple.dev"
    };
    {
        if let Some(entries) = dict
            .get_mut("CFBundleURLTypes")
            .and_then(plist::Value::as_array_mut)
        {
            for entry in entries.iter_mut() {
                if let Some(schemes) = entry
                    .as_dictionary_mut()
                    .and_then(|entry| entry.get_mut("CFBundleURLSchemes"))
                    .and_then(plist::Value::as_array_mut)
                {
                    let old_len = schemes.len();
                    schemes.retain(|value| value.as_string() != Some(other_scheme));
                    changed |= old_len != schemes.len();
                }
            }
        }
    }

    let has_scheme = dict
        .get("CFBundleURLTypes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|entry| {
                entry
                    .as_dictionary()
                    .and_then(|d| d.get("CFBundleURLSchemes"))
                    .and_then(|v| v.as_array())
                    .map(|schemes| schemes.iter().any(|s| s.as_string() == Some(scheme)))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    if !has_scheme {
        let mut url_type = plist::Dictionary::new();
        url_type.insert(
            "CFBundleURLSchemes".into(),
            vec![plist::Value::String(scheme.to_string())].into(),
        );
        url_type.insert(
            "CFBundleURLName".into(),
            plist::Value::String(scheme.to_string()),
        );

        if !dict.contains_key("CFBundleURLTypes") {
            dict.insert("CFBundleURLTypes".into(), plist::Value::Array(vec![]));
        }

        if let Some(arr) = dict
            .get_mut("CFBundleURLTypes")
            .and_then(|v| v.as_array_mut())
        {
            arr.push(plist::Value::Dictionary(url_type));
        }

        changed = true;
    }
    if changed {
        plist::to_file_xml(plist_path, &plist).expect("failed to write Info.plist");
    }
}
