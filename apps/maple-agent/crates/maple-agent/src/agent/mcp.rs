//! MCP server configuration for a task.
//!
//! Maple accepts MCP servers from two places: a user's saved list, and the
//! transient set an external surface leases for one session. Both are
//! normalized and validated here, turned into Goose extension configs,
//! selected per session, and their connection failures rendered into the
//! one-line warning a host shows.

use super::*;

const MAPLE_CUA_STATE_KEY: &str = "maple_cua";
const MAPLE_CUA_STATE_VERSION: &str = "1";

/// Maple-owned logical CUA selection for one task. The embedded client is
/// intentionally absent from Goose's enabled-extension snapshot because Goose
/// cannot reconstruct a host-owned Rust object after a cold start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CuaSessionState {
    pub(super) backend: AgentIntegrationBackend,
    pub(super) enabled: bool,
}

impl ExtensionState for CuaSessionState {
    const EXTENSION_NAME: &'static str = MAPLE_CUA_STATE_KEY;
    const VERSION: &'static str = MAPLE_CUA_STATE_VERSION;
}

pub(super) fn session_cua_state(session: &Session) -> Option<CuaSessionState> {
    CuaSessionState::from_extension_data(&session.extension_data)
}

pub(super) fn put_session_cua_state(
    extension_data: &mut goose::session::ExtensionData,
    state: CuaSessionState,
) -> Result<(), String> {
    state
        .to_extension_data(extension_data)
        .map_err(|error| format!("Failed to save task CUA setting: {error}"))
}

pub(super) async fn persist_session_cua_state(
    session_manager: &SessionManager,
    session_id: &str,
    state: CuaSessionState,
) -> Result<Session, String> {
    // Reload immediately before writing so a preceding Goose extension
    // mutation cannot be overwritten by a stale ExtensionData snapshot.
    let session = session_manager
        .get_session(session_id, false)
        .await
        .map_err(|error| format!("Failed to load Agent task: {error}"))?;
    let mut extension_data = session.extension_data.clone();
    put_session_cua_state(&mut extension_data, state)?;
    session_manager
        .update(session_id)
        .extension_data(extension_data)
        .apply()
        .await
        .map_err(|error| format!("Failed to persist task CUA setting: {error}"))?;
    session_manager
        .get_session(session_id, false)
        .await
        .map_err(|error| format!("Failed to reload Agent task: {error}"))
}

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
        let key = goose::config::extensions::name_to_key(&server.name);
        if key.is_empty() {
            return Err(format!(
                "MCP server name '{}' must contain a letter, number, underscore, or hyphen",
                server.name
            ));
        }
        if maple_reserved_extension_key(&key) {
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
                let parts = split_mcp_command(command, &server.name)?;
                if parts.is_empty() || parts[0].is_empty() {
                    return Err(format!(
                        "MCP server '{}' requires an executable",
                        server.name
                    ));
                }
                validate_mcp_key_values(environment, &server.name, "environment variable", false)?;
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
                validate_mcp_key_values(environment, &server.name, "environment variable", false)?;
                validate_mcp_key_values(headers, &server.name, "HTTP header", true)?;
                environment
            }
        };

        for entry in environment {
            let accepted = Envs::new(HashMap::from([(entry.key.clone(), entry.value.clone())]))
                .get_env()
                .contains_key(&entry.key);
            if !accepted {
                return Err(format!(
                    "MCP server '{}' cannot override the environment variable {}",
                    server.name, entry.key
                ));
            }
        }
    }

    Ok(servers)
}

pub(super) fn maple_reserved_extension_key(key: &str) -> bool {
    matches!(key, "developer" | MAPLE_SKILLS_CLIENT_KEY)
}

pub(super) fn validate_mcp_key_values(
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
        let comparison_key = if case_insensitive {
            entry.key.to_ascii_lowercase()
        } else {
            entry.key.clone()
        };
        if !keys.insert(comparison_key) {
            return Err(format!(
                "MCP server '{server_name}' has a duplicate {label} named {}",
                entry.key
            ));
        }
    }
    Ok(())
}

pub(super) fn mcp_environment(server: &AgentMcpServer) -> &[AgentMcpKeyValue] {
    match &server.transport {
        AgentMcpTransport::Stdio { environment, .. }
        | AgentMcpTransport::StreamableHttp { environment, .. } => environment,
    }
}

pub(super) fn split_mcp_command(command: &str, server_name: &str) -> Result<Vec<String>, String> {
    goose::utils::split_command_args(command)
        .map_err(|error| format!("MCP server '{server_name}' has an invalid command: {error}"))
}

