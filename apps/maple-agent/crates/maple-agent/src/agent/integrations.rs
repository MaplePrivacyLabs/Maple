//! Maple-curated, device-local integrations.
//!
//! This module owns the product-level catalog, validation, device-local
//! default, and migration between an installed external MCP server and a
//! Maple-hosted implementation. A task freezes that backend choice when it is
//! created; account defaults never rewrite existing tasks.

use super::external_agents::codex::{self, CodexDetection};
use super::*;
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::time::Duration;

pub(super) const CUA_DRIVER_INTEGRATION_ID: &str = "cua-driver";
/// The name Goose shows for the extension and that Maple uses when it has to
/// talk about the integration in an error.
pub(super) const CUA_DRIVER_NAME: &str = "Computer use (CUA)";
pub(super) const CUA_DRIVER_MCP_NAME: &str = "cua-driver";
#[cfg(embedded_cua)]
pub(super) const CUA_DRIVER_DESCRIPTION: &str =
    "Let models view and control desktop applications using CUA built into Maple.";
/// What the Integrations page shows. The catalog owns this copy so the page
/// renders what it is given instead of keeping a second set of strings.
const CUA_DRIVER_CARD_NAME: &str = "Cua";
const CUA_DRIVER_CARD_DESCRIPTION: &str = "Let Maple see and control apps on this computer.";
const CUA_EXTERNAL_MCP_DESCRIPTION: &str =
    "Control desktop applications through the locally installed Cua Driver.";
#[cfg(target_os = "macos")]
const CUA_DRIVER_MACOS_BINARY: &str = "/Applications/CuaDriver.app/Contents/MacOS/cua-driver";
/// The Codex CLI as an external agent a task can hand work to.
pub(super) const CODEX_INTEGRATION_ID: &str = "codex";
const CODEX_CARD_NAME: &str = "Codex";
const CODEX_CARD_DESCRIPTION: &str =
    "Let a task hand work to the Codex CLI installed on this computer, with its own account.";
/// Provider metadata shared by Settings, task selection, and tool admission.
/// Add a descriptor and the provider's detection/transport adapter to extend
/// delegation; task persistence and the composer need no provider-specific code.
pub(super) struct ExternalAgentIntegration {
    pub(super) id: &'static str,
    pub(super) name: &'static str,
    pub(super) description: &'static str,
    project: fn(&IntegrationDetections, Option<&StoredIntegration>) -> AgentIntegration,
}

pub(super) const EXTERNAL_AGENT_INTEGRATIONS: &[ExternalAgentIntegration] =
    &[ExternalAgentIntegration {
        id: CODEX_INTEGRATION_ID,
        name: CODEX_CARD_NAME,
        description: CODEX_CARD_DESCRIPTION,
        project: |detections, stored| codex_public(&detections.codex, stored),
    }];

pub(super) fn external_agent_selection(id: &str) -> Option<&'static ExternalAgentIntegration> {
    EXTERNAL_AGENT_INTEGRATIONS
        .iter()
        .find(|entry| entry.id == id)
}

#[derive(Default, Serialize, Deserialize)]
struct TaskIntegrationOverrides {
    enabled: std::collections::BTreeMap<String, bool>,
}

impl ExtensionState for TaskIntegrationOverrides {
    const EXTENSION_NAME: &'static str = "maple_integrations";
    const VERSION: &'static str = "1";
}

fn session_external_agent_enabled(
    stored: &StoredIntegrationRegistry,
    session: &Session,
    id: &str,
) -> bool {
    // Missing metadata is inheritance, including tasks created before this
    // integration existed. Never snapshot an inherited false into old tasks.
    TaskIntegrationOverrides::from_extension_data(&session.extension_data)
        .and_then(|state| state.enabled.get(id).copied())
        .unwrap_or_else(|| {
            stored
                .integrations
                .iter()
                .any(|entry| entry.id == id && entry.enabled)
        })
}

pub(super) fn session_external_agent_providers(
    stored: &StoredIntegrationRegistry,
    session: &Session,
    desktop: bool,
) -> Vec<String> {
    if !desktop || session.session_type != SessionType::User {
        return Vec::new();
    }
    EXTERNAL_AGENT_INTEGRATIONS
        .iter()
        .filter(|entry| session_external_agent_enabled(stored, session, entry.id))
        .map(|entry| entry.id.to_string())
        .collect()
}

pub(super) async fn persist_task_integration_override(
    manager: &SessionManager,
    session_id: &str,
    id: &str,
    enabled: bool,
) -> Result<Session, String> {
    let session = manager
        .get_session(session_id, false)
        .await
        .map_err(|error| format!("Failed to load Agent task: {error}"))?;
    let mut state =
        TaskIntegrationOverrides::from_extension_data(&session.extension_data).unwrap_or_default();
    state.enabled.insert(id.to_string(), enabled);
    let mut data = session.extension_data;
    state
        .to_extension_data(&mut data)
        .map_err(|error| format!("Failed to save task integration: {error}"))?;
    manager
        .update(session_id)
        .extension_data(data)
        .apply()
        .await
        .map_err(|error| format!("Failed to save task integration: {error}"))?;
    manager
        .get_session(session_id, false)
        .await
        .map_err(|error| format!("Failed to reload Agent task: {error}"))
}

/// Frontmatter line that marks a skill file as Maple's, so disabling the
/// integration removes only what enabling it wrote.
const EXTERNAL_AGENT_SKILL_MARKER: &str = "maple: external-agents";
/// The skills that teach a task how to delegate. They are installed into
/// the account's Goose skills directory, which both the composer's `/`
/// list and the `load_skill` tool already scan.
const EXTERNAL_AGENT_SKILLS: [(&str, &str); 3] = [
    (
        "handoff",
        include_str!("../../resources/skills/handoff/SKILL.md"),
    ),
    (
        "committee",
        include_str!("../../resources/skills/committee/SKILL.md"),
    ),
    (
        "advisor",
        include_str!("../../resources/skills/advisor/SKILL.md"),
    ),
];
const INTEGRATIONS_FILE_NAME: &str = "integrations.json";
const INTEGRATIONS_FILE_VERSION: u32 = 2;
const LEGACY_INTEGRATIONS_FILE_VERSION: u32 = 1;
#[cfg(any(target_os = "macos", test))]
const CUA_MANIFEST_SCHEMA_VERSION: &str = "1";
#[cfg(target_os = "macos")]
const CUA_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(target_os = "macos")]
const MAX_CUA_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoredIntegrationRegistry {
    version: u32,
    #[serde(default)]
    integrations: Vec<StoredIntegration>,
}

impl Default for StoredIntegrationRegistry {
    fn default() -> Self {
        Self {
            version: INTEGRATIONS_FILE_VERSION,
            integrations: Vec::new(),
        }
    }
}

impl StoredIntegrationRegistry {
    fn cua(&self) -> Option<&StoredIntegration> {
        self.integrations
            .iter()
            .find(|entry| entry.id == CUA_DRIVER_INTEGRATION_ID)
    }

    fn cua_mut(&mut self) -> Option<&mut StoredIntegration> {
        self.integrations
            .iter_mut()
            .find(|entry| entry.id == CUA_DRIVER_INTEGRATION_ID)
    }
}

/// Everything discovered about the curated integrations on this device.
#[derive(Debug)]
pub(super) struct IntegrationDetections {
    pub(super) cua: CuaDetection,
    pub(super) codex: CodexDetection,
}

/// The Integrations card for Codex. Availability comes from the
/// installation alone; sign-in is reported on the card but does not gate
/// enabling, because the tool itself says what to do when it is missing.
fn codex_public(
    detection: &CodexDetection,
    stored: Option<&StoredIntegration>,
) -> AgentIntegration {
    let availability = match (&detection.executable, &detection.problem) {
        (None, _) => AgentIntegrationAvailability::NotDetected,
        (Some(_), Some(_)) => AgentIntegrationAvailability::SetupRequired,
        (Some(_), None) => AgentIntegrationAvailability::Available,
    };
    let detail = match (
        &detection.executable,
        &detection.problem,
        detection.signed_in,
    ) {
        (None, _, _) => Some(
            "Install the Codex CLI and make sure `codex` is on your PATH, then reopen this page."
                .to_string(),
        ),
        (Some(_), Some(problem), _) => Some(problem.clone()),
        (Some(_), None, Some(false)) => Some(codex::sign_in_hint().to_string()),
        (Some(_), None, Some(true)) => Some("Signed in.".to_string()),
        (Some(_), None, None) => None,
    };
    AgentIntegration {
        id: CODEX_INTEGRATION_ID.to_string(),
        name: CODEX_CARD_NAME.to_string(),
        description: CODEX_CARD_DESCRIPTION.to_string(),
        availability,
        backend: stored.map(|entry| entry.backend),
        version: detection.version.clone(),
        standalone_version: None,
        permissions: None,
        setup_available: false,
        enabled_for_new_tasks: stored.is_some_and(|entry| entry.enabled),
        detail,
    }
}

