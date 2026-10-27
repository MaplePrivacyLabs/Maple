//! Agent-only authenticated update discovery, download and installation primitives.
//! Callers must retain installation and host-startup fences around installation.
//!
//! Platform-specific installers share the authenticated discovery and download
//! state machine. Signed metadata keeps the macOS, Linux and Windows inventories
//! separate so adding a platform cannot reinterpret installed clients' feeds.
mod client;
pub mod installer;
pub mod manifest;
mod state;

pub use client::{CheckOutcome, Config, Revalidation, Updater};
pub use manifest::{Artifact, Downloads, Manifest, Target, VerifiedRelease};
pub use semver::Version;
pub use state::PreparedUpdate;
pub use url::Url;

#[cfg(test)]
mod tests;
