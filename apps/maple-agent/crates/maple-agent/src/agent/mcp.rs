//! The account's MCP servers: their settings, normalized and checked before
//! they are saved (names, commands, endpoints, environment variables and
//! headers), and the servers each task runs, over rmcp, by Pi's conventions.

use std::collections::HashSet;
use std::path::Path;

use super::config::{account_store, normalize_project_root, path_string};
use super::integrations::{is_cua_identity, validate_new_mcp_integration_collisions};
use super::{
    AgentMcpKeyValue, AgentMcpServer, AgentMcpTransport, AgentPathLayout, AgentRuntimeHandle,
};
use crate::store::records::{
    McpDefinition, McpEditOrigin, McpLaunchSecrets, McpSecretEdit, McpSecretUpdate, McpServerEdit,
    McpTarget, SecretString,
};

mod connection;
#[cfg(test)]
pub(super) mod fake_server;
pub(crate) mod oauth;
mod task;
mod tool;

pub(super) use task::{
    MAX_IDLE_TASKS_WITH_SERVERS, STARTUP_WAIT, TaskMcp, read_saved_servers, servers_for_new_task,
    set_chosen_servers, task_servers,
};

const MAX_MCP_SERVER_NAME_CHARS: usize = 64;

/// Apply only this project's explicit switches over the live catalog defaults.
pub(super) fn servers_with_project_defaults(
    paths: &AgentPathLayout,
    user_id: &str,
    project_root: &Path,
    mut servers: Vec<AgentMcpServer>,
) -> Result<Vec<AgentMcpServer>, String> {
    let (store, account) = account_store(paths, user_id).map_err(|error| error.to_string())?;
    if let Some(project) = store
        .project(account, &path_string(project_root))
        .map_err(|error| error.to_string())?
    {
        for server in &mut servers {
            if !is_cua_identity(&server.name)
                && let Some(enabled) = project
                    .mcp_defaults
                    .get(name_to_key(&server.name))
                    .and_then(serde_json::Value::as_bool)
            {
                server.enabled = enabled;
            }
        }
    }
    Ok(servers)
}

/// Keys of Maple's own tool groups, which a server cannot take.
const RESERVED_KEYS: [&str; 2] = ["developer", "maple-skills-extension"];

/// Variables a server's environment cannot set: they change how programs and
/// libraries are found and loaded.
const DISALLOWED_ENVIRONMENT: [&str; 31] = [
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "windir",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "LD_AUDIT",
    "LD_DEBUG",
    "LD_BIND_NOW",
    "LD_ASSUME_KERNEL",
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_FRAMEWORK_PATH",
    "PYTHONPATH",
    "PYTHONHOME",
    "NODE_OPTIONS",
    "RUBYOPT",
    "GEM_PATH",
    "GEM_HOME",
    "CLASSPATH",
    "GO111MODULE",
    "GOROOT",
    "APPINIT_DLLS",
    "SESSIONNAME",
    "ComSpec",
    "TEMP",
    "TMP",
    "LOCALAPPDATA",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
];

