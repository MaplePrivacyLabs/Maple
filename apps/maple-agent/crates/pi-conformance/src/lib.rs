//! Test-only checks and replay support for the pinned Pi reference corpus.
//!
//! Expected data is produced exclusively by the TypeScript recorder. This
//! crate deliberately has no command that accepts Rust output as a baseline.

pub mod compare;
pub mod coverage;
pub mod dependencies;
pub mod integrity;
pub mod replay;
pub mod selection;
mod timers;

use std::path::{Path, PathBuf};

pub type CheckResult<T = ()> = Result<T, String>;

pub fn agent_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the Agent workspace must exist")
}

pub fn reference_root() -> PathBuf {
    agent_root().join("pi-conformance")
}

pub(crate) fn read(path: &Path) -> CheckResult<String> {
    std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn json<T: serde::de::DeserializeOwned>(path: &Path) -> CheckResult<T> {
    serde_json::from_str(&read(path)?).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn toml<T: serde::de::DeserializeOwned>(path: &Path) -> CheckResult<T> {
    toml::from_str(&read(path)?).map_err(|error| format!("{}: {error}", path.display()))
}
