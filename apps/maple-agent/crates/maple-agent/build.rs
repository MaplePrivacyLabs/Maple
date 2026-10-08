//! Declare the platforms on which Maple can host the CUA runtime in-process.
//!
//! The Cua Driver SDK ships a native backend per desktop platform. Maple
//! enables the ones it can build and test. Spelling that decision once as a
//! named `cfg` keeps the platform predicate out of every call site, so adding
//! a platform is a one-line change here rather than an edit in each module.

#[path = "../../release_profile.rs"]
mod release_profile;

fn main() {
    release_profile::emit(
        "../../release-profiles.json",
        "../../release_profile.rs",
        false,
    );
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(embedded_cua)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(target_os.as_str(), "macos" | "linux") {
        println!("cargo::rustc-cfg=embedded_cua");
    }
}