/// A server name as a key: lowercase letters, digits, `_` and `-`; spaces
/// are dropped and anything else becomes `_`.
pub(super) fn name_to_key(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

impl AgentMcpServer {
    /// The stable catalog identity used to match a server's display name with
    /// saved project and task choices.
    pub fn key_for_name(name: &str) -> String {
        name_to_key(name)
    }
}

/// A command line's words. Quotes keep spaces in a word; a single quote opens
/// only at the start of one.
pub(super) fn split_command(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let (mut in_double, mut in_single) = (false, false);
    for c in command.chars() {
        match c {
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double && (in_single || current.is_empty()) => in_single = !in_single,
            c if c.is_whitespace() && !in_double && !in_single => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if in_double || in_single {
        return Err("Unmatched quote in command".to_string());
    }
    if !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

fn validate_key_values(
    entries: &mut [AgentMcpKeyValue],
    server_name: &str,
    label: &str,
    case_insensitive: bool,
) -> Result<(), String> {
    let mut keys = HashSet::new();
    for entry in entries {
        entry.key = entry.key.trim().to_string();
        if entry.key.is_empty() {
            return Err(format!(
                "MCP server '{server_name}' has an empty {label} name"
            ));
        }
        if label == "HTTP header" && entry.key.chars().any(char::is_whitespace) {
            return Err(format!(
                "MCP server '{server_name}' HTTP header names cannot contain whitespace"
            ));
        }
        let comparison = if case_insensitive {
            entry.key.to_ascii_lowercase()
        } else {
            entry.key.clone()
        };
        if !keys.insert(comparison) {
            return Err(format!(
                "MCP server '{server_name}' has a duplicate {label} named {}",
                entry.key
            ));
        }
    }
    Ok(())
}

/// The servers trimmed and checked: unique, usable names; a command or an
/// endpoint; a timeout; and environment variables and headers that are named,
/// distinct, and do not change how programs are loaded.
pub(super) fn normalize_mcp_servers(
    mut servers: Vec<AgentMcpServer>,
) -> Result<Vec<AgentMcpServer>, String> {
    let mut names = HashSet::new();
    for server in &mut servers {
        server.name = server.name.trim().to_string();
        server.description = server.description.trim().to_string();
        if server.name.is_empty() {
            return Err("MCP server name cannot be empty".to_string());
        }
        if server.name.chars().count() > MAX_MCP_SERVER_NAME_CHARS {
            return Err(format!(
                "MCP server name '{}' must be 64 characters or fewer",
                server.name
            ));
        }
        let key = name_to_key(&server.name);
        if key.is_empty() {
            return Err(format!(
                "MCP server name '{}' must contain a letter, number, underscore, or hyphen",
                server.name
            ));
        }
        if RESERVED_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "The MCP server name '{}' is reserved by Maple",
                server.name
            ));
        }
        if !names.insert(key) {
            return Err(format!(
                "MCP server name '{}' conflicts with another configured server",
                server.name
            ));
        }
        if server.timeout_seconds == 0 {
            return Err(format!(
                "MCP server '{}' must have a timeout greater than zero",
                server.name
            ));
        }

        let environment = match &mut server.transport {
            AgentMcpTransport::Stdio {
                command,
                environment,
            } => {
                *command = command.trim().to_string();
                if command.is_empty() {
                    return Err(format!("MCP server '{}' requires a command", server.name));
                }
                let words = split_command(command).map_err(|error| {
                    format!(
                        "MCP server '{}' has an invalid command: {error}",
                        server.name
                    )
                })?;
                if words.first().is_none_or(String::is_empty) {
                    return Err(format!(
                        "MCP server '{}' requires an executable",
                        server.name
                    ));
                }
                validate_key_values(environment, &server.name, "environment variable", false)?;
                environment
            }
            AgentMcpTransport::StreamableHttp {
                url,
                environment,
                headers,
            } => {
                *url = url.trim().to_string();
                if url.is_empty() {
                    return Err(format!(
                        "MCP server '{}' requires an endpoint URL",
                        server.name
                    ));
                }
                validate_key_values(environment, &server.name, "environment variable", false)?;
                validate_key_values(headers, &server.name, "HTTP header", true)?;
                environment
            }
        };
        if let Some(entry) = environment.iter().find(|entry| {
            DISALLOWED_ENVIRONMENT
                .iter()
                .any(|disallowed| disallowed.eq_ignore_ascii_case(&entry.key))
        }) {
            return Err(format!(
                "MCP server '{}' cannot override the environment variable {}",
                server.name, entry.key
            ));
        }
    }
    Ok(servers)
}

impl AgentRuntimeHandle {
    /// The account's custom MCP catalog with this project's new-task defaults.
    /// Servers without an explicit project choice inherit the Settings default.
    pub async fn list_project_mcp_servers(
        &self,
        project_root: String,
    ) -> Result<Vec<AgentMcpServer>, String> {
        self.verify_generation().await?;
        let project_root = normalize_project_root(Path::new(&project_root))?;
        let _settings = self.lock_settings().await;
        self.verify_generation().await?;
        servers_with_project_defaults(
            self.paths(),
            &self.user_id,
            &project_root,
            load_saved_mcp_servers(self.paths(), &self.user_id)?,
        )
    }

    /// Pin one custom server's new-task default on this device. This does not
    /// connect the server or alter any existing task's choices.
    pub async fn set_project_mcp_server_enabled(
        &self,
        project_root: String,
        name: String,
        enabled: bool,
    ) -> Result<(), String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let project_root = normalize_project_root(Path::new(&project_root))?;
        let name = name.trim();
        let key = name_to_key(name);
        if key.is_empty() || RESERVED_KEYS.contains(&key.as_str()) || is_cua_identity(name) {
            return Err("That MCP server cannot be changed".to_string());
        }
        let _settings = self.lock_settings().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let servers = load_saved_mcp_servers(self.paths(), &self.user_id)?;
        if !servers
            .iter()
            .any(|server| name_to_key(&server.name) == key)
        {
            return Err(format!("MCP server '{name}' is no longer configured"));
        }
        let (store, account) =
            account_store(self.paths(), &self.user_id).map_err(|error| error.to_string())?;
        let root = path_string(&project_root);
        let mut defaults = store
            .project(account, &root)
            .map_err(|error| error.to_string())?
            .map(|project| project.mcp_defaults)
            .unwrap_or_else(|| serde_json::json!({}));
        defaults
            .as_object_mut()
            .ok_or("Saved MCP defaults are invalid")?
            .insert(key, enabled.into());
        store
            .set_project_mcp_defaults(account, &root, &defaults, super::unix_ms() as i64)
            .map_err(|error| error.to_string())
    }

    /// The account's saved MCP servers, checked as a save checks them.
    pub async fn list_mcp_servers(&self) -> Result<Vec<AgentMcpServer>, String> {
        self.verify_generation().await?;
        let _settings = self.lock_settings().await;
        load_saved_mcp_servers(self.paths(), &self.user_id)
    }

    /// Check and save the account's MCP servers, in place of the saved ones.
    pub async fn save_mcp_servers(
        &self,
        servers: Vec<AgentMcpServer>,
    ) -> Result<Vec<AgentMcpServer>, String> {
        self.save_mcp_servers_from(servers, McpEditOrigin::Local)
            .await
    }

    /// The remote dispatch boundary must call this variant, never select an
    /// edit origin from a controller-supplied parameter.
    pub async fn save_mcp_servers_remote(
        &self,
        servers: Vec<AgentMcpServer>,
    ) -> Result<Vec<AgentMcpServer>, String> {
        self.save_mcp_servers_from(servers, McpEditOrigin::Remote)
            .await
    }

    async fn save_mcp_servers_from(
        &self,
        servers: Vec<AgentMcpServer>,
        origin: McpEditOrigin,
    ) -> Result<Vec<AgentMcpServer>, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let servers = normalize_mcp_servers(servers)?;
        let _settings = self.lock_settings().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        save_saved_mcp_servers(self.paths(), &self.user_id, servers, origin)?;
        load_saved_mcp_servers(self.paths(), &self.user_id)
    }

    /// Name/presence DTO for the revised settings surface. Saved values never
    /// enter the returned serde type. The compatibility projection above uses
    /// empty strings for names, so an unchanged old form means Keep.
    pub async fn list_mcp_definitions(&self) -> Result<Vec<McpDefinition>, String> {
        self.verify_generation().await?;
        let _settings = self.lock_settings().await;
        let (store, account) =
            account_store(self.paths(), &self.user_id).map_err(|error| error.to_string())?;
        store
            .mcp_definitions(account)
            .map_err(|error| error.to_string())
    }

    /// Explicit Keep/Set/Clear patches for the local GUI. The edit type cannot
    /// be serialized back as a response, and Store checks target bindings.
    pub async fn save_mcp_definition_edits(
        &self,
        edits: Vec<McpServerEdit>,
    ) -> Result<Vec<McpDefinition>, String> {
        self.save_mcp_definition_edits_with_origin(edits, McpEditOrigin::Local)
            .await
    }

    /// Called only by the authenticated remote transport dispatcher.
    pub async fn save_mcp_definition_edits_remote(
        &self,
        edits: Vec<McpServerEdit>,
    ) -> Result<Vec<McpDefinition>, String> {
        self.save_mcp_definition_edits_with_origin(edits, McpEditOrigin::Remote)
            .await
    }

    async fn save_mcp_definition_edits_with_origin(
        &self,
        edits: Vec<McpServerEdit>,
        origin: McpEditOrigin,
    ) -> Result<Vec<McpDefinition>, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let _settings = self.lock_settings().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let (store, account) =
            account_store(self.paths(), &self.user_id).map_err(|error| error.to_string())?;
        // Reuse the runtime's executable/environment and built-in-name checks
        // before the typed Store's persistence validation.
        let proposed: Vec<_> = edits.iter().map(edit_to_validation_server).collect();
        let normalized = normalize_mcp_servers(proposed)?;
        let previous = load_saved_mcp_servers(self.paths(), &self.user_id)?;
        validate_new_mcp_integration_collisions(&previous, &normalized)?;
        store
            .save_mcp_definitions(account, &edits, origin)
            .map_err(|error| error.to_string())?;
        store
            .mcp_definitions(account)
            .map_err(|error| error.to_string())
    }
}

