//! Maple-curated, device-local integrations.
//!
//! This module owns the product-level catalog, validation, device-local
//! default, and migration between an installed external MCP server and a
//! Maple-hosted implementation. A task freezes that backend choice when it is
//! created; account defaults never rewrite existing tasks.

use super::external_agents::claude::{self, ClaudeDetection};
use super::external_agents::codex::{self, CodexDetection};
use super::*;
use std::collections::HashSet;

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

pub(super) const EXTERNAL_AGENT_INTEGRATIONS: &[ExternalAgentIntegration] = &[
    ExternalAgentIntegration {
        id: CODEX_INTEGRATION_ID,
        name: CODEX_CARD_NAME,
        description: CODEX_CARD_DESCRIPTION,
        project: |detections, stored| codex_public(&detections.codex, stored),
    },
    ExternalAgentIntegration {
        id: claude::PROVIDER_ID,
        name: claude::PROVIDER_NAME,
        description: "Let a task hand work to the Claude Code CLI installed on this computer, with its own account.",
        project: |detections, stored| claude_public(&detections.claude, stored),
    },
];

impl AgentIntegration {
    /// Whether this card is an external coding agent in the runtime catalog.
    pub fn is_external_agent(&self) -> bool {
        external_agent_selection(&self.id).is_some()
    }
}

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

pub(super) fn external_agent_enabled(stored: &StoredIntegrationRegistry, id: &str) -> bool {
    stored
        .integrations
        .iter()
        .any(|entry| entry.id == id && entry.enabled)
}

fn session_external_agent_enabled(
    stored: &StoredIntegrationRegistry,
    session: &Session,
    id: &str,
) -> bool {
    // Settings admits the provider; each task must also explicitly select it.
    // A saved selection cannot bypass a disabled account/device integration.
    external_agent_enabled(stored, id)
        && TaskIntegrationOverrides::from_extension_data(&session.extension_data)
            .and_then(|state| state.enabled.get(id).copied())
            .unwrap_or(false)
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
    pub(super) claude: ClaudeDetection,
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
        backend: stored.map(|entry| entry.backend.public()),
        version: detection.version.clone(),
        permissions: None,
        setup_available: false,
        enabled_for_new_tasks: stored.is_some_and(|entry| entry.enabled),
        detail,
    }
}

fn claude_public(
    detection: &ClaudeDetection,
    stored: Option<&StoredIntegration>,
) -> AgentIntegration {
    let descriptor = external_agent_selection(claude::PROVIDER_ID).expect("Claude catalog entry");
    AgentIntegration {
        id: descriptor.id.into(),
        name: descriptor.name.into(),
        description: descriptor.description.into(),
        availability: match (&detection.executable, &detection.problem) {
            (None, _) => AgentIntegrationAvailability::NotDetected,
            (_, Some(_)) => AgentIntegrationAvailability::SetupRequired,
            _ => AgentIntegrationAvailability::Available,
        },
        backend: stored.map(|entry| entry.backend.public()),
        version: detection.version.clone(),
        permissions: None,
        setup_available: false,
        enabled_for_new_tasks: stored.is_some_and(|entry| entry.enabled),
        detail: Some(if detection.executable.is_none() {
            "Install Claude Code and make sure `claude` is on PATH, then reopen this page.".into()
        } else if let Some(problem) = &detection.problem {
            problem.clone()
        } else {
            match detection.signed_in {
                Some(true) => "Signed in.".into(),
                Some(false) => claude::sign_in_hint().into(),
                None => "Could not check sign-in. Run `claude auth status` in a terminal.".into(),
            }
        }),
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
    backend: StoredBackend,
}

/// The backend column of the device-local file. Files written by builds that
/// offered the standalone CuaDriver say `external`; those entries are dropped
/// on load, not migrated, so the file stays readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredBackend {
    Embedded,
    #[serde(other)]
    Retired,
}

impl StoredBackend {
    fn public(self) -> AgentIntegrationBackend {
        match self {
            Self::Embedded | Self::Retired => AgentIntegrationBackend::Embedded,
        }
    }
}