pub(super) fn mcp_server_to_extension(server: &AgentMcpServer) -> Result<ExtensionConfig, String> {
    let envs = Envs::new(
        mcp_environment(server)
            .iter()
            .map(|entry| (entry.key.clone(), entry.value.clone()))
            .collect(),
    );
    match &server.transport {
        AgentMcpTransport::Stdio { command, .. } => {
            let mut parts = split_mcp_command(command, &server.name)?;
            if parts.is_empty() {
                return Err(format!("MCP server '{}' requires a command", server.name));
            }
            let cmd = parts.remove(0);
            Ok(ExtensionConfig::Stdio {
                name: server.name.clone(),
                description: server.description.clone(),
                cmd,
                args: parts,
                envs,
                env_keys: Vec::new(),
                timeout: Some(server.timeout_seconds),
                cwd: None,
                bundled: Some(false),
                available_tools: Vec::new(),
            })
        }
        AgentMcpTransport::StreamableHttp { url, headers, .. } => {
            Ok(ExtensionConfig::StreamableHttp {
                name: server.name.clone(),
                description: server.description.clone(),
                uri: url.clone(),
                envs,
                env_keys: Vec::new(),
                headers: headers
                    .iter()
                    .map(|entry| (entry.key.clone(), entry.value.clone()))
                    .collect(),
                timeout: Some(server.timeout_seconds),
                socket: None,
                client_id: None,
                client_secret_key: None,
                scopes: Vec::new(),
                bundled: Some(false),
                available_tools: Vec::new(),
            })
        }
    }
}

pub(super) fn normalize_transient_mcp_servers(
    servers: Vec<AgentTransientMcpServer>,
) -> Result<Vec<AgentTransientMcpServer>, String> {
    const MAX_TRANSIENT_MCP_SERVERS: usize = 16;
    const MAX_TRANSIENT_MCP_TOTAL_BYTES: usize = 64 * 1024;
    const MAX_TRANSIENT_MCP_DESCRIPTION_BYTES: usize = 1024;
    const MAX_TRANSIENT_MCP_REQUEST_TIMEOUT_SECONDS: u64 = 30;

    if servers.len() > MAX_TRANSIENT_MCP_SERVERS {
        return Err(format!(
            "An Agent surface may provide at most {MAX_TRANSIENT_MCP_SERVERS} transient MCP servers"
        ));
    }

    let mut keys = HashSet::new();
    let mut normalized = Vec::with_capacity(servers.len());
    for mut server in servers {
        server.name = server.name.trim().to_string();
        server.description = server.description.trim().to_string();
        if server.name.is_empty() || server.name.chars().count() > MAX_MCP_SERVER_NAME_CHARS {
            return Err(
                "Transient MCP server names must be between 1 and 64 characters".to_string(),
            );
        }
        let key = goose::config::extensions::name_to_key(&server.name);
        if key.is_empty() || maple_reserved_extension_key(&key) {
            return Err(format!(
                "The transient MCP server name '{}' is invalid or reserved by Maple",
                server.name
            ));
        }
        if !keys.insert(key) {
            return Err(format!(
                "Transient MCP server '{}' conflicts with another supplied server",
                server.name
            ));
        }
        if server.timeout_seconds == 0 {
            return Err(format!(
                "Transient MCP server '{}' must have a timeout greater than zero",
                server.name
            ));
        }
        if server.description.len() > MAX_TRANSIENT_MCP_DESCRIPTION_BYTES {
            return Err(format!(
                "Transient MCP server '{}' description exceeds the {MAX_TRANSIENT_MCP_DESCRIPTION_BYTES} byte limit",
                server.name
            ));
        }
        server.timeout_seconds = server
            .timeout_seconds
            .min(MAX_TRANSIENT_MCP_REQUEST_TIMEOUT_SECONDS);

        let AgentTransientMcpTransport::StreamableHttp { url, headers } = &mut server.transport;
        *url = url.trim().to_string();
        validate_transient_mcp_url(url, &server.name)?;
        validate_transient_mcp_key_values(
            headers,
            &server.name,
            "HTTP header",
            true,
            MAX_TRANSIENT_MCP_TOTAL_BYTES,
        )?;
        normalized.push(server);
    }
    Ok(normalized)
}

