//! Connecting a catalog app that runs on this computer. Everything an app
//! needs is in its catalog entry, so this is the same for each: check what it
//! requires, start it and list its tools, and only then save it, on for new
//! tasks. What is saved is an ordinary server, which tasks run like any other.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::super::AgentRuntimeHandle;
use super::super::config::account_store;
use super::super::integrations::find_executable;
use super::super::login_path::login_search_path;
use super::super::types::default_mcp_timeout_seconds;
use super::catalog::{McpCatalogEntry, McpCatalogRequirement, mcp_catalog};
use super::connection::McpServer;
use super::oauth::{RunningSignIn, SIGN_IN_CANCELLED};
use super::{
    edit_to_validation_server, load_saved_mcp_servers, name_to_key, normalize_mcp_servers,
};
use crate::store::records::{McpDefinition, McpServerEdit};

/// How long one requirement's check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Something an app needs that this computer does not have.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppMissing {
    pub name: String,
    pub min_version: Option<String>,
    pub get_url: String,
}

impl McpAppMissing {
    pub fn message(&self) -> String {
        match &self.min_version {
            Some(version) => format!("Needs {} {version} or newer.", self.name),
            None => format!("Needs {}.", self.name),
        }
    }

    pub fn action(&self) -> String {
        format!("Get {}", self.name)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum McpAppConnection {
    Connected(Vec<McpDefinition>),
    Missing(McpAppMissing),
}

/// The numbers of the first version in `text`, as in `v24.1.0`.
fn version_numbers(text: &str) -> Option<Vec<u64>> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let version = &text[start..];
    let end = version
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(version.len());
    version[..end]
        .split('.')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().ok())
        .collect()
}

fn at_least(found: &str, minimum: &str) -> bool {
    let (Some(found), Some(minimum)) = (version_numbers(found), version_numbers(minimum)) else {
        return false;
    };
    let at = |numbers: &[u64], index: usize| numbers.get(index).copied().unwrap_or(0);
    for index in 0..found.len().max(minimum.len()) {
        let (have, need) = (at(&found, index), at(&minimum, index));
        if have != need {
            return have > need;
        }
    }
    true
}

async fn requirement_met(requirement: &McpCatalogRequirement, search_path: Option<&str>) -> bool {
    let Some((program, arguments)) = requirement.check.split_first() else {
        return false;
    };
    let Some(program) = find_executable(program, search_path) else {
        return false;
    };
    let mut command = tokio::process::Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = search_path {
        command.env("PATH", path);
    }
    let Ok(Ok(output)) = tokio::time::timeout(CHECK_TIMEOUT, command.output()).await else {
        return false;
    };
    output.status.success()
        && requirement
            .min_version
            .as_deref()
            .is_none_or(|minimum| at_least(&String::from_utf8_lossy(&output.stdout), minimum))
}

/// Start the app and list its tools, then stop it again.
async fn try_starting(
    entry: &McpCatalogEntry,
    edit: &McpServerEdit,
    search_path: Option<String>,
) -> Result<(), String> {
    let mut config = edit_to_validation_server(edit);
    config.timeout_seconds = entry.connect_seconds;
    let folder: PathBuf = std::env::temp_dir();
    let server = McpServer::new(config, &folder, search_path, Arc::new(|| {}));
    let started = server.connect().await;
    server.shutdown().await;
    started.map(|_| ())
}

impl AgentRuntimeHandle {
    /// Connect a local app by its catalog id, the only way a caller can name
    /// what runs. Only public configuration returns to the desktop.
    pub async fn connect_mcp_app(&self, id: &str) -> Result<McpAppConnection, String> {
        let entry = mcp_catalog()
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| "This app is not available here.".to_string())?;
        self.connect_catalog_app(entry).await
    }

    pub(super) async fn connect_catalog_app(
        &self,
        entry: &McpCatalogEntry,
    ) -> Result<McpAppConnection, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let key = name_to_key(&entry.name);
        let target = entry
            .target()
            .ok_or_else(|| "This app is not available here.".to_string())?;
        let edit = McpServerEdit {
            key: key.clone(),
            name: entry.name.clone(),
            description: entry.summary.clone(),
            enabled: true,
            timeout_seconds: default_mcp_timeout_seconds(),
            target,
            environment: Vec::new(),
            headers: Vec::new(),
        };
        let proposed = normalize_mcp_servers(vec![edit_to_validation_server(&edit)])?;
        let (_running, cancel) = RunningSignIn::start(self.sign_in_key(&key));
        let attempt = async {
            let search_path = login_search_path().await;
            for requirement in &entry.requires {
                if !requirement_met(requirement, search_path.as_deref()).await {
                    return Ok(McpAppConnection::Missing(McpAppMissing {
                        name: requirement.name.clone(),
                        min_version: requirement.min_version.clone(),
                        get_url: requirement.get_url.clone(),
                    }));
                }
            }
            try_starting(entry, &edit, search_path)
                .await
                .map_err(|error| {
                    log::debug!("Catalog app {} could not start: {error}", entry.id);
                    entry
                        .help
                        .clone()
                        .unwrap_or_else(|| format!("{} could not start. Try again.", entry.name))
                })?;
            let _settings = self.lock_settings().await;
            self.verify_generation().await?;
            self.ensure_accepting_new_work()?;
            let previous = load_saved_mcp_servers(self.paths(), &self.user_id)?;
            super::super::integrations::validate_new_mcp_integration_collisions(
                &previous, &proposed,
            )?;
            let (store, account) =
                account_store(self.paths(), &self.user_id).map_err(|error| error.to_string())?;
            store
                .add_mcp_definition(account, &edit)
                .map_err(|_| format!("A server named {} already exists.", entry.name))?;
            store
                .mcp_definitions(account)
                .map(McpAppConnection::Connected)
                .map_err(|error| error.to_string())
        };
        tokio::select! {
            result = attempt => result,
            _ = cancel.cancelled() => Err(SIGN_IN_CANCELLED.into()),
        }
    }
}

#[cfg(test)]
mod tests;