#[derive(Debug)]
pub(super) struct CuaDetection {
    availability: AgentIntegrationAvailability,
    permissions: Option<AgentIntegrationPermissions>,
    setup_available: bool,
    detail: Option<String>,
}

impl CuaDetection {
    #[cfg(any(not(embedded_cua), test))]
    fn not_detected() -> Self {
        Self {
            availability: AgentIntegrationAvailability::NotDetected,
            permissions: None,
            setup_available: false,
            detail: Some("Built-in CUA is not available on this operating system yet.".to_string()),
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
            backend: stored.map(|entry| entry.backend.public()),
            version: embedded_cua_version(),
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

/// Discover every curated integration. `search_path` is the PATH
/// to look on for external agents, when the host knows a fuller one than
/// the process environment (a macOS GUI launch).
pub(super) async fn detect_integrations(search_path: Option<&str>) -> IntegrationDetections {
    IntegrationDetections {
        cua: detect_cua_driver().await,
        codex: codex::detect(search_path).await,
        claude: claude::detect(search_path).await,
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
            backend: StoredBackend::Embedded,
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
        if !detection.embedded_ready() {
            return Err(
                "Set up Maple's Accessibility and Screen Recording permissions before enabling built-in CUA"
                    .to_string(),
            );
        }
        match stored.cua_mut() {
            Some(entry) => {
                entry.backend = StoredBackend::Embedded;
                entry.enabled = true;
            }
            None => {
                stored.integrations.push(StoredIntegration {
                    id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                    enabled: true,
                    backend: StoredBackend::Embedded,
                });
            }
        }
    } else if let Some(entry) = stored.cua_mut() {
        entry.enabled = false;
    }

    save_stored_integrations(paths, user_id, &stored)
}

/// Record Maple's embedded backend as the new-task default once the OS
/// reports both grants. An incomplete setup leaves the stored choice alone.
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
        Some(entry) => entry.backend = StoredBackend::Embedded,
        None => stored.integrations.push(StoredIntegration {
            id: CUA_DRIVER_INTEGRATION_ID.to_string(),
            enabled: false,
            backend: StoredBackend::Embedded,
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
    if stored.cua().is_some() {
        // The integration owns this product identity. A custom server that a
        // previous release accepted under any historical spelling is shadowed
        // before Goose can select or start it. Merging would make
        // normalization fail and lock the account out of every task, while
        // starting it briefly before embedded CUA replaces it would cross the
        // explicit backend boundary.
        servers.retain(|candidate| !is_cua_identity(&candidate.name));
    }
    normalize_mcp_servers(servers)
}

/// Whether a configured server name addresses the curated CUA integration.
pub(super) fn is_cua_key(name: &str) -> bool {
    is_cua_identity(name)
}

/// Whether a configured server uses any spelling reserved for curated CUA.
pub(super) fn is_cua_identity(name: &str) -> bool {
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
    if !allow_embedded {
        if explicitly_requested == Some(true) {
            return Err(
                "Built-in CUA is available only to tasks running in the Maple desktop app"
                    .to_string(),
            );
        }
        return Ok(None);
    }
    Ok(Some(CuaSessionState {
        backend: entry.backend.public(),
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
    let backend = stored.cua().map(|entry| entry.backend.public())?;
    if session.session_type != SessionType::User {
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
    if session.session_type == SessionType::User {
        servers.extend(
            EXTERNAL_AGENT_INTEGRATIONS
                .iter()
                .filter(|entry| external_agent_enabled(stored, entry.id))
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
    let enabled = match backend {
        Some(AgentIntegrationBackend::Embedded) => state.is_some_and(|state| state.enabled),
        None => false,
    };
    let available = match backend {
        Some(AgentIntegrationBackend::Embedded) => embedded_cua_ready(),
        None => false,
    };
    servers.push(AgentSessionMcpServer {
        name: CUA_DRIVER_MCP_NAME.to_string(),
        kind: AgentSessionIntegrationKind::Mcp,
        display_name: CUA_DRIVER_CARD_NAME.to_string(),
        description: CUA_DRIVER_CARD_DESCRIPTION.to_string(),
        transport: match backend {
            Some(AgentIntegrationBackend::Embedded) => "embedded",
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
    let mut registry: StoredIntegrationRegistry = match u32::try_from(version) {
        Ok(INTEGRATIONS_FILE_VERSION) => serde_json::from_str(&contents).map_err(|error| {
            format!("Failed to parse device-local integration settings: {error}")
        })?,
        _ => {
            return Err(format!(
                "Unsupported device-local integration settings version {version}"
            ));
        }
    };
    // Entries that chose the retired standalone CuaDriver are dropped, not
    // migrated: the file stays readable and the card falls back to "not set
    // up" until the user enables built-in CUA.
    let before = registry.integrations.len();
    registry
        .integrations
        .retain(|entry| entry.backend != StoredBackend::Retired);
    if registry.integrations.len() != before {
        log::warn!("Ignoring a device-local integration entry that selected a retired backend");
    }
    validate_stored_registry(registry)
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
        if (entry.id != CUA_DRIVER_INTEGRATION_ID && external_agent_selection(&entry.id).is_none())
            || !ids.insert(entry.id.as_str())
        {
            return Err(
                "Device-local integration settings contain an unknown or duplicate integration"
                    .to_string(),
            );
        }
    }
    Ok(registry)
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
        let availability = if permissions.ready() {
            AgentIntegrationAvailability::Available
        } else {
            AgentIntegrationAvailability::SetupRequired
        };
        // Say what is missing on the card itself, and say it in terms of what
        // is left to do rather than repeating the requirement's name.
        let detail = super::cua::desktop_helper_hint();
        CuaDetection {
            availability,
            permissions: Some(permissions),
            setup_available: super::cua::desktop_setup_available(),
            detail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cua_only(cua: CuaDetection) -> IntegrationDetections {
        IntegrationDetections {
            claude: ClaudeDetection::default(),
            cua,
            codex: CodexDetection::default(),
        }
    }

    fn codex_registry(enabled: bool) -> StoredIntegrationRegistry {
        provider_registry("codex", enabled)
    }

    fn provider_registry(provider: &str, enabled: bool) -> StoredIntegrationRegistry {
        StoredIntegrationRegistry {
            version: INTEGRATIONS_FILE_VERSION,
            integrations: vec![StoredIntegration {
                id: provider.to_string(),
                enabled,
                backend: StoredBackend::Embedded,
            }],
        }
    }

    #[tokio::test]
    async fn tasks_require_both_settings_and_persisted_provider_selection() {
        let temp = tempfile::tempdir().unwrap();
        let manager = SessionManager::new(temp.path().join("sessions"));
        let session = manager
            .create_session(
                temp.path().to_path_buf(),
                "Old task".into(),
                SessionType::User,
                GooseMode::Auto,
            )
            .await
            .unwrap();
        assert!(
            session_external_agent_providers(&codex_registry(false), &session, true).is_empty()
        );
        assert!(session_external_agent_providers(&codex_registry(true), &session, true).is_empty());
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
        assert!(
            session_external_agent_providers(&codex_registry(false), &enabled, true).is_empty()
        );
        assert_eq!(
            session_external_agent_providers(&codex_registry(true), &enabled, true),
            ["codex"]
        );
        let other_session = manager
            .create_session(
                temp.path().to_path_buf(),
                "Other task".into(),
                SessionType::User,
                GooseMode::Auto,
            )
            .await
            .unwrap();
        assert!(
            session_external_agent_providers(&codex_registry(true), &other_session, true)
                .is_empty()
        );
        drop(manager);
        let manager = SessionManager::new(temp.path().join("sessions"));
        let restored = manager.get_session(&session.id, false).await.unwrap();
        assert_eq!(
            session_external_agent_providers(&codex_registry(true), &restored, true),
            ["codex"]
        );
    }

    #[test]
    fn composer_lists_only_settings_enabled_providers_without_selecting_them() {
        let session = Session {
            session_type: SessionType::User,
            ..Session::default()
        };
        let rows =
            project_session_mcp_servers(&StoredIntegrationRegistry::default(), &[], &session)
                .unwrap();
        assert!(
            !rows
                .iter()
                .any(|row| row.kind == AgentSessionIntegrationKind::ExternalAgent)
        );
        for provider in ["codex", "claude"] {
            let rows =
                project_session_mcp_servers(&provider_registry(provider, true), &[], &session)
                    .unwrap();
            let native: Vec<_> = rows
                .iter()
                .filter(|row| row.kind == AgentSessionIntegrationKind::ExternalAgent)
                .collect();
            assert_eq!(native.len(), 1);
            assert_eq!(native[0].name, provider);
            assert!(!native[0].enabled);
        }
        assert!(
            rows.iter()
                .any(|row| row.name == CUA_DRIVER_MCP_NAME && !row.enabled && !row.available)
        );
        let mut custom = http_server("codex");
        custom.enabled = false;
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
                .any(|row| row.kind == AgentSessionIntegrationKind::ExternalAgent && !row.enabled)
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
        assert_resumed_task_refreshes_provider("codex").await;
        assert_resumed_task_refreshes_provider("claude").await;
    }

    async fn assert_resumed_task_refreshes_provider(provider: &str) {
        let fixture =
            super::super::test_support::started_agent_runtime("integration-refresh").await;
        let service = &fixture.handle.service;
        let user = fixture.handle.user_id.as_ref();
        let paths = &service.host.paths;
        let (manager, transport) = {
            let runtime = service.inner.lock().await;
            let runtime = runtime.as_ref().unwrap();
            (
                runtime.session_manager.clone(),
                runtime.maple_api_session.clone(),
            )
        };
        let session = manager
            .create_session(
                fixture.project_root.clone(),
                "Before Codex".into(),
                SessionType::User,
                GooseMode::Auto,
            )
            .await
            .unwrap();
        let registry = Arc::new(ExternalAgentRegistry::new(ExternalAgentHost {
            service: service.clone(),
            runtime: fixture.handle.clone(),
            session_manager: manager.clone(),
            project_root: fixture.project_root.clone(),
            lifetime: CancellationToken::new(),
        }));
        let config = GooseAgentConfig::new(
            manager.clone(),
            Arc::new(PermissionManager::new(fixture.root.join("permissions"))),
            None,
            GooseMode::Auto,
            true,
            GoosePlatform::GooseDesktop,
        );
        let mut agent = Arc::new(Agent::with_config(config.clone()));
        let context = SharedAgentToolContext::new(AgentToolContextSpec::default());
        // Settings alone never selects a provider. Revocation removes tools
        // from warm and cold agents despite a saved selection; re-enabling
        // Settings preserves the task's choice. ACP never receives the tools.
        for (default, override_value, cold, desktop, expected) in [
            (false, None, false, true, false),
            (true, None, true, true, false),
            (false, None, false, true, false),
            (true, None, false, true, false),
            (true, Some(true), false, true, true),
            (false, Some(true), false, true, false),
            (true, Some(true), true, true, true),
            (false, Some(true), true, true, false),
            (true, Some(false), true, true, false),
            (true, Some(true), false, false, false),
        ] {
            save_stored_integrations(paths, user, &provider_registry(provider, default)).unwrap();
            if let Some(enabled) = override_value {
                persist_task_integration_override(&manager, &session.id, provider, enabled)
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
                &transport,
                SessionAgentConfiguration {
                    session: &session,
                    model: DEFAULT_AGENT_MODEL,
                    context_limit: None,
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
        save_stored_integrations(paths, user, &provider_registry(provider, false)).unwrap();
        let error = fixture
            .handle
            .set_session_mcp_server_enabled(AgentSetSessionMcpServerRequest {
                session_id: session.id.clone(),
                name: provider.to_string(),
                kind: AgentSessionIntegrationKind::ExternalAgent,
                enabled: true,
            })
            .await
            .unwrap_err();
        assert!(
            error.contains("Enable this integration in Settings"),
            "{error}"
        );
        let hidden = fixture
            .handle
            .list_session_mcp_servers(session.id.clone())
            .await
            .unwrap();
        assert!(
            !hidden
                .iter()
                .any(|row| row.kind == AgentSessionIntegrationKind::ExternalAgent)
        );
        let rows = fixture
            .handle
            .set_session_mcp_server_enabled(AgentSetSessionMcpServerRequest {
                session_id: session.id.clone(),
                name: provider.to_string(),
                kind: AgentSessionIntegrationKind::ExternalAgent,
                enabled: false,
            })
            .await
            .unwrap();
        assert!(
            !rows
                .iter()
                .any(|row| row.kind == AgentSessionIntegrationKind::ExternalAgent)
        );
        let stored_task = manager.get_session(&session.id, false).await.unwrap();
        assert!(
            session_external_agent_providers(
                &provider_registry(provider, true),
                &stored_task,
                true
            )
            .is_empty()
        );
        // A future provider selected alongside an installed but unselected
        // Codex must not grant Codex access through forged tool arguments.
        use goose::agents::mcp_client::McpClientTrait;
        let client = MapleDeveloperClient::new(
            agent.extension_manager.get_context().clone(),
            false,
            transport,
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
                    Some(rmcp::object!({"provider": provider})),
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
        save_stored_integrations(
            &first,
            user,
            &StoredIntegrationRegistry {
                version: INTEGRATIONS_FILE_VERSION,
                integrations: vec![StoredIntegration {
                    id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                    enabled: true,
                    backend: StoredBackend::Embedded,
                }],
            },
        )
        .unwrap();

        let first_device = stored_integrations_for_read(&first, user);
        let entry = first_device
            .cua()
            .expect("the first device stored a choice");
        assert!(entry.enabled);
        assert_eq!(entry.backend, StoredBackend::Embedded);
        // A custom server under the integration's key is shadowed on the
        // device that stored the choice.
        assert_eq!(
            effective_mcp_servers(&first_device, vec![http_server("cua-driver")]).unwrap(),
            Vec::new()
        );
        assert!(stored_integrations_for_read(&second, user).cua().is_none());
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
    fn embedded_integration_shadows_every_legacy_cua_alias() {
        let stored = StoredIntegrationRegistry {
            version: INTEGRATIONS_FILE_VERSION,
            integrations: vec![StoredIntegration {
                id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                enabled: true,
                backend: StoredBackend::Embedded,
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
        let detection = CuaDetection {
            availability: AgentIntegrationAvailability::Available,
            setup_available: true,
            permissions: Some(AgentIntegrationPermissions::none_required()),
            detail: None,
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
        assert_eq!(enabled[0].backend, Some(AgentIntegrationBackend::Embedded));
        // Embedded CUA is Maple metadata, not an MCP server: the custom list
        // is untouched.
        let effective = effective_mcp_servers(
            &stored_integrations_for_read(&paths, user),
            load_agent_config_inner(&paths, user).unwrap().mcp_servers,
        )
        .unwrap();
        assert_eq!(effective, vec![custom.clone()]);

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
        assert!(
            stored_integrations_for_read(&paths, user)
                .cua()
                .is_some_and(|entry| !entry.enabled),
            "disabling keeps the managed entry, switched off"
        );
        let effective = effective_mcp_servers(
            &stored_integrations_for_read(&paths, user),
            load_agent_config_inner(&paths, user).unwrap().mcp_servers,
        )
        .unwrap();
        assert_eq!(effective, vec![custom]);
    }

    /// Files from builds that offered the standalone CuaDriver still load:
    /// a version-1 file is unsupported and read as empty, and a version-2
    /// entry that selected the `external` backend is dropped while the other
    /// entries survive.
    #[test]
    fn registries_that_name_the_retired_standalone_backend_still_load() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AgentPathLayout::from_app_roots(
            temporary.path().join("config"),
            temporary.path().join("local-data"),
        );
        let user = "cua-retired@example.com";
        let path = integrations_path(&paths, user).unwrap();
        write_device_local_json_file(
            &path,
            &json!({
                "version": 1,
                "integrations": [{
                    "id": CUA_DRIVER_INTEGRATION_ID,
                    "server": http_server("cua-driver"),
                }],
            }),
        )
        .unwrap();
        assert!(load_stored_integrations(&paths, user).is_err());
        assert!(stored_integrations_for_read(&paths, user).cua().is_none());

        write_device_local_json_file(
            &path,
            &json!({
                "version": INTEGRATIONS_FILE_VERSION,
                "integrations": [
                    {
                        "id": CUA_DRIVER_INTEGRATION_ID,
                        "enabled": true,
                        "backend": "external",
                        "externalServer": http_server("cua-driver"),
                    },
                    { "id": "codex", "enabled": true, "backend": "embedded" }
                ],
            }),
        )
        .unwrap();
        let loaded = load_stored_integrations(&paths, user).unwrap();
        assert!(loaded.cua().is_none(), "the retired entry is dropped");
        assert!(external_agent_enabled(&loaded, "codex"));
    }

    #[test]
    fn explicit_setup_switches_only_new_tasks_to_embedded() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AgentPathLayout::from_app_roots(
            temporary.path().join("config"),
            temporary.path().join("local-data"),
        );
        let user = "cua-embedded@example.com";
        save_stored_integrations(
            &paths,
            user,
            &StoredIntegrationRegistry {
                version: INTEGRATIONS_FILE_VERSION,
                integrations: vec![StoredIntegration {
                    id: CUA_DRIVER_INTEGRATION_ID.to_string(),
                    enabled: true,
                    backend: StoredBackend::Embedded,
                }],
            },
        )
        .unwrap();
        let detection = CuaDetection {
            availability: AgentIntegrationAvailability::Available,
            setup_available: true,
            permissions: Some(AgentIntegrationPermissions::none_required()),
            detail: None,
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
            claude: ClaudeDetection::default(),
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
                claude: ClaudeDetection::default(),
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
            claude: ClaudeDetection::default(),
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
            claude: ClaudeDetection::default(),
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
    #[test]
    fn claude_card_reports_authentication_without_gating_availability() {
        for (signed_in, detail) in [
            (Some(true), "Signed in."),
            (Some(false), claude::sign_in_hint()),
            (
                None,
                "Could not check sign-in. Run `claude auth status` in a terminal.",
            ),
        ] {
            let card = claude_public(
                &ClaudeDetection {
                    executable: Some(PathBuf::from("claude")),
                    signed_in,
                    ..Default::default()
                },
                None,
            );
            assert_eq!(card.detail.as_deref(), Some(detail));
            assert_eq!(card.availability, AgentIntegrationAvailability::Available);
        }
    }

    #[test]
    fn claude_setup_and_defaults_keep_shared_skills_until_both_are_off() {
        let temp = tempfile::tempdir().unwrap();
        let paths =
            AgentPathLayout::from_app_roots(temp.path().join("config"), temp.path().join("data"));
        let mut detections = cua_only(CuaDetection::not_detected());
        let request = AgentSetIntegrationEnabledRequest {
            id: "claude".into(),
            enabled: true,
        };
        assert!(
            set_integration_default(&paths, "user", &request, &detections)
                .unwrap_err()
                .contains("Install Claude Code")
        );
        detections.claude = ClaudeDetection {
            executable: Some(temp.path().join("claude")),
            version: Some("2.1.270".into()),
            signed_in: None,
            problem: Some("Claude Code could not be started".into()),
        };
        assert!(
            set_integration_default(&paths, "user", &request, &detections)
                .unwrap_err()
                .contains("could not be started")
        );
        detections.claude.problem = None;
        save_stored_integrations(&paths, "user", &codex_registry(true)).unwrap();
        set_integration_default(&paths, "user", &request, &detections).unwrap();
        set_integration_default(
            &paths,
            "user",
            &AgentSetIntegrationEnabledRequest {
                id: "codex".into(),
                enabled: false,
            },
            &detections,
        )
        .unwrap();
        assert!(external_agents_enabled(&paths, "user"));
        let skills = external_agent_skills_dir(&paths, "user").unwrap();
        assert!(skills.join("handoff/SKILL.md").is_file());
        set_integration_default(
            &paths,
            "user",
            &AgentSetIntegrationEnabledRequest {
                id: "claude".into(),
                enabled: false,
            },
            &detections,
        )
        .unwrap();
        assert!(!external_agents_enabled(&paths, "user"));
        assert!(!skills.join("handoff/SKILL.md").exists());
    }
}
