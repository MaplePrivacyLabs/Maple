//! Per-account ACP configuration: the on-disk shape, where it lives, and
//! how it is loaded and saved.

use crate::maple_api::account_scope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;

/// Upper bound on concurrent ACP connections one account may hold open.
pub(super) const MAX_ACP_CONNECTIONS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentAcpPermissionMode {
    ReadOnly,
    AllowAll,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentAcpConfig {
    #[serde(default = "default_permission_mode")]
    pub permission_mode: AgentAcpPermissionMode,
    #[serde(default)]
    pub allowed_project_roots: Vec<String>,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
}

fn default_permission_mode() -> AgentAcpPermissionMode {
    AgentAcpPermissionMode::ReadOnly
}

fn default_max_connections() -> usize {
    8
}

impl Default for AgentAcpConfig {
    fn default() -> Self {
        Self {
            permission_mode: default_permission_mode(),
            allowed_project_roots: Vec::new(),
            max_connections: default_max_connections(),
        }
    }
}
#[derive(Default)]
pub(super) struct AgentAcpStats {
    pub(super) active_sessions: AtomicUsize,
    pub(super) active_runs: AtomicUsize,
    pub(super) credential_connections: AtomicUsize,
}
/// Per-account ACP configuration saved under `local_data_root`.
pub fn load_acp_config(local_data_root: &Path, user_id: &str) -> Result<AgentAcpConfig, String> {
    load_config(local_data_root, user_id)
}
pub(super) fn config_path(local_data_root: &Path, user_id: &str) -> Result<PathBuf, String> {
    let scope = account_scope(user_id)
        .map_err(|_| "Maple ACP configuration requires an authenticated user".to_string())?;
    config_path_for_scope(local_data_root, &scope)
}

pub(super) fn config_path_for_scope(
    local_data_root: &Path,
    scope: &str,
) -> Result<PathBuf, String> {
    Ok(acp_accounts_root(local_data_root)?
        .join(scope)
        .join("config.json"))
}

pub(super) fn acp_accounts_root(local_data_root: &Path) -> Result<PathBuf, String> {
    Ok(local_data_root.join("acp").join("accounts"))
}

pub(super) fn legacy_config_path(local_data_root: &Path, user_id: &str) -> Result<PathBuf, String> {
    if user_id.trim().is_empty() {
        return Err("Maple ACP configuration requires an authenticated user".to_string());
    }
    let digest = Sha256::digest(user_id.as_bytes());
    let legacy_scope = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(acp_accounts_root(local_data_root)?
        .join(legacy_scope)
        .join("config.json"))
}

pub(super) fn load_config(local_data_root: &Path, user_id: &str) -> Result<AgentAcpConfig, String> {
    let path = config_path(local_data_root, user_id)?;
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("Failed to parse Maple ACP configuration: {error}"))
            .and_then(normalize_config),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let legacy_path = legacy_config_path(local_data_root, user_id)?;
            match std::fs::read(legacy_path) {
                Ok(bytes) => {
                    let config = serde_json::from_slice(&bytes)
                        .map_err(|error| {
                            format!("Failed to parse Maple ACP configuration: {error}")
                        })
                        .and_then(normalize_config)?;
                    // Keep the POC file intact so switching back to the original
                    // branch remains harmless, while future saves use Maple's
                    // canonical full account scope.
                    if let Err(error) = save_config(local_data_root, user_id, &config) {
                        log::warn!("Failed to migrate Maple ACP configuration: {error}");
                    }
                    Ok(config)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(AgentAcpConfig::default())
                }
                Err(error) => Err(format!("Failed to read Maple ACP configuration: {error}")),
            }
        }
        Err(error) => Err(format!("Failed to read Maple ACP configuration: {error}")),
    }
}

pub(super) fn save_config(
    local_data_root: &Path,
    user_id: &str,
    config: &AgentAcpConfig,
) -> Result<(), String> {
    let scope = account_scope(user_id)
        .map_err(|_| "Maple ACP configuration requires an authenticated user".to_string())?;
    save_config_for_scope(local_data_root, &scope, config)
}

pub(super) fn save_config_for_scope(
    local_data_root: &Path,
    account_scope: &str,
    config: &AgentAcpConfig,
) -> Result<(), String> {
    let path = config_path_for_scope(local_data_root, account_scope)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Invalid Maple ACP configuration path".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create Maple ACP configuration directory: {error}"))?;
    crate::private_file::set_owner_only_dir(parent)
        .map_err(|error| format!("Failed to secure Maple ACP configuration directory: {error}"))?;
    // Atomic replace: a crash mid-write must not leave a truncated file
    // that `load_config` rejects, which would block `maple-agent acp`.
    crate::private_file::write_private_json(&path, config)
        .map_err(|error| format!("Failed to save Maple ACP configuration: {error}"))?;
    Ok(())
}

pub(super) fn normalize_config(mut config: AgentAcpConfig) -> Result<AgentAcpConfig, String> {
    // `allow_all` was the exploratory Desktop-owned bypass. Caller-owned ACP
    // supersedes it; old files migrate to the guarded policy on their next load.
    config.permission_mode = AgentAcpPermissionMode::ReadOnly;
    config.max_connections = config.max_connections.clamp(1, MAX_ACP_CONNECTIONS);
    let mut roots = Vec::new();
    for root in config.allowed_project_roots {
        let root = root.trim();
        if root.is_empty() {
            continue;
        }
        let path = PathBuf::from(root);
        if !path.is_absolute() {
            return Err("ACP allowed project roots must be absolute paths".to_string());
        }
        let root = path.to_string_lossy().into_owned();
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    config.allowed_project_roots = roots;
    Ok(config)
}
