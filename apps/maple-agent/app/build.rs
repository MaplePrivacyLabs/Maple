//! Bakes the git revision into `--version` so a running binary can be
//! matched back to a checkout. Falls back to `unknown` outside a git
//! repository (e.g. a tarball build), never fails the build.

mod git_metadata;
#[path = "../release_profile.rs"]
mod release_profile;

fn main() {
    release_profile::emit("../release-profiles.json", "../release_profile.rs", true);
    let git_metadata::GitMetadata {
        revision,
        source_sha,
    } = git_metadata::emit();

    // The CLI and native signing preflights read the same immutable public
    // metadata. Signing must inspect the artifact without running a downloaded
    // executable while signing credentials exist.
    let channel = std::env::var("MAPLE_RELEASE_PROFILE").ok();
    let pcr = std::env::var("VITE_OPEN_SECRET_PCR_ENVIRONMENT").ok();
    let (channel, profile) = release_profile::select_profile(channel.as_deref(), pcr.as_deref())
        .expect("validated Maple Agent build profile");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").expect("Cargo target OS");
    let update_feed_url = profile.update_feed_for_target_os(&target_os);
    let metadata = serde_json::json!({
        "profile": channel,
        "display_name": profile.display_name,
        "bundle_id": profile.bundle_id,
        "data_namespace": profile.data_namespace,
        "api_url": profile.api_url,
        "billing_api_url": profile.billing_api_url,
        "web_url": profile.web_url,
        "auth_origin": profile.auth_origin,
        "client_id": profile.client_id,
        "pcr_environment": profile.pcr_environment,
        "version": std::env::var("CARGO_PKG_VERSION").expect("Cargo package version"),
        "git_revision": revision,
        "source_sha": source_sha,
        "update_tag_prefix": profile.update_tag_prefix,
        "update_feed_url": update_feed_url,
        "update_public_keys": profile.update_public_keys,
        "prerelease": profile.prerelease,
    });
    // GPUI unifies serde_json's preserve_order feature in desktop builds.
    // Serialize a BTreeMap so metadata remains canonical in every feature set,
    // matching the non-executing reader and CLI byte-for-byte.
    let sorted_metadata: std::collections::BTreeMap<_, _> = metadata
        .as_object()
        .expect("public build metadata object")
        .iter()
        .collect();
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    std::fs::write(
        out.join("build-info.json"),
        serde_json::to_vec(&sorted_metadata).expect("public build metadata"),
    )
    .expect("write public build metadata");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Retain the dedicated metadata symbol even with release LTO and
        // linker dead stripping. __TEXT keeps it read-only in the final app.
        println!("cargo:rustc-link-arg-bin=maple-agent=-Wl,-u,_MAPLE_AGENT_BUILD_INFO");
        // The embedded ScreenCaptureKit bridge can link Swift compatibility
        // libraries with @rpath install names. A transitive library cannot
        // choose the final host executable's bundle layout, so keep the
        // system runtime and application fallback paths on Maple's binary
        // target. Prefer the system runtime so Apple frameworks and Maple do
        // not load duplicate copies on current macOS releases.
        println!("cargo:rustc-link-arg-bin=maple-agent=-Wl,-rpath,/usr/lib/swift");
        println!("cargo:rustc-link-arg-bin=maple-agent=-Wl,-rpath,@executable_path/../Frameworks");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64")
    {
        // #[used] retains the object symbol; /INCLUDE additionally roots it
        // through MSVC release /OPT:REF and LTO. x64 C symbols are undecorated.
        println!("cargo:rustc-link-arg-bin=maple-agent=/INCLUDE:MAPLE_AGENT_BUILD_INFO");
    }
}