/// Configuration readers receive names only; no legacy JSON is inspected.
pub(super) fn load_saved_mcp_servers(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<Vec<AgentMcpServer>, String> {
    let (store, account) = account_store(paths, user_id).map_err(|error| error.to_string())?;
    store
        .mcp_definitions(account)
        .map(|definitions| definitions.into_iter().map(definition_to_server).collect())
        .map_err(|error| error.to_string())
}

fn save_saved_mcp_servers(
    paths: &AgentPathLayout,
    user_id: &str,
    servers: Vec<AgentMcpServer>,
    origin: McpEditOrigin,
) -> Result<(), String> {
    let servers = normalize_mcp_servers(servers)?;
    let previous = load_saved_mcp_servers(paths, user_id)?;
    validate_new_mcp_integration_collisions(&previous, &servers)?;
    let edits: Vec<_> = servers.into_iter().map(server_to_edit).collect();
    let (store, account) = account_store(paths, user_id).map_err(|error| error.to_string())?;
    store
        .save_mcp_definitions(account, &edits, origin)
        .map_err(|error| error.to_string())
}

/// This projection is deliberately private to host-side launch code.
fn load_launch_mcp_servers(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<Vec<AgentMcpServer>, String> {
    let (store, account) = account_store(paths, user_id).map_err(|error| error.to_string())?;
    let servers = store
        .mcp_definitions(account)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|definition| {
            let secrets = store
                .mcp_launch_secrets(account, &definition.key, &definition.target)
                .map_err(|error| error.to_string())?;
            Ok(launch_to_server(secrets))
        })
        .collect::<Result<Vec<_>, String>>()?;
    normalize_mcp_servers(servers)
}

fn definition_to_server(definition: McpDefinition) -> AgentMcpServer {
    let environment = definition
        .environment
        .into_iter()
        .map(|name| AgentMcpKeyValue {
            key: name.name,
            value: String::new(),
        })
        .collect();
    let transport = match definition.target {
        McpTarget::Stdio { command } => AgentMcpTransport::Stdio {
            command,
            environment,
        },
        McpTarget::StreamableHttp { url } => AgentMcpTransport::StreamableHttp {
            url,
            environment,
            headers: definition
                .headers
                .into_iter()
                .map(|name| AgentMcpKeyValue {
                    key: name.name,
                    value: String::new(),
                })
                .collect(),
        },
    };
    AgentMcpServer {
        name: definition.name,
        description: definition.description,
        enabled: definition.enabled,
        timeout_seconds: definition.timeout_seconds,
        transport,
    }
}

fn launch_to_server(launch: McpLaunchSecrets) -> AgentMcpServer {
    let mut server = definition_to_server(launch.definition);
    let environment = launch
        .environment
        .into_iter()
        .map(|(key, value)| AgentMcpKeyValue {
            key,
            value: value.expose().to_owned(),
        })
        .collect();
    match &mut server.transport {
        AgentMcpTransport::Stdio {
            environment: saved, ..
        } => *saved = environment,
        AgentMcpTransport::StreamableHttp {
            environment: saved,
            headers,
            ..
        } => {
            *saved = environment;
            *headers = launch
                .headers
                .into_iter()
                .map(|(key, value)| AgentMcpKeyValue {
                    key,
                    value: value.expose().to_owned(),
                })
                .collect();
        }
    }
    server
}

fn server_to_edit(server: AgentMcpServer) -> McpServerEdit {
    let convert = |values: Vec<AgentMcpKeyValue>| {
        values
            .into_iter()
            .map(|value| McpSecretEdit {
                name: value.key,
                update: if value.value.is_empty() {
                    McpSecretUpdate::Keep
                } else {
                    McpSecretUpdate::Set(SecretString::new(value.value))
                },
            })
            .collect()
    };
    let (target, environment, headers) = match server.transport {
        AgentMcpTransport::Stdio {
            command,
            environment,
        } => (McpTarget::Stdio { command }, convert(environment), vec![]),
        AgentMcpTransport::StreamableHttp {
            url,
            environment,
            headers,
        } => (
            McpTarget::StreamableHttp { url },
            convert(environment),
            convert(headers),
        ),
    };
    McpServerEdit {
        key: name_to_key(&server.name),
        name: server.name,
        description: server.description,
        enabled: server.enabled,
        timeout_seconds: server.timeout_seconds,
        target,
        environment,
        headers,
    }
}

fn edit_to_validation_server(edit: &McpServerEdit) -> AgentMcpServer {
    let values = |edits: &[McpSecretEdit]| {
        edits
            .iter()
            .map(|edit| AgentMcpKeyValue {
                key: edit.name.clone(),
                value: String::new(),
            })
            .collect()
    };
    let transport = match &edit.target {
        McpTarget::Stdio { command } => AgentMcpTransport::Stdio {
            command: command.clone(),
            environment: values(&edit.environment),
        },
        McpTarget::StreamableHttp { url } => AgentMcpTransport::StreamableHttp {
            url: url.clone(),
            environment: values(&edit.environment),
            headers: values(&edit.headers),
        },
    };
    AgentMcpServer {
        name: edit.name.clone(),
        description: edit.description.clone(),
        enabled: edit.enabled,
        timeout_seconds: edit.timeout_seconds,
        transport,
    }
}

#[cfg(test)]
mod tests;
