//! Bakes the git revision into `--version` so a running binary can be
//! matched back to a checkout. Falls back to `unknown` outside a git
//! repository (e.g. a tarball build), never fails the build.

use std::process::Command;

#[path = "../release_profile.rs"]
mod release_profile;

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    release_profile::emit("../release-profiles.json", "../release_profile.rs", true);
    // A new commit must re-bake the hash, but cargo only reruns build
    // scripts when declared inputs change. Watch the git pointers: HEAD
    // moves on branch switches, and the ref file it names moves on every
    // commit to that branch. Narrowing the triggers means the -dirty
    // suffix only refreshes alongside these files, which is acceptable:
    // the revision is the load-bearing part.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        if let Ok(head) = std::fs::read_to_string(format!("{git_dir}/HEAD"))
            && let Some(reference) = head.trim().strip_prefix("ref: ")
            && let Some(reference_path) = git(&["rev-parse", "--git-path", reference])
        {
            // Worktree HEAD lives in its own git dir, while branch refs live
            // in the common git dir. Ask Git for the real path in either case.
            println!("cargo:rerun-if-changed={reference_path}");
        }
        if let Some(packed_refs) = git(&["rev-parse", "--git-path", "packed-refs"]) {
            println!("cargo:rerun-if-changed={packed_refs}");
        }
    }
    let mut revision = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    if git(&["status", "--porcelain"]).is_some_and(|status| !status.is_empty()) {
        revision.push_str("-dirty");
    }
    println!("cargo:rustc-env=MAPLE_GIT_HASH={revision}");
    let source_sha = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=MAPLE_GIT_SOURCE_SHA={source_sha}");

    // The CLI and macOS signing preflight read the same immutable public
    // metadata. Signing must inspect the Mach-O section without running a
    // downloaded executable while its keychain or Apple credentials exist.
    let channel = std::env::var("MAPLE_RELEASE_PROFILE").ok();
    let pcr = std::env::var("VITE_OPEN_SECRET_PCR_ENVIRONMENT").ok();
    let (channel, profile) = release_profile::select_profile(channel.as_deref(), pcr.as_deref())
        .expect("validated Maple Agent build profile");
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
}
