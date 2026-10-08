//! The account's MCP server settings, normalized and checked before they are
//! saved: names, commands, endpoints, environment variables and headers.
//! Running the servers moves to the Pi runtime with MCP itself.

use std::collections::HashSet;

use super::config::{load_agent_config_inner, save_agent_config_inner};
use super::{AgentMcpKeyValue, AgentMcpServer, AgentMcpTransport, AgentRuntimeHandle};

const MAX_MCP_SERVER_NAME_CHARS: usize = 64;

/// Keys of Maple's own tool groups, which a server cannot take.
const RESERVED_KEYS: [&str; 2] = ["developer", "maple-skills-extension"];

/// Every spelling of the computer use integration's name.
const CUA_NAME: &str = "Computer use (CUA)";
const CUA_NAMES: [&str; 4] = ["cua-driver", CUA_NAME, "Cua Driver", "cua_driver"];

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

fn is_cua_identity(name: &str) -> bool {
    let key = name_to_key(name.trim());
    CUA_NAMES
        .iter()
        .any(|candidate| name_to_key(candidate) == key)
}

/// Refuse a server newly added under the computer use integration's name. A
/// name an earlier release accepted stays saveable, so one old entry cannot
/// make every unrelated change fail.
pub(super) fn validate_new_mcp_integration_collisions(
    previous: &[AgentMcpServer],
    next: &[AgentMcpServer],
) -> Result<(), String> {
    let existing: HashSet<String> = previous
        .iter()
        .map(|server| name_to_key(&server.name))
        .collect();
    match next
        .iter()
        .filter(|server| !existing.contains(&name_to_key(&server.name)))
        .find(|server| is_cua_identity(&server.name))
    {
        Some(server) => Err(format!(
            "Custom MCP server '{}' conflicts with the {CUA_NAME} integration. Rename or remove the custom server before enabling the integration.",
            server.name
        )),
        None => Ok(()),
    }
}

impl AgentRuntimeHandle {
    /// The account's saved MCP servers, checked as a save checks them.
    pub async fn list_mcp_servers(&self) -> Result<Vec<AgentMcpServer>, String> {
        self.verify_generation().await?;
        let _settings = self.lock_settings().await;
        let config = load_agent_config_inner(self.paths(), &self.user_id)
            .map_err(|error| error.to_string())?;
        normalize_mcp_servers(config.mcp_servers)
    }

    /// Check and save the account's MCP servers, in place of the saved ones.
    pub async fn save_mcp_servers(
        &self,
        servers: Vec<AgentMcpServer>,
    ) -> Result<Vec<AgentMcpServer>, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let servers = normalize_mcp_servers(servers)?;
        let _settings = self.lock_settings().await;
        let mut config = load_agent_config_inner(self.paths(), &self.user_id)
            .map_err(|error| error.to_string())?;
        validate_new_mcp_integration_collisions(&config.mcp_servers, &servers)?;
        config.mcp_servers = servers.clone();
        save_agent_config_inner(self.paths(), &self.user_id, &config)
            .map_err(|error| error.to_string())?;
        Ok(servers)
    }
}

#[cfg(test)]
mod tests;