/// Whether tasks of this account may delegate to external agents. Read
/// on a path that must keep working, so an unusable file reads as off.
pub(super) fn external_agents_enabled(paths: &AgentPathLayout, user_id: &str) -> bool {
    let stored = stored_integrations_for_read(paths, user_id);
    EXTERNAL_AGENT_INTEGRATIONS.iter().any(|provider| {
        stored
            .integrations
            .iter()
            .any(|entry| entry.id == provider.id && entry.enabled)
    })
}

/// The slash commands for the skills Maple installed in the account's
/// Goose skills directory. A minimal frontmatter read: `name`,
/// `description`, and `argument-hint`, which is all the composer shows.
pub(super) fn account_skill_commands(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Vec<AgentSlashCommand> {
    let Ok(root) = external_agent_skills_dir(paths, user_id) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut commands = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| fs::read_to_string(entry.path().join("SKILL.md")).ok())
        .filter_map(|content| skill_command_from_frontmatter(&content))
        .collect::<Vec<_>>();
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

fn skill_command_from_frontmatter(content: &str) -> Option<AgentSlashCommand> {
    let body = content.trim_start().strip_prefix("---")?;
    let (frontmatter, _) = body.split_once("\n---")?;
    let field = |key: &str| {
        frontmatter.lines().find_map(|line| {
            let (found, value) = line.split_once(':')?;
            (found.trim() == key).then(|| value.trim().trim_matches('"').to_string())
        })
    };
    let name = field("name")?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(AgentSlashCommand {
        name,
        description: field("description").unwrap_or_default(),
        input_hint: field("argument-hint").filter(|hint| !hint.is_empty()),
    })
}

fn external_agent_skills_dir(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, String> {
    Ok(account_config_dir_path(paths, user_id)
        .map_err(|error| error.to_string())?
        .join("goose")
        .join("config")
        .join("skills"))
}

/// Install or remove the delegation skills so they match the toggle. Only
/// files that carry Maple's marker are ever removed.
pub(super) fn sync_external_agent_skills(
    paths: &AgentPathLayout,
    user_id: &str,
    enabled: bool,
) -> Result<(), String> {
    let root = external_agent_skills_dir(paths, user_id)?;
    for (name, content) in EXTERNAL_AGENT_SKILLS {
        debug_assert!(content.contains(EXTERNAL_AGENT_SKILL_MARKER));
        let dir = root.join(name);
        let file = dir.join("SKILL.md");
        if enabled {
            let current = fs::read_to_string(&file).ok();
            if current.as_deref() == Some(content) {
                continue;
            }
            if current.is_some_and(|current| !current.contains(EXTERNAL_AGENT_SKILL_MARKER)) {
                log::warn!(
                    "Leaving the user's own skill in place at {}",
                    file.display()
                );
                continue;
            }
            crate::private_file::write_private_file(&file, content.as_bytes())
                .map_err(|error| format!("Failed to install the {name} skill: {error}"))?;
            set_owner_only_dir_permissions(&dir);
        } else {
            let Ok(current) = fs::read_to_string(&file) else {
                continue;
            };
            if !current.contains(EXTERNAL_AGENT_SKILL_MARKER) {
                continue;
            }
            fs::remove_file(&file)
                .map_err(|error| format!("Failed to remove the {name} skill: {error}"))?;
            // Only the directory Maple made; a user's extra files keep it.
            let _ = fs::remove_dir(&dir);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredIntegration {
    id: String,
    enabled: bool,
    backend: AgentIntegrationBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    external_server: Option<AgentMcpServer>,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyStoredIntegrationRegistry {
    version: u32,
    #[serde(default)]
    integrations: Vec<LegacyStoredIntegration>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyStoredIntegration {
    id: String,
    server: AgentMcpServer,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Deserialize)]
struct CuaManifest {
    schema_version: String,
    binary_path: String,
    binary_version: String,
    mcp_invocation: CuaMcpInvocation,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Deserialize)]
struct CuaMcpInvocation {
    command: String,
    args: Vec<String>,
}

#[derive(Debug)]
pub(super) struct CuaDetection {
    availability: AgentIntegrationAvailability,
    permissions: Option<AgentIntegrationPermissions>,
    setup_available: bool,
    standalone_version: Option<String>,
    detail: Option<String>,
    external_server: Option<AgentMcpServer>,
}

impl CuaDetection {
    #[cfg(any(not(embedded_cua), test))]
    fn not_detected() -> Self {
        Self {
            availability: AgentIntegrationAvailability::NotDetected,
            permissions: None,
            setup_available: false,
            standalone_version: None,
            detail: Some("Built-in CUA is not available on this operating system yet.".to_string()),
            external_server: None,
        }
    }

    fn embedded_ready(&self) -> bool {
        self.permissions
            .as_ref()
            .is_some_and(AgentIntegrationPermissions::ready)
    }

    fn public(&self, stored: Option<&StoredIntegration>) -> AgentIntegration {
        AgentIntegration {
            id: CUA_DRIVER_INTEGRATION_ID.to_string(),
            name: CUA_DRIVER_CARD_NAME.to_string(),
            description: CUA_DRIVER_CARD_DESCRIPTION.to_string(),
            availability: self.availability,
            backend: stored.map(|entry| entry.backend),
            version: embedded_cua_version(),
            standalone_version: self.standalone_version.clone(),
            permissions: self.permissions.clone(),
            setup_available: self.setup_available,
            enabled_for_new_tasks: stored.is_some_and(|entry| entry.enabled),
            detail: self.detail.clone(),
        }
    }
}

#[cfg(embedded_cua)]
fn embedded_cua_version() -> Option<String> {
    Some(super::cua::EMBEDDED_CUA_VERSION.to_string())
}

#[cfg(not(embedded_cua))]
fn embedded_cua_version() -> Option<String> {
    None
}

/// Discover every curated integration. `codex_search_path` is the PATH
/// to look on for the Codex CLI, when the host knows a fuller one than
/// the process environment (a macOS GUI launch).
pub(super) async fn detect_integrations(codex_search_path: Option<&str>) -> IntegrationDetections {
    IntegrationDetections {
        cua: detect_cua_driver().await,
        codex: codex::detect(codex_search_path).await,
    }
}

/// The integrations Maple curates. Every entry point that accepts an
/// integration id checks it here so they cannot disagree.
pub(super) fn require_known_integration(id: &str) -> Result<(), String> {
    if id.trim() == CUA_DRIVER_INTEGRATION_ID
        || EXTERNAL_AGENT_INTEGRATIONS
            .iter()
            .any(|entry| entry.id == id.trim())
    {
        return Ok(());
    }
    Err(format!("Unknown integration '{}'", id.trim()))
}

pub(super) fn project_integrations(
    paths: &AgentPathLayout,
    user_id: &str,
    detections: &IntegrationDetections,
) -> Result<Vec<AgentIntegration>, String> {
    let stored = load_stored_integrations(paths, user_id)?;
    let mut cards = vec![detections.cua.public(stored.cua())];
    cards.extend(EXTERNAL_AGENT_INTEGRATIONS.iter().map(|provider| {
        (provider.project)(
            detections,
            stored
                .integrations
                .iter()
                .find(|entry| entry.id == provider.id),
        )
    }));
    Ok(cards)
}

pub(super) fn set_integration_default(
    paths: &AgentPathLayout,
    user_id: &str,
    request: &AgentSetIntegrationEnabledRequest,
    detections: &IntegrationDetections,
) -> Result<Vec<AgentIntegration>, String> {
    require_known_integration(&request.id)?;
    if let Some(provider) = EXTERNAL_AGENT_INTEGRATIONS
        .iter()
        .find(|entry| entry.id == request.id.trim())
    {
        set_external_agent_default(paths, user_id, request.enabled, provider, detections)?;
    } else {
        set_cua_default(paths, user_id, request.enabled, &detections.cua)?;
    }
    project_integrations(paths, user_id, detections)
}

/// Enabling needs a usable installation; disabling never fails on the
/// installation, so a removed Codex can still be switched off.
fn set_external_agent_default(
    paths: &AgentPathLayout,
    user_id: &str,
    enabled: bool,
    provider: &ExternalAgentIntegration,
    detections: &IntegrationDetections,
) -> Result<(), String> {
    let mut stored = load_stored_integrations(paths, user_id)?;
    if enabled {
        let card = (provider.project)(detections, None);
        if card.availability != AgentIntegrationAvailability::Available {
            return Err(card
                .detail
                .unwrap_or_else(|| format!("Set up {} in Integrations first", provider.name)));
        }
    }
    match stored
        .integrations
        .iter_mut()
        .find(|entry| entry.id == provider.id)
    {
        Some(entry) => entry.enabled = enabled,
        None if enabled => stored.integrations.push(StoredIntegration {
            id: provider.id.to_string(),
            enabled,
            backend: AgentIntegrationBackend::Embedded,
            external_server: None,
        }),
        None => {}
    }
    save_stored_integrations(paths, user_id, &stored)?;
    sync_external_agent_skills(paths, user_id, external_agents_enabled(paths, user_id))
}

pub(super) fn project_task_provider_availability(
    rows: &mut [AgentSessionMcpServer],
    detections: &IntegrationDetections,
) {
    for row in rows {
        if row.kind == AgentSessionIntegrationKind::ExternalAgent
            && let Some(provider) = external_agent_selection(&row.name)
        {
            let card = (provider.project)(detections, None);
            row.available = card.availability == AgentIntegrationAvailability::Available;
        }
    }
}

fn set_cua_default(
    paths: &AgentPathLayout,
    user_id: &str,
    enabled: bool,
    detection: &CuaDetection,
) -> Result<(), String> {
    let custom = normalize_mcp_servers(
        load_agent_config_inner(paths, user_id)
            .map_err(|error| format!("Failed to load MCP servers: {error}"))?
            .mcp_servers,
    )?;
    let mut stored = load_stored_integrations(paths, user_id)?;

    if enabled {
        ensure_no_custom_integration_collision(&custom)?;
        match stored.cua_mut() {
            Some(entry) => {
                match entry.backend {
                    AgentIntegrationBackend::Embedded if !detection.embedded_ready() => {
                        return Err(
                            "Set up Maple's Accessibility and Screen Recording permissions before enabling built-in CUA"
                                .to_string(),
                        );
                    }
                    AgentIntegrationBackend::External => {
                        let server = detection.external_server.clone().ok_or_else(|| {
                            "The standalone CuaDriver selected by this setting is no longer available. Set up built-in CUA instead."
                                .to_string()
                        })?;
                        entry.external_server = Some(server);
                    }
                    AgentIntegrationBackend::Embedded => {}
                }
                entry.enabled = true;
            }
            None if detection.embedded_ready() => {
                stored.integrations.push(StoredIntegration {
                    id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                    enabled: true,
                    backend: AgentIntegrationBackend::Embedded,
                    external_server: detection.external_server.clone(),
                });
            }
            None => {
                let server = detection.external_server.clone().ok_or_else(|| {
                    "Set up Maple's Accessibility and Screen Recording permissions before enabling CUA"
                        .to_string()
                })?;
                stored.integrations.push(StoredIntegration {
                    id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                    enabled: true,
                    backend: AgentIntegrationBackend::External,
                    external_server: Some(server),
                });
            }
        }
    } else if let Some(entry) = stored.cua_mut() {
        entry.enabled = false;
    }

    save_stored_integrations(paths, user_id, &stored)
}

/// Switch the new-task default to Maple's embedded backend only after the OS
/// reports both grants. An incomplete setup never silently takes a working
/// external backend away from the user.
pub(super) fn select_embedded_integration_backend(
    paths: &AgentPathLayout,
    user_id: &str,
    detections: &IntegrationDetections,
) -> Result<Vec<AgentIntegration>, String> {
    let detection = &detections.cua;
    if !detection.embedded_ready() {
        return project_integrations(paths, user_id, detections);
    }
    let custom = normalize_mcp_servers(
        load_agent_config_inner(paths, user_id)
            .map_err(|error| format!("Failed to load MCP servers: {error}"))?
            .mcp_servers,
    )?;
    ensure_no_custom_integration_collision(&custom)?;
    let mut stored = load_stored_integrations(paths, user_id)?;
    match stored.cua_mut() {
        Some(entry) => {
            entry.backend = AgentIntegrationBackend::Embedded;
            if detection.external_server.is_some() {
                entry.external_server = detection.external_server.clone();
            }
        }
        None => stored.integrations.push(StoredIntegration {
            id: CUA_DRIVER_INTEGRATION_ID.to_string(),
            enabled: false,
            backend: AgentIntegrationBackend::Embedded,
            external_server: detection.external_server.clone(),
        }),
    }
    save_stored_integrations(paths, user_id, &stored)?;
    project_integrations(paths, user_id, detections)
}

pub(super) fn effective_mcp_servers(
    stored: &StoredIntegrationRegistry,
    custom: Vec<AgentMcpServer>,
) -> Result<Vec<AgentMcpServer>, String> {
    let mut servers = custom;
    if let Some(entry) = stored.cua() {
        // The integration owns this product identity even when the embedded
        // backend has no external server definition. A custom server that a
        // previous release accepted under any historical spelling is shadowed
        // before Goose can select or start it. Merging would make
        // normalization fail and lock the account out of every task, while
        // starting it briefly before embedded CUA replaces it would cross the
        // explicit backend boundary.
        servers.retain(|candidate| !is_cua_identity(&candidate.name));
        let Some(server) = entry.external_server.as_ref() else {
            return normalize_mcp_servers(servers);
        };
        let mut server = server.clone();
        // Keep the concrete external definition available to old tasks,
        // but only select it by default while External owns the global
        // default. Embedded sessions are represented in Maple metadata.
        server.enabled = entry.enabled && entry.backend == AgentIntegrationBackend::External;
        servers.push(server);
    }
    normalize_mcp_servers(servers)
}

/// Whether a configured server name addresses the curated CUA integration.
pub(super) fn is_cua_key(name: &str) -> bool {
    is_cua_identity(name)
}

/// Whether a configured server uses any spelling reserved for curated CUA.
fn is_cua_identity(name: &str) -> bool {
    let key = goose::config::extensions::name_to_key(name.trim());
    [
        CUA_DRIVER_MCP_NAME,
        CUA_DRIVER_NAME,
        "Cua Driver",
        "cua_driver",
    ]
    .into_iter()
    .any(|candidate| goose::config::extensions::name_to_key(candidate) == key)
}

/// Read the device-local registry for a path that must keep working.
///
/// Task creation and the composer MCP menu must not fail because an optional
/// device-local file was written by a newer build or damaged. Those paths get
/// an empty registry and a log line instead of an error. None of them writes
/// the file, so the stored choice is never clobbered and the Integrations page
/// still reports the real problem.
pub(super) fn stored_integrations_for_read(
    paths: &AgentPathLayout,
    user_id: &str,
) -> StoredIntegrationRegistry {
    match load_stored_integrations(paths, user_id) {
        Ok(registry) => registry,
        Err(error) => {
            log::warn!("Ignoring unusable device-local integration settings: {error}");
            StoredIntegrationRegistry::default()
        }
    }
}

/// Freeze the device default into a newly-created task. Explicit server names
/// override the enabled-by-default bit, matching custom MCP selection. The
/// backend itself always comes from the Maple-managed registry and cannot be
/// supplied by an external caller.
pub(super) fn cua_state_for_new_session(
    stored: &StoredIntegrationRegistry,
    requested_names: Option<&[String]>,
    allow_embedded: bool,
) -> Result<Option<CuaSessionState>, String> {
    let Some(entry) = stored.cua() else {
        return Ok(None);
    };
    let explicitly_requested =
        requested_names.map(|names| names.iter().any(|name| is_cua_key(name)));
    let enabled = explicitly_requested.unwrap_or(entry.enabled);
    if entry.backend == AgentIntegrationBackend::Embedded && !allow_embedded {
        if explicitly_requested == Some(true) {
            return Err(
                "Built-in CUA is available only to tasks running in the Maple desktop app"
                    .to_string(),
            );
        }
        return Ok(None);
    }
    Ok(Some(CuaSessionState {
        backend: entry.backend,
        enabled,
    }))
}

pub(super) fn requested_mcp_names_without_embedded_cua(
    requested_names: Option<&[String]>,
    cua_state: Option<CuaSessionState>,
) -> Option<Vec<String>> {
    requested_names.map(|names| {
        names
            .iter()
            .filter(|name| {
                cua_state.is_none_or(|state| {
                    state.backend != AgentIntegrationBackend::Embedded || !is_cua_key(name)
                })
            })
            .cloned()
            .collect()
    })
}

/// Which backend a task uses, in the order the answer becomes authoritative.
///
/// A task that recorded its own choice keeps it. A task that predates that
/// metadata but holds the concrete external extension is unambiguous. Only a
/// task that never expressed a choice falls back to the device default, and
/// only when it could actually run that default: a task outside the desktop
/// app never adopts the embedded backend, because it cannot use it.
pub(super) fn session_cua_backend(
    stored: &StoredIntegrationRegistry,
    session: &Session,
) -> Option<AgentIntegrationBackend> {
    if let Some(state) = session_cua_state(session) {
        return Some(state.backend);
    }
    if session_mcp_extension_keys(session)
        .iter()
        .any(|name| is_cua_key(name))
    {
        // Tasks created by the external-driver PR predate Maple's logical
        // metadata. Their concrete persisted stdio extension is unambiguous.
        return Some(AgentIntegrationBackend::External);
    }
    let backend = stored.cua().map(|entry| entry.backend)?;
    if backend == AgentIntegrationBackend::Embedded && session.session_type != SessionType::User {
        return None;
    }
    Some(backend)
}

pub(super) fn project_session_mcp_servers(
    stored: &StoredIntegrationRegistry,
    configured: &[AgentMcpServer],
    session: &Session,
) -> Result<Vec<AgentSessionMcpServer>, String> {
    let mut servers = session_mcp_servers(configured, session);
    servers.retain(|server| !is_cua_key(&server.name));

    let state = session_cua_state(session);
    let active_external = session_mcp_extension_keys(session)
        .iter()
        .any(|name| is_cua_key(name));
    if session.session_type == SessionType::User {
        servers.extend(
            EXTERNAL_AGENT_INTEGRATIONS
                .iter()
                .map(|entry| AgentSessionMcpServer {
                    name: entry.id.to_string(),
                    kind: AgentSessionIntegrationKind::ExternalAgent,
                    display_name: entry.name.to_string(),
                    description: entry.description.to_string(),
                    transport: "external_agent".to_string(),
                    enabled: session_external_agent_enabled(stored, session, entry.id),
                    // Installation is checked before enabling and again at launch.
                    available: true,
                }),
        );
    }
    let backend = session_cua_backend(stored, session);
    if backend.is_none() && session.session_type != SessionType::User {
        return Ok(servers);
    }
    let stored = stored.cua();
    let enabled = match backend {
        Some(AgentIntegrationBackend::Embedded) => state.is_some_and(|state| state.enabled),
        Some(AgentIntegrationBackend::External) => active_external,
        None => false,
    };
    let available = match backend {
        Some(AgentIntegrationBackend::Embedded) => embedded_cua_ready(),
        Some(AgentIntegrationBackend::External) => {
            active_external || stored.is_some_and(|entry| entry.external_server.is_some())
        }
        None => false,
    };
    servers.push(AgentSessionMcpServer {
        name: CUA_DRIVER_MCP_NAME.to_string(),
        kind: AgentSessionIntegrationKind::Mcp,
        display_name: CUA_DRIVER_CARD_NAME.to_string(),
        description: CUA_DRIVER_CARD_DESCRIPTION.to_string(),
        transport: match backend {
            Some(AgentIntegrationBackend::Embedded) => "embedded",
            Some(AgentIntegrationBackend::External) => "stdio",
            None => "unconfigured",
        }
        .to_string(),
        enabled,
        available,
    });
    Ok(servers)
}

#[cfg(embedded_cua)]
fn embedded_cua_ready() -> bool {
    super::cua::embedded_cua_permission_status().ready()
}

#[cfg(not(embedded_cua))]
fn embedded_cua_ready() -> bool {
    false
}

/// Reject a *newly* introduced custom server that would shadow the curated
/// integration.
///
/// A name an earlier release accepted stays saveable, so one legacy entry
/// cannot make every unrelated MCP edit fail. It is still shadowed at
/// selection time by [`effective_mcp_servers`], and enabling the integration
/// still refuses outright while it exists.
pub(super) fn validate_new_mcp_integration_collisions(
    previous: &[AgentMcpServer],
    next: &[AgentMcpServer],
) -> Result<(), String> {
    let existing = previous
        .iter()
        .map(|server| goose::config::extensions::name_to_key(&server.name))
        .collect::<HashSet<_>>();
    let added = next
        .iter()
        .filter(|server| !existing.contains(&goose::config::extensions::name_to_key(&server.name)))
        .cloned()
        .collect::<Vec<_>>();
    ensure_no_custom_integration_collision(&added)
}

fn ensure_no_custom_integration_collision(custom: &[AgentMcpServer]) -> Result<(), String> {
    // Treat the human-readable and conventional config spellings as one
    // product identity even though Goose preserves '-' and '_' in its lower
    // level extension key. Otherwise a custom server could shadow Maple's
    // built-in Computer use integration under its historical MCP name.
    if let Some(server) = custom.iter().find(|server| is_cua_identity(&server.name)) {
        return Err(format!(
            "Custom MCP server '{}' conflicts with the {CUA_DRIVER_NAME} integration. Rename or remove the custom server before enabling the integration.",
            server.name
        ));
    }
    Ok(())
}

fn integrations_path(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, String> {
    Ok(account_local_data_dir_path(paths, user_id)
        .map_err(|error| error.to_string())?
        .join(INTEGRATIONS_FILE_NAME))
}

fn load_stored_integrations(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<StoredIntegrationRegistry, String> {
    let path = integrations_path(paths, user_id)?;
    if !path
        .try_exists()
        .map_err(|error| format!("Failed to inspect device-local integration settings: {error}"))?
    {
        return Ok(StoredIntegrationRegistry::default());
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Failed to read device-local integration settings: {error}"))?;
    let value: Value = serde_json::from_str(&contents)
        .map_err(|error| format!("Failed to parse device-local integration settings: {error}"))?;
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Device-local integration settings have no version".to_string())?;
    let registry = match u32::try_from(version) {
        Ok(INTEGRATIONS_FILE_VERSION) => serde_json::from_str(&contents).map_err(|error| {
            format!("Failed to parse device-local integration settings: {error}")
        })?,
        Ok(LEGACY_INTEGRATIONS_FILE_VERSION) => {
            let legacy: LegacyStoredIntegrationRegistry =
                serde_json::from_str(&contents).map_err(|error| {
                    format!("Failed to parse legacy device-local integration settings: {error}")
                })?;
            migrate_legacy_registry(legacy)?
        }
        _ => {
            return Err(format!(
                "Unsupported device-local integration settings version {version}"
            ));
        }
    };
    validate_stored_registry(registry)
}

fn migrate_legacy_registry(
    registry: LegacyStoredIntegrationRegistry,
) -> Result<StoredIntegrationRegistry, String> {
    if registry.version != LEGACY_INTEGRATIONS_FILE_VERSION {
        return Err(format!(
            "Unsupported legacy integration settings version {}",
            registry.version
        ));
    }
    let integrations = registry
        .integrations
        .into_iter()
        .map(|entry| {
            let enabled = entry.server.enabled;
            StoredIntegration {
                id: entry.id,
                enabled,
                backend: AgentIntegrationBackend::External,
                external_server: Some(entry.server),
            }
        })
        .collect();
    Ok(StoredIntegrationRegistry {
        version: INTEGRATIONS_FILE_VERSION,
        integrations,
    })
}

fn validate_stored_registry(
    registry: StoredIntegrationRegistry,
) -> Result<StoredIntegrationRegistry, String> {
    if registry.version != INTEGRATIONS_FILE_VERSION {
        return Err(format!(
            "Unsupported device-local integration settings version {}",
            registry.version
        ));
    }
    let mut ids = HashSet::new();
    for entry in &registry.integrations {
        if !matches!(
            entry.id.as_str(),
            CUA_DRIVER_INTEGRATION_ID | CODEX_INTEGRATION_ID
        ) || !ids.insert(entry.id.as_str())
        {
            return Err(
                "Device-local integration settings contain an unknown or duplicate integration"
                    .to_string(),
            );
        }
        if entry.id == CODEX_INTEGRATION_ID {
            if entry.backend != AgentIntegrationBackend::Embedded || entry.external_server.is_some()
            {
                return Err("Device-local Codex settings are invalid".to_string());
            }
            continue;
        }
        if entry.backend == AgentIntegrationBackend::External && entry.external_server.is_none() {
            return Err(
                "Device-local external Cua Driver settings have no server definition".to_string(),
            );
        }
        if let Some(server) = entry.external_server.as_ref() {
            validate_stored_cua_server(server)?;
        }
    }
    Ok(registry)
}

fn validate_stored_cua_server(server: &AgentMcpServer) -> Result<(), String> {
    if server.name != CUA_DRIVER_MCP_NAME
        || server.description != CUA_EXTERNAL_MCP_DESCRIPTION
        || server.timeout_seconds != DEFAULT_MCP_TIMEOUT_SECONDS
    {
        return Err("Device-local Cua Driver settings are invalid".to_string());
    }
    let AgentMcpTransport::Stdio {
        command,
        environment,
    } = &server.transport
    else {
        return Err("Device-local Cua Driver transport is invalid".to_string());
    };
    if !environment.is_empty() {
        return Err("Device-local Cua Driver environment must be empty".to_string());
    }
    let parts = split_mcp_command(command, CUA_DRIVER_MCP_NAME)?;
    if parts.len() != 2 || parts[1] != "mcp" || !Path::new(&parts[0]).is_absolute() {
        return Err("Device-local Cua Driver command is invalid".to_string());
    }
    Ok(())
}

fn save_stored_integrations(
    paths: &AgentPathLayout,
    user_id: &str,
    registry: &StoredIntegrationRegistry,
) -> Result<(), String> {
    validate_stored_registry(registry.clone())?;
    write_device_local_json_file(&integrations_path(paths, user_id)?, registry)
        .map_err(|error| format!("Failed to save device-local integration settings: {error}"))
}

async fn detect_cua_driver() -> CuaDetection {
    #[cfg(not(embedded_cua))]
    {
        CuaDetection::not_detected()
    }

    #[cfg(embedded_cua)]
    {
        let permissions = super::cua::embedded_cua_permission_status();
        // Only macOS ships a standalone CuaDriver application at a path Maple
        // knows. Everywhere else the built-in runtime is the only backend, so
        // there is nothing to discover and no foreign executable to run.
        let (standalone_version, external_server, standalone_error) =
            detect_standalone_driver().await;
        let embedded_ready = permissions.ready();
        let availability = if embedded_ready || external_server.is_some() {
            AgentIntegrationAvailability::Available
        } else {
            AgentIntegrationAvailability::SetupRequired
        };
        // Say what is missing on the card itself, and say it in terms of what
        // is left to do rather than repeating the requirement's name.
        let detail = super::cua::desktop_helper_hint()
            .or_else(|| {
                standalone_error.map(|error| {
                    format!(
                        "Maple could not verify the standalone CuaDriver installation: {error}. Built-in CUA can still be set up."
                    )
                })
            });
        CuaDetection {
            availability,
            permissions: Some(permissions),
            setup_available: super::cua::desktop_setup_available(),
            standalone_version,
            detail,
            external_server,
        }
    }
}

/// Discover a separately installed CuaDriver application, if this platform has
/// one at a path Maple knows.
#[cfg(target_os = "macos")]
async fn detect_standalone_driver() -> (Option<String>, Option<AgentMcpServer>, Option<String>) {
    let candidate = PathBuf::from(CUA_DRIVER_MACOS_BINARY);
    let exists = match candidate.try_exists() {
        Ok(exists) => exists,
        Err(error) => {
            return (
                None,
                None,
                Some(format!(
                    "could not inspect the standalone application: {error}"
                )),
            );
        }
    };
    if !exists {
        return (None, None, None);
    }
    if let Err(error) = ensure_standalone_binary_is_protected(&candidate) {
        return (None, None, Some(error));
    }
    match probe_cua_manifest(&candidate).await {
        Ok((version, server)) => (Some(version), Some(server), None),
        Err(error) => (None, None, Some(error)),
    }
}

#[cfg(all(embedded_cua, not(target_os = "macos")))]
async fn detect_standalone_driver() -> (Option<String>, Option<AgentMcpServer>, Option<String>) {
    (None, None, None)
}

/// Refuse to execute a driver that any other account can rewrite.
///
/// Maple runs this binary to read its manifest whenever the Integrations page
/// opens, so a group- or world-writable file at the expected path would let a
/// second account choose the code Maple runs. Ownership is deliberately not
/// checked: a normal drag-install leaves the application owned by the user who
/// installed it.
#[cfg(target_os = "macos")]
fn ensure_standalone_binary_is_protected(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("could not inspect the standalone application: {error}"))?;
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(
            "the standalone application is writable by other accounts and was not run".to_string(),
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn probe_cua_manifest(path: &Path) -> Result<(String, AgentMcpServer), String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("could not resolve the executable: {error}"))?;
    let mut command = tokio::process::Command::new(&canonical);
    command
        .arg("manifest")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start the manifest probe: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "could not capture the manifest".to_string())?;
    let mut reader = tokio::spawn(super::bounded_process::read_bounded_stdout(
        stdout,
        MAX_CUA_MANIFEST_BYTES,
        "the Cua Driver manifest",
    ));

    let status = match tokio::time::timeout(CUA_PROBE_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            reader.abort();
            return Err(format!("could not wait for the manifest probe: {error}"));
        }
        Err(_) => {
            reader.abort();
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err("manifest probe timed out".to_string());
        }
    };
    let bytes = match tokio::time::timeout(CUA_PROBE_TIMEOUT, &mut reader).await {
        Ok(Ok(result)) => result?,
        Ok(Err(error)) => return Err(format!("could not collect the manifest: {error}")),
        Err(_) => {
            reader.abort();
            return Err("manifest output did not close".to_string());
        }
    };
    if !status.success() {
        return Err(format!("manifest probe exited with {status}"));
    }
    parse_cua_manifest(&canonical, &bytes)
}

#[cfg(any(target_os = "macos", test))]
fn parse_cua_manifest(
    detected_binary: &Path,
    bytes: &[u8],
) -> Result<(String, AgentMcpServer), String> {
    let manifest: CuaManifest = serde_json::from_slice(bytes)
        .map_err(|error| format!("manifest is not valid JSON: {error}"))?;
    if manifest.schema_version != CUA_MANIFEST_SCHEMA_VERSION {
        return Err(format!(
            "manifest schema {} is not supported",
            manifest.schema_version
        ));
    }
    let version = manifest.binary_version.trim();
    if version.is_empty() {
        return Err("manifest has no binary version".to_string());
    }
    if manifest.mcp_invocation.args != ["mcp"] {
        return Err("manifest does not advertise the expected MCP entrypoint".to_string());
    }

    let declared_binary = canonical_manifest_path(&manifest.binary_path, "binary path")?;
    let command_binary = canonical_manifest_path(&manifest.mcp_invocation.command, "MCP command")?;
    if declared_binary != detected_binary || command_binary != detected_binary {
        return Err("manifest executable does not match the detected application".to_string());
    }

    let verified_binary = detected_binary
        .to_str()
        .ok_or_else(|| "detected Cua Driver path is not valid UTF-8".to_string())?;
    let command = join_mcp_command(std::iter::once(verified_binary).chain(["mcp"]))?;
    let server = AgentMcpServer {
        name: CUA_DRIVER_MCP_NAME.to_string(),
        description: CUA_EXTERNAL_MCP_DESCRIPTION.to_string(),
        enabled: false,
        timeout_seconds: DEFAULT_MCP_TIMEOUT_SECONDS,
        transport: AgentMcpTransport::Stdio {
            command,
            environment: Vec::new(),
        },
    };
    validate_stored_cua_server(&server)?;
    Ok((version.to_string(), server))
}

#[cfg(any(target_os = "macos", test))]
fn canonical_manifest_path(path: &str, label: &str) -> Result<PathBuf, String> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(format!("manifest {label} is not absolute"));
    }
    path.canonicalize()
        .map_err(|error| format!("could not resolve manifest {label}: {error}"))
}

#[cfg(any(target_os = "macos", test))]
fn join_mcp_command<'a>(parts: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
    parts
        .into_iter()
        .map(|part| {
            if part.is_empty() || part.contains(['\0', '"']) {
                return Err("manifest MCP arguments contain unsupported characters".to_string());
            }
            if part.chars().any(char::is_whitespace) {
                Ok(format!("\"{part}\""))
            } else {
                Ok(part.to_string())
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cua_only(cua: CuaDetection) -> IntegrationDetections {
        IntegrationDetections {
            cua,
            codex: CodexDetection::default(),
        }
    }

    fn stored_cua_server(root: &Path, enabled: bool) -> AgentMcpServer {
        let binary = root.join("cua-driver");
        let command = join_mcp_command([binary.to_str().expect("UTF-8 test path"), "mcp"])
            .expect("valid test command");
        AgentMcpServer {
            name: CUA_DRIVER_MCP_NAME.to_string(),
            description: CUA_EXTERNAL_MCP_DESCRIPTION.to_string(),
            enabled,
            timeout_seconds: DEFAULT_MCP_TIMEOUT_SECONDS,
            transport: AgentMcpTransport::Stdio {
                command,
                environment: Vec::new(),
            },
        }
    }

    fn manifest(binary: &Path, schema: &str, args: &[&str]) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": schema,
            "binary_path": binary,
            "binary_version": "0.21.1-test",
            "mcp_invocation": {
                "command": binary,
                "args": args,
            }
        }))
        .unwrap()
    }

    fn codex_registry(enabled: bool) -> StoredIntegrationRegistry {
        StoredIntegrationRegistry {
            version: INTEGRATIONS_FILE_VERSION,
            integrations: vec![StoredIntegration {
                id: CODEX_INTEGRATION_ID.to_string(),
                enabled,
                backend: AgentIntegrationBackend::Embedded,
                external_server: None,
            }],
        }
    }

    #[tokio::test]
    async fn old_task_inherits_providers_until_explicitly_overridden() {
        let temp = tempfile::tempdir().unwrap();
        let manager = SessionManager::new(temp.path().join("sessions"));
        let session = manager
            .create_session(
                temp.path().to_path_buf(),
                "Old task".into(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        assert!(
            session_external_agent_providers(&codex_registry(false), &session, true).is_empty()
        );
        assert_eq!(
            session_external_agent_providers(&codex_registry(true), &session, true),
            ["codex"]
        );
        assert!(
            session_external_agent_providers(&codex_registry(true), &session, false).is_empty()
        );
        let acp = Session {
            session_type: SessionType::Acp,
            ..session.clone()
        };
        assert!(session_external_agent_providers(&codex_registry(true), &acp, true).is_empty());
        let disabled = persist_task_integration_override(&manager, &session.id, "codex", false)
            .await
            .unwrap();
        assert!(
            session_external_agent_providers(&codex_registry(true), &disabled, true).is_empty()
        );
        let enabled = persist_task_integration_override(&manager, &session.id, "codex", true)
            .await
            .unwrap();
        assert_eq!(
            session_external_agent_providers(&codex_registry(false), &enabled, true),
            ["codex"]
        );
        drop(manager);
        let manager = SessionManager::new(temp.path().join("sessions"));
        let restored = manager.get_session(&session.id, false).await.unwrap();
        assert_eq!(
            session_external_agent_providers(&codex_registry(false), &restored, true),
            ["codex"]
        );
    }

    #[test]
    fn composer_lists_native_integrations_without_prior_setup() {
        let session = Session {
            session_type: SessionType::User,
            ..Session::default()
        };
        let rows =
            project_session_mcp_servers(&StoredIntegrationRegistry::default(), &[], &session)
                .unwrap();
        assert!(rows.iter().any(|row| row.name == "codex"
            && row.kind == AgentSessionIntegrationKind::ExternalAgent
            && row.display_name == "Codex"
            && !row.enabled));
        assert!(
            rows.iter()
                .any(|row| row.name == CUA_DRIVER_MCP_NAME && !row.enabled && !row.available)
        );
        let temp = tempfile::tempdir().unwrap();
        let mut custom = stored_cua_server(temp.path(), false);
        custom.name = "codex".to_string();
        let rows = project_session_mcp_servers(&codex_registry(true), &[custom], &session).unwrap();
        let codex_rows = rows
            .iter()
            .filter(|row| row.name == "codex")
            .collect::<Vec<_>>();
        assert_eq!(codex_rows.len(), 2);
        assert!(
            codex_rows
                .iter()
                .any(|row| row.kind == AgentSessionIntegrationKind::Mcp && !row.enabled)
        );
        assert!(
            codex_rows
                .iter()
                .any(|row| row.kind == AgentSessionIntegrationKind::ExternalAgent && row.enabled)
        );
        let legacy_request: AgentSetSessionMcpServerRequest = serde_json::from_value(json!({
            "sessionId": "s1", "name": "codex", "enabled": true,
        }))
        .unwrap();
        assert_eq!(legacy_request.kind, AgentSessionIntegrationKind::Mcp);
        assert!(external_agent_selection("unknown").is_none());
        assert_eq!(external_agent_selection("codex").unwrap().id, "codex");
    }

    /// Exercise the actual run-boundary wiring and Goose tool cache, including
    /// a cold Agent with the old persisted developer extension. No inference or
    /// external process is needed to inspect the model-facing tool catalog.
    #[tokio::test]
    async fn resumed_task_refreshes_external_tools_on_every_run() {
        let fixture =
            super::super::test_support::started_agent_runtime("integration-refresh").await;
        let service = &fixture.handle.service;
        let user = fixture.handle.user_id.as_ref();
        let paths = &service.host.paths;
        let (manager, transport, web_state) = {
            let runtime = service.inner.lock().await;
            let runtime = runtime.as_ref().unwrap();
            (
                runtime.session_manager.clone(),
                runtime.maple_api_session.clone(),
                runtime.web_tool_state.clone(),
            )
        };
        let session = manager
            .create_session(
                fixture.project_root.clone(),
                "Before Codex".into(),
                SessionType::User,
                GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let registry = Arc::new(ExternalAgentRegistry::new(ExternalAgentHost {
            service: service.clone(),
            runtime: fixture.handle.clone(),
            session_manager: manager.clone(),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            project_root: fixture.project_root.clone(),
            lifetime: CancellationToken::new(),
        }));
        let config = GooseAgentConfig::new(
            manager.clone(),
            Arc::new(PermissionManager::new(fixture.root.join("permissions"))),
            None,
            GooseMode::SmartApprove,
            true,
            GoosePlatform::GooseDesktop,
        );
        let mut agent = Arc::new(Agent::with_config(config.clone()));
        let context = SharedAgentToolContext::new(AgentToolContextSpec::default());
        // Old task before enable, cached task after enable, cold restore after
        // enable, explicit off, explicit on despite default off, and ACP lease.
        for (default, override_value, cold, desktop, expected) in [
            (false, None, false, true, false),
            (true, None, true, true, true),
            (false, None, false, true, false),
            (true, None, false, true, true),
            (true, Some(false), true, true, false),
            (false, Some(true), true, true, true),
            (true, None, false, false, false),
        ] {
            save_stored_integrations(paths, user, &codex_registry(default)).unwrap();
            if let Some(enabled) = override_value {
                persist_task_integration_override(&manager, &session.id, "codex", enabled)
                    .await
                    .unwrap();
            }
            let session = manager.get_session(&session.id, false).await.unwrap();
            if cold {
                agent = Arc::new(Agent::with_config(config.clone()));
                // Restore the exact extension snapshot from the previous run.
                if let Some(state) = goose::session::EnabledExtensionsState::from_extension_data(
                    &session.extension_data,
                ) {
                    for extension in state.extensions {
                        agent.add_extension(extension, &session.id).await.unwrap();
                    }
                }
            }
            let (configured, errors) = finish_session_agent(
                PreparedSessionAgent {
                    agent: agent.clone(),
                    mcp_errors: Vec::new(),
                },
                AgentSkillsScope {
                    paths,
                    user_id: user,
                },
                &manager,
                &transport,
                SessionAgentConfiguration {
                    web_tool_state: &web_state,
                    session: &session,
                    model: DEFAULT_AGENT_MODEL,
                    context_limit: None,
                    mode: DEFAULT_GOOSE_MODE,
                    primary_model_supports_vision: false,
                    tool_context: &context,
                    allow_embedded_cua: desktop,
                    external_agents: Some(&registry),
                    host_search_path: None,
                },
            )
            .await
            .unwrap();
            assert!(errors.is_empty());
            let tools = configured
                .extension_manager
                .get_prefixed_tools(&session.id, None)
                .await
                .unwrap();
            for name in external_agents::EXTERNAL_AGENT_TOOLS {
                assert_eq!(
                    tools.iter().any(|tool| tool.name.as_ref() == name),
                    expected,
                    "{name}: default={default}, override={override_value:?}, cold={cold}, desktop={desktop}"
                );
            }
        }
        let rows = fixture
            .handle
            .set_session_mcp_server_enabled(AgentSetSessionMcpServerRequest {
                session_id: session.id.clone(),
                name: "codex".to_string(),
                kind: AgentSessionIntegrationKind::ExternalAgent,
                enabled: false,
            })
            .await
            .unwrap();
        assert!(
            rows.iter()
                .any(|row| row.kind == AgentSessionIntegrationKind::ExternalAgent && !row.enabled)
        );
        let stored_task = manager.get_session(&session.id, false).await.unwrap();
        assert!(
            session_external_agent_providers(&codex_registry(true), &stored_task, true).is_empty()
        );
        // A future provider selected alongside an installed but unselected
        // Codex must not grant Codex access through forged tool arguments.
        use goose::agents::mcp_client::McpClientTrait;
        let client = MapleDeveloperClient::new(
            agent.extension_manager.get_context().clone(),
            false,
            transport,
            web_state,
            context,
        )
        .unwrap()
        .with_external_agents(Some(registry), vec!["future-provider".to_string()]);
        for name in [
            external_agents::AGENT_START_TOOL,
            external_agents::AGENT_SEND_TOOL,
            external_agents::AGENT_STATUS_TOOL,
            external_agents::AGENT_CANCEL_TOOL,
        ] {
            let result = client
                .call_tool(
                    &goose::agents::ToolCallContext::new(session.id.clone(), None, None),
                    name,
                    Some(rmcp::object!({"provider": "codex"})),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(result.is_error, Some(true));
            assert!(format!("{:?}", result.content).contains("not enabled for this task"));
        }
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn manifest_builds_an_ordinary_disabled_mcp_server() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("cua driver");
        fs::write(&binary, b"fixture").unwrap();
        let binary = binary.canonicalize().unwrap();

        let (version, server) =
            parse_cua_manifest(&binary, &manifest(&binary, "1", &["mcp"])).unwrap();

        assert_eq!(version, "0.21.1-test");
        assert!(!server.enabled);
        assert_eq!(server.name, CUA_DRIVER_MCP_NAME);
        assert_eq!(
            server.transport,
            AgentMcpTransport::Stdio {
                command: format!("\"{}\" mcp", binary.display()),
                environment: Vec::new(),
            }
        );
    }

    #[test]
    fn manifest_rejects_unknown_schema_and_non_mcp_invocation() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("cua-driver");
        fs::write(&binary, b"fixture").unwrap();
        let binary = binary.canonicalize().unwrap();

        assert!(
            parse_cua_manifest(&binary, &manifest(&binary, "2", &["mcp"]))
                .unwrap_err()
                .contains("schema")
        );
        assert!(
            parse_cua_manifest(&binary, &manifest(&binary, "1", &["serve"]))
                .unwrap_err()
                .contains("MCP entrypoint")
        );
    }

    #[test]
    fn manifest_rejects_a_different_or_relative_binary() {
        let temporary = tempfile::tempdir().unwrap();
        let detected = temporary.path().join("cua-driver");
        let different = temporary.path().join("different");
        fs::write(&detected, b"fixture").unwrap();
        fs::write(&different, b"fixture").unwrap();
        let detected = detected.canonicalize().unwrap();
        let different = different.canonicalize().unwrap();

        assert!(
            parse_cua_manifest(&detected, &manifest(&different, "1", &["mcp"]))
                .unwrap_err()
                .contains("does not match")
        );
        let relative = br#"{
            "schema_version":"1",
            "binary_path":"cua-driver",
            "binary_version":"test",
            "mcp_invocation":{"command":"cua-driver","args":["mcp"]}
        }"#;
        assert!(
            parse_cua_manifest(&detected, relative)
                .unwrap_err()
                .contains("not absolute")
        );
    }

    #[test]
    fn stored_integrations_are_device_local_and_join_the_effective_registry() {
        let temporary = tempfile::tempdir().unwrap();
        let first = AgentPathLayout::from_app_roots(
            temporary.path().join("shared-config"),
            temporary.path().join("first-device"),
        );
        let second = AgentPathLayout::from_app_roots(
            temporary.path().join("shared-config"),
            temporary.path().join("second-device"),
        );
        let user = "cua-device-local@example.com";
        let server = stored_cua_server(temporary.path(), true);
        save_stored_integrations(
            &first,
            user,
            &StoredIntegrationRegistry {
                version: INTEGRATIONS_FILE_VERSION,
                integrations: vec![StoredIntegration {
                    id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                    enabled: true,
                    backend: AgentIntegrationBackend::External,
                    external_server: Some(server.clone()),
                }],
            },
        )
        .unwrap();

        assert_eq!(
            effective_mcp_servers(&stored_integrations_for_read(&first, user), Vec::new()).unwrap(),
            vec![server]
        );
        assert!(
            effective_mcp_servers(&stored_integrations_for_read(&second, user), Vec::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn custom_server_collision_is_explicit() {
        let custom = AgentMcpServer {
            name: "Cua Driver".to_string(),
            description: String::new(),
            enabled: false,
            timeout_seconds: DEFAULT_MCP_TIMEOUT_SECONDS,
            transport: AgentMcpTransport::Stdio {
                command: "elsewhere mcp".to_string(),
                environment: Vec::new(),
            },
        };
        assert!(
            ensure_no_custom_integration_collision(&[custom])
                .unwrap_err()
                .contains("Rename or remove")
        );
        assert!(is_cua_key("cua-driver"));
        assert!(is_cua_key("Cua Driver"));
        assert!(is_cua_key("cua_driver"));
        assert!(is_cua_key(CUA_DRIVER_NAME));
    }

    fn http_server(name: &str) -> AgentMcpServer {
        AgentMcpServer {
            name: name.to_string(),
            description: String::new(),
            enabled: true,
            timeout_seconds: DEFAULT_MCP_TIMEOUT_SECONDS,
            transport: AgentMcpTransport::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                environment: Vec::new(),
                headers: Vec::new(),
            },
        }
    }

    #[test]
    fn an_unusable_registry_file_does_not_block_tasks() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AgentPathLayout::from_app_roots(
            temporary.path().join("config"),
            temporary.path().join("local-data"),
        );
        let user = "cua-unusable@example.com";
        let path = integrations_path(&paths, user).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A file written by a newer build. Task creation must survive it.
        std::fs::write(&path, br#"{"version": 99, "integrations": []}"#).unwrap();

        assert!(load_stored_integrations(&paths, user).is_err());
        let stored = stored_integrations_for_read(&paths, user);
        assert!(stored.cua().is_none());
        assert_eq!(
            effective_mcp_servers(&stored, vec![http_server("Docs")]).unwrap(),
            vec![http_server("Docs")]
        );
        // The unusable file is still there for Settings to report, not clobbered.
        assert!(path.exists());
    }

    #[test]
    fn a_legacy_cua_named_server_does_not_block_unrelated_saves() {
        let legacy = http_server("Cua Driver");
        // Saving the same list again, or adding an unrelated server, succeeds.
        assert!(
            validate_new_mcp_integration_collisions(
                std::slice::from_ref(&legacy),
                &[legacy.clone(), http_server("Docs")],
            )
            .is_ok()
        );
        // Introducing the colliding name for the first time still fails.
        assert!(
            validate_new_mcp_integration_collisions(
                &[http_server("Docs")],
                std::slice::from_ref(&legacy),
            )
            .unwrap_err()
            .contains("Rename or remove")
        );
    }

    #[test]
    fn the_integration_shadows_a_custom_server_that_shares_its_key() {
        let temporary = tempfile::tempdir().unwrap();
        let managed = stored_cua_server(temporary.path(), true);
        let stored = StoredIntegrationRegistry {
            version: INTEGRATIONS_FILE_VERSION,
            integrations: vec![StoredIntegration {
                id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                enabled: true,
                backend: AgentIntegrationBackend::External,
                external_server: Some(managed.clone()),
            }],
        };

        // A legacy custom server under the same key is replaced rather than
        // merged: merging would make normalization reject the whole account.
        let effective = effective_mcp_servers(
            &stored,
            vec![http_server("cua-driver"), http_server("Docs")],
        )
        .unwrap();
        assert_eq!(effective.len(), 2);
        assert!(effective.iter().any(|server| server.name == "Docs"));
        assert!(effective.iter().any(|server| {
            server.name == CUA_DRIVER_MCP_NAME
                && matches!(server.transport, AgentMcpTransport::Stdio { .. })
        }));
    }

    #[test]
    fn embedded_integration_shadows_every_legacy_cua_alias() {
        let stored = StoredIntegrationRegistry {
            version: INTEGRATIONS_FILE_VERSION,
            integrations: vec![StoredIntegration {
                id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                enabled: true,
                backend: AgentIntegrationBackend::Embedded,
                external_server: None,
            }],
        };

        let effective = effective_mcp_servers(
            &stored,
            vec![
                http_server("cua-driver"),
                http_server("Cua Driver"),
                http_server("cua_driver"),
                http_server(CUA_DRIVER_NAME),
                http_server("Docs"),
            ],
        )
        .unwrap();

        assert_eq!(effective, vec![http_server("Docs")]);
    }

    #[test]
    fn enabling_and_disabling_preserve_custom_servers_and_the_managed_entry() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AgentPathLayout::from_app_roots(
            temporary.path().join("config"),
            temporary.path().join("local-data"),
        );
        let user = "cua-toggle@example.com";
        let custom = AgentMcpServer {
            name: "Docs".to_string(),
            description: String::new(),
            enabled: true,
            timeout_seconds: DEFAULT_MCP_TIMEOUT_SECONDS,
            transport: AgentMcpTransport::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                environment: Vec::new(),
                headers: Vec::new(),
            },
        };
        save_agent_config_inner(
            &paths,
            user,
            &AgentConfig {
                mcp_servers: vec![custom.clone()],
                ..AgentConfig::default()
            },
        )
        .unwrap();
        let managed = stored_cua_server(temporary.path(), false);
        let detection = CuaDetection {
            availability: AgentIntegrationAvailability::Available,
            setup_available: true,
            permissions: None,
            standalone_version: Some("test".to_string()),
            detail: None,
            external_server: Some(managed),
        };

        let enabled = set_integration_default(
            &paths,
            user,
            &AgentSetIntegrationEnabledRequest {
                id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                enabled: true,
            },
            &cua_only(detection),
        )
        .unwrap();
        assert!(enabled[0].enabled_for_new_tasks);
        let effective = effective_mcp_servers(
            &stored_integrations_for_read(&paths, user),
            load_agent_config_inner(&paths, user).unwrap().mcp_servers,
        )
        .unwrap();
        assert_eq!(effective.len(), 2);
        assert_eq!(effective[0], custom);
        assert!(effective[1].enabled);

        let disabled = set_integration_default(
            &paths,
            user,
            &AgentSetIntegrationEnabledRequest {
                id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                enabled: false,
            },
            &cua_only(CuaDetection::not_detected()),
        )
        .unwrap();
        assert!(!disabled[0].enabled_for_new_tasks);
        let effective = effective_mcp_servers(
            &stored_integrations_for_read(&paths, user),
            load_agent_config_inner(&paths, user).unwrap().mcp_servers,
        )
        .unwrap();
        assert_eq!(effective.len(), 2);
        assert_eq!(effective[0], custom);
        assert!(!effective[1].enabled);
    }

    #[test]
    fn version_one_registry_migrates_to_the_external_backend_without_switching() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AgentPathLayout::from_app_roots(
            temporary.path().join("config"),
            temporary.path().join("local-data"),
        );
        let user = "cua-migration@example.com";
        let server = stored_cua_server(temporary.path(), true);
        let path = integrations_path(&paths, user).unwrap();
        write_device_local_json_file(
            &path,
            &json!({
                "version": 1,
                "integrations": [{
                    "id": CUA_DRIVER_INTEGRATION_ID,
                    "server": server,
                }],
            }),
        )
        .unwrap();

        let migrated = load_stored_integrations(&paths, user).unwrap();
        assert_eq!(migrated.version, INTEGRATIONS_FILE_VERSION);
        assert_eq!(migrated.integrations.len(), 1);
        assert!(migrated.integrations[0].enabled);
        assert_eq!(
            migrated.integrations[0].backend,
            AgentIntegrationBackend::External
        );
        assert!(migrated.integrations[0].external_server.is_some());
    }

    #[test]
    fn explicit_setup_switches_only_new_tasks_to_embedded() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AgentPathLayout::from_app_roots(
            temporary.path().join("config"),
            temporary.path().join("local-data"),
        );
        let user = "cua-embedded@example.com";
        let external = stored_cua_server(temporary.path(), true);
        save_stored_integrations(
            &paths,
            user,
            &StoredIntegrationRegistry {
                version: INTEGRATIONS_FILE_VERSION,
                integrations: vec![StoredIntegration {
                    id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                    enabled: true,
                    backend: AgentIntegrationBackend::External,
                    external_server: Some(external),
                }],
            },
        )
        .unwrap();
        let detection = CuaDetection {
            availability: AgentIntegrationAvailability::Available,
            setup_available: true,
            permissions: Some(AgentIntegrationPermissions::none_required()),
            standalone_version: Some("test".to_string()),
            detail: None,
            external_server: None,
        };

        let projected =
            select_embedded_integration_backend(&paths, user, &cua_only(detection)).unwrap();
        assert_eq!(
            projected[0].backend,
            Some(AgentIntegrationBackend::Embedded)
        );
        assert!(projected[0].enabled_for_new_tasks);
        let stored = stored_integrations_for_read(&paths, user);
        assert_eq!(
            cua_state_for_new_session(&stored, None, true).unwrap(),
            Some(CuaSessionState {
                backend: AgentIntegrationBackend::Embedded,
                enabled: true,
            })
        );
        assert!(
            cua_state_for_new_session(&stored, None, false)
                .unwrap()
                .is_none()
        );
        assert!(
            cua_state_for_new_session(&stored, Some(&[CUA_DRIVER_MCP_NAME.to_string()]), false)
                .unwrap_err()
                .contains("Maple desktop app")
        );
    }
    #[test]
    fn codex_toggle_persists_default_off_and_installs_skills() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AgentPathLayout::from_app_roots(
            temporary.path().join("config"),
            temporary.path().join("data"),
        );
        let user = "codex-user";
        let detections = IntegrationDetections {
            cua: CuaDetection::not_detected(),
            codex: CodexDetection {
                executable: Some(temporary.path().join("codex")),
                version: Some("codex-cli 0.150.0".to_string()),
                signed_in: Some(false),
                problem: None,
            },
        };
        let projected = project_integrations(&paths, user, &detections).unwrap();
        let codex_card = projected
            .iter()
            .find(|integration| integration.id == CODEX_INTEGRATION_ID)
            .unwrap();
        assert!(!codex_card.enabled_for_new_tasks);
        assert_eq!(
            codex_card.availability,
            AgentIntegrationAvailability::Available
        );
        assert_eq!(codex_card.version.as_deref(), Some("codex-cli 0.150.0"));
        assert!(
            codex_card
                .detail
                .as_deref()
                .unwrap()
                .contains("codex login")
        );
        assert!(!external_agents_enabled(&paths, user));

        let enabled = set_integration_default(
            &paths,
            user,
            &AgentSetIntegrationEnabledRequest {
                id: CODEX_INTEGRATION_ID.to_string(),
                enabled: true,
            },
            &detections,
        )
        .unwrap();
        assert!(
            enabled
                .iter()
                .find(|integration| integration.id == CODEX_INTEGRATION_ID)
                .unwrap()
                .enabled_for_new_tasks
        );
        assert!(external_agents_enabled(&paths, user));
        let skills = external_agent_skills_dir(&paths, user).unwrap();
        for (name, content) in EXTERNAL_AGENT_SKILLS {
            assert_eq!(
                fs::read_to_string(skills.join(name).join("SKILL.md")).unwrap(),
                content
            );
        }
        let commands = account_skill_commands(&paths, user);
        assert_eq!(
            commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["advisor", "committee", "handoff"]
        );
        assert_eq!(
            commands[2].input_hint.as_deref(),
            Some("<what to hand off>")
        );
        assert!(commands[2].description.contains("Codex"));
        // A user's own skill of the same name is never overwritten or removed.
        let own = skills.join("advisor").join("SKILL.md");
        fs::write(&own, "---\nname: advisor\n---\nmine\n").unwrap();

        let disabled = set_integration_default(
            &paths,
            user,
            &AgentSetIntegrationEnabledRequest {
                id: CODEX_INTEGRATION_ID.to_string(),
                enabled: false,
            },
            &IntegrationDetections {
                cua: CuaDetection::not_detected(),
                codex: CodexDetection::default(),
            },
        )
        .unwrap();
        assert!(
            !disabled
                .iter()
                .find(|integration| integration.id == CODEX_INTEGRATION_ID)
                .unwrap()
                .enabled_for_new_tasks
        );
        assert!(!external_agents_enabled(&paths, user));
        assert!(!skills.join("handoff").join("SKILL.md").exists());
        assert!(!skills.join("committee").join("SKILL.md").exists());
        assert_eq!(
            fs::read_to_string(&own).unwrap(),
            "---\nname: advisor\n---\nmine\n"
        );
    }

    #[test]
    fn codex_cannot_be_enabled_without_a_usable_installation() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AgentPathLayout::from_app_roots(
            temporary.path().join("config"),
            temporary.path().join("data"),
        );
        let user = "codex-user";
        let request = AgentSetIntegrationEnabledRequest {
            id: CODEX_INTEGRATION_ID.to_string(),
            enabled: true,
        };
        let missing = IntegrationDetections {
            cua: CuaDetection::not_detected(),
            codex: CodexDetection::default(),
        };
        let error = set_integration_default(&paths, user, &request, &missing).unwrap_err();
        assert!(error.contains("Install the Codex CLI"));
        let projected = project_integrations(&paths, user, &missing).unwrap();
        assert_eq!(
            projected[1].availability,
            AgentIntegrationAvailability::NotDetected
        );

        let old = IntegrationDetections {
            cua: CuaDetection::not_detected(),
            codex: CodexDetection {
                executable: Some(temporary.path().join("codex")),
                version: Some("0.100.0".to_string()),
                signed_in: Some(true),
                problem: Some(
                    "Codex 0.100.0 is older than the 0.143.0 that Maple needs.".to_string(),
                ),
            },
        };
        let error = set_integration_default(&paths, user, &request, &old).unwrap_err();
        assert!(error.contains("older"));
        let projected = project_integrations(&paths, user, &old).unwrap();
        assert_eq!(
            projected[1].availability,
            AgentIntegrationAvailability::SetupRequired
        );
        assert!(!external_agents_enabled(&paths, user));
    }
}