pub(super) fn validate_transient_mcp_key_values(
    entries: &mut [AgentMcpKeyValue],
    server_name: &str,
    label: &str,
    case_insensitive: bool,
    max_total_bytes: usize,
) -> Result<(), String> {
    let mut keys = HashSet::new();
    let mut total_bytes = 0usize;
    for entry in entries {
        entry.key = entry.key.trim().to_string();
        let comparison_key = if case_insensitive {
            entry.key.to_ascii_lowercase()
        } else {
            entry.key.clone()
        };
        if entry.key.is_empty()
            || entry.key.contains(['\0', '='])
            || (label == "HTTP header"
                && (entry.key.chars().any(char::is_whitespace)
                    || entry.value.contains(['\r', '\n'])))
            || entry.value.contains('\0')
            || entry.value.len() > 16 * 1024
            || !keys.insert(comparison_key)
        {
            return Err(format!(
                "Transient MCP server '{server_name}' has an invalid or duplicate {label}"
            ));
        }
        total_bytes = total_bytes
            .checked_add(entry.key.len())
            .and_then(|total| total.checked_add(entry.value.len()))
            .ok_or_else(|| format!("Transient MCP server '{server_name}' metadata is too large"))?;
        if total_bytes > max_total_bytes {
            return Err(format!(
                "Transient MCP server '{server_name}' metadata exceeds the {max_total_bytes} byte limit"
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_transient_mcp_url(url: &str, server_name: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| format!("Transient MCP server '{server_name}' has an invalid URL"))?;
    if parsed.host_str().is_none() {
        return Err(format!(
            "Transient MCP server '{server_name}' URL requires a host"
        ));
    }
    // The ACP client owns this choice: remote MCP servers are a normal part
    // of client-supplied session setups, so any HTTP(S) host is admissible.
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(format!(
            "Transient MCP server '{server_name}' must use HTTP or HTTPS"
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(format!(
            "Transient MCP server '{server_name}' URL cannot contain credentials or a fragment"
        ));
    }
    Ok(())
}

pub(super) fn ensure_extension_sets_do_not_conflict(
    persisted: &[ExtensionConfig],
    transient: &[AgentTransientMcpServer],
) -> Result<(), String> {
    let persisted = persisted
        .iter()
        .map(ExtensionConfig::key)
        .collect::<HashSet<_>>();
    if let Some(conflict) = transient
        .iter()
        .find(|server| persisted.contains(&goose::config::extensions::name_to_key(&server.name)))
    {
        return Err(format!(
            "Transient MCP server '{}' conflicts with this task's persisted MCP configuration",
            conflict.name
        ));
    }
    Ok(())
}

pub(super) async fn install_transient_mcp_router(
    tool_context: &SharedAgentToolContext,
    session: &Session,
    servers: Vec<AgentTransientMcpServer>,
    setup_cancel: &CancellationToken,
) -> Result<(), String> {
    if servers.is_empty() {
        return Ok(());
    }

    let configs = servers
        .into_iter()
        .map(|server| {
            let AgentTransientMcpTransport::StreamableHttp { url, headers } = server.transport;
            TransientMcpConfig {
                name: server.name,
                session_id: session.id.clone(),
                request_timeout: std::time::Duration::from_secs(server.timeout_seconds),
                url,
                headers: headers
                    .into_iter()
                    .map(|entry| (entry.key, entry.value))
                    .collect(),
            }
        })
        .collect();
    let lease_cancel = tool_context.lifetime_token();
    let connect = TransientMcpRouter::connect(configs, lease_cancel.clone());
    tokio::pin!(connect);
    let router = tokio::select! {
        biased;
        _ = setup_cancel.cancelled() => {
            tool_context.revoke();
            return Err("Transient MCP setup was cancelled".to_string());
        }
        _ = lease_cancel.cancelled() => {
            return Err("Agent tool context was revoked during MCP setup".to_string());
        }
        result = &mut connect => result.map_err(|error| format!("Failed to connect transient MCP: {error}"))?,
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            tool_context.revoke();
            return Err("Transient MCP setup timed out".to_string());
        }
    };
    if let Err(error) = tool_context.install_transient_mcp(router.clone()) {
        router.shutdown().await;
        return Err(error);
    }
    Ok(())
}

pub(super) fn select_mcp_servers(
    configured: &[AgentMcpServer],
    requested_names: Option<&[String]>,
) -> Result<Vec<AgentMcpServer>, String> {
    let Some(requested_names) = requested_names else {
        return Ok(configured
            .iter()
            .filter(|server| server.enabled)
            .cloned()
            .collect());
    };
    let configured_by_key = configured
        .iter()
        .map(|server| (goose::config::extensions::name_to_key(&server.name), server))
        .collect::<HashMap<_, _>>();
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for requested_name in requested_names {
        let key = goose::config::extensions::name_to_key(requested_name.trim());
        if !seen.insert(key.clone()) {
            continue;
        }
        let server = configured_by_key.get(&key).ok_or_else(|| {
            format!(
                "MCP server '{}' is no longer configured. Reopen the MCP menu and try again.",
                requested_name.trim()
            )
        })?;
        selected.push((*server).clone());
    }
    Ok(selected)
}

pub(super) fn mcp_extension_keys(configs: &[ExtensionConfig]) -> HashSet<String> {
    configs
        .iter()
        .filter(|config| mcp_transport_label(config).is_some())
        .map(ExtensionConfig::key)
        .collect()
}

pub(super) fn session_mcp_extension_keys(session: &Session) -> HashSet<String> {
    goose::session::EnabledExtensionsState::from_extension_data(&session.extension_data)
        .map(|state| mcp_extension_keys(&state.extensions))
        .unwrap_or_default()
}

pub(super) fn mcp_connection_errors(
    results: Vec<goose::agents::ExtensionLoadResult>,
    mcp_keys: &HashSet<String>,
) -> Vec<AgentMcpConnectionError> {
    results
        .into_iter()
        .filter_map(|result| {
            (!result.success
                && mcp_keys.contains(&goose::config::extensions::name_to_key(&result.name)))
            .then(|| AgentMcpConnectionError {
                name: result.name,
                error: result
                    .error
                    .unwrap_or_else(|| "Connection failed".to_string()),
            })
        })
        .collect()
}

pub(super) fn format_mcp_connection_errors(errors: &[AgentMcpConnectionError]) -> String {
    let mut details = errors
        .iter()
        .take(MAX_MCP_CONNECTION_ERRORS)
        .map(|error| {
            format!(
                "{}: {}",
                bounded_timeline_text(&error.name, MAX_MCP_SERVER_NAME_CHARS),
                bounded_timeline_text(&error.error, MAX_MCP_CONNECTION_ERROR_CHARS)
            )
        })
        .collect::<Vec<_>>();
    let remaining = errors.len().saturating_sub(details.len());
    if remaining > 0 {
        details.push(format!("and {remaining} more"));
    }
    bounded_timeline_text(
        &format!("{MCP_CONNECTION_ERROR_PREFIX} {}", details.join("; ")),
        MAX_AGENT_ERROR_CHARS,
    )
}

pub(super) fn mcp_transport_label(config: &ExtensionConfig) -> Option<&'static str> {
    match config {
        ExtensionConfig::Stdio { .. } => Some("stdio"),
        ExtensionConfig::StreamableHttp { .. } => Some("streamable_http"),
        _ => None,
    }
}

pub(super) fn mcp_extension_description(config: &ExtensionConfig) -> String {
    match config {
        ExtensionConfig::Stdio { description, .. }
        | ExtensionConfig::StreamableHttp { description, .. } => description.clone(),
        _ => String::new(),
    }
}

pub(super) fn session_mcp_servers(
    configured: &[AgentMcpServer],
    session: &Session,
) -> Vec<AgentSessionMcpServer> {
    let active =
        goose::session::EnabledExtensionsState::from_extension_data(&session.extension_data)
            .map(|state| state.extensions)
            .unwrap_or_default();
    let active_keys = active
        .iter()
        .filter(|config| mcp_transport_label(config).is_some())
        .map(ExtensionConfig::key)
        .collect::<HashSet<_>>();
    let mut entries = configured
        .iter()
        .map(|server| AgentSessionMcpServer {
            name: server.name.clone(),
            kind: AgentSessionIntegrationKind::Mcp,
            display_name: server.name.clone(),
            description: server.description.clone(),
            transport: match server.transport {
                AgentMcpTransport::Stdio { .. } => "stdio",
                AgentMcpTransport::StreamableHttp { .. } => "streamable_http",
            }
            .to_string(),
            enabled: active_keys.contains(&goose::config::extensions::name_to_key(&server.name)),
            available: true,
        })
        .collect::<Vec<_>>();
    let configured_keys = configured
        .iter()
        .map(|server| goose::config::extensions::name_to_key(&server.name))
        .collect::<HashSet<_>>();
    entries.extend(active.iter().filter_map(|config| {
        let transport = mcp_transport_label(config)?;
        (!configured_keys.contains(&config.key())).then(|| AgentSessionMcpServer {
            name: config.name(),
            kind: AgentSessionIntegrationKind::Mcp,
            display_name: config.name(),
            description: mcp_extension_description(config),
            transport: transport.to_string(),
            enabled: true,
            available: false,
        })
    }));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cua_task_metadata_coexists_with_goose_extension_state() {
        let mut extension_data = goose::session::ExtensionData::new();
        goose::session::EnabledExtensionsState::new(Vec::new())
            .to_extension_data(&mut extension_data)
            .unwrap();
        let state = CuaSessionState {
            backend: AgentIntegrationBackend::Embedded,
            enabled: true,
        };

        put_session_cua_state(&mut extension_data, state).unwrap();

        assert_eq!(
            CuaSessionState::from_extension_data(&extension_data),
            Some(state)
        );
        assert!(
            goose::session::EnabledExtensionsState::from_extension_data(&extension_data).is_some()
        );
    }
}
