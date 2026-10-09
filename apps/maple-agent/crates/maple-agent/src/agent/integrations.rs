//! Maple-curated integrations on this device: built-in computer use (CUA),
//! and the Codex and Claude Code command lines as external agents. A card
//! joins what Maple finds on the device with the choice saved for new tasks
//! in the account's device-local `integrations.json`.
//!
//! The catalog, saved choices and detection are Goose's. What a choice does
//! in a task moves with embedded CUA and external agents; until then CUA
//! reads as not available, and an external agent switched on for new tasks
//! is not offered to them yet.

mod detect;

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::config::{
    account_local_data_dir_path, load_agent_config_inner, write_device_local_json_file,
};
use super::mcp::{name_to_key, normalize_mcp_servers};
use super::{
    AgentIntegration, AgentIntegrationAvailability, AgentIntegrationBackend,
    AgentIntegrationPermissions, AgentMcpServer, AgentPathLayout, AgentRuntimeHandle,
    AgentSetIntegrationEnabledRequest, AgentSetupIntegrationRequest,
};
use detect::CliDetection;

const CUA_INTEGRATION_ID: &str = "cua-driver";
/// The integration's name in errors.
const CUA_NAME: &str = "Computer use (CUA)";
/// Every spelling of the integration's name a custom server could take.
const CUA_NAMES: [&str; 4] = [CUA_INTEGRATION_ID, CUA_NAME, "Cua Driver", "cua_driver"];
const CUA_CARD_NAME: &str = "Cua";
const CUA_CARD_DESCRIPTION: &str = "Let Maple see and control apps on this computer.";

/// An external coding agent a task can hand work to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalAgent {
    Codex,
    Claude,
}

impl ExternalAgent {
    const ALL: [Self; 2] = [Self::Codex, Self::Claude];

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|agent| agent.id() == id)
    }

    fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Codex => {
                "Let a task hand work to the Codex CLI installed on this computer, with its own account."
            }
            Self::Claude => {
                "Let a task hand work to the Claude Code CLI installed on this computer, with its own account."
            }
        }
    }

    /// The card's note: what is missing, or whether the CLI is signed in.
    /// Sign-in is reported but does not gate enabling: the CLI itself says
    /// what to do when it is missing.
    fn detail(self, detection: &CliDetection) -> Option<String> {
        let detail = match (self, &detection.executable, &detection.problem) {
            (Self::Codex, None, _) => {
                "Install the Codex CLI and make sure `codex` is on your PATH, then reopen this page."
            }
            (Self::Claude, None, _) => {
                "Install Claude Code and make sure `claude` is on PATH, then reopen this page."
            }
            (_, Some(_), Some(problem)) => return Some(problem.clone()),
            (Self::Codex, Some(_), None) => match detection.signed_in {
                Some(true) => "Signed in.",
                Some(false) => detect::CODEX_SIGN_IN_HINT,
                None => return None,
            },
            (Self::Claude, Some(_), None) => match detection.signed_in {
                Some(true) => "Signed in.",
                Some(false) => detect::CLAUDE_SIGN_IN_HINT,
                None => "Could not check sign-in. Run `claude auth status` in a terminal.",
            },
        };
        Some(detail.to_string())
    }

    fn card(
        self,
        detection: &CliDetection,
        stored: Option<&StoredIntegration>,
    ) -> AgentIntegration {
        AgentIntegration {
            id: self.id().to_string(),
            name: self.name().to_string(),
            description: self.description().to_string(),
            availability: match (&detection.executable, &detection.problem) {
                (None, _) => AgentIntegrationAvailability::NotDetected,
                (Some(_), Some(_)) => AgentIntegrationAvailability::SetupRequired,
                (Some(_), None) => AgentIntegrationAvailability::Available,
            },
            backend: stored.map(StoredIntegration::backend),
            version: detection.version.clone(),
            permissions: None,
            setup_available: false,
            enabled_for_new_tasks: stored.is_some_and(|entry| entry.enabled),
            detail: self.detail(detection),
        }
    }
}

impl AgentIntegration {
    /// Whether this card is an external coding agent.
    pub fn is_external_agent(&self) -> bool {
        ExternalAgent::from_id(&self.id).is_some()
    }
}

/// Every integration Maple curates, so the entry points that take an id
/// cannot disagree.
fn require_known_integration(id: &str) -> Result<(), String> {
    let id = id.trim();
    if id == CUA_INTEGRATION_ID || ExternalAgent::from_id(id).is_some() {
        Ok(())
    } else {
        Err(format!("Unknown integration '{id}'"))
    }
}

/// Start the host-owned setup of a built-in integration.
pub fn begin_integration_setup(
    request: &AgentSetupIntegrationRequest,
) -> Result<AgentIntegrationPermissions, String> {
    require_known_integration(&request.id)?;
    Err("Built-in CUA setup is not available in this build of Maple yet".to_string())
}

/// Whether a server name is one of the computer use integration's.
pub(super) fn is_cua_identity(name: &str) -> bool {
    let key = name_to_key(name.trim());
    CUA_NAMES
        .iter()
        .any(|candidate| name_to_key(candidate) == key)
}

/// Refuse a custom server under the computer use integration's name.
fn ensure_no_custom_integration_collision(custom: &[AgentMcpServer]) -> Result<(), String> {
    match custom.iter().find(|server| is_cua_identity(&server.name)) {
        Some(server) => Err(format!(
            "Custom MCP server '{}' conflicts with the {CUA_NAME} integration. Rename or remove the custom server before enabling the integration.",
            server.name
        )),
        None => Ok(()),
    }
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
    let added: Vec<AgentMcpServer> = next
        .iter()
        .filter(|server| !existing.contains(&name_to_key(&server.name)))
        .cloned()
        .collect();
    ensure_no_custom_integration_collision(&added)
}

const INTEGRATIONS_FILE_NAME: &str = "integrations.json";
const INTEGRATIONS_FILE_VERSION: u32 = 2;

/// The choices saved on this device for the account's new tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredIntegrations {
    version: u32,
    #[serde(default)]
    integrations: Vec<StoredIntegration>,
}

impl Default for StoredIntegrations {
    fn default() -> Self {
        Self {
            version: INTEGRATIONS_FILE_VERSION,
            integrations: Vec::new(),
        }
    }
}

impl StoredIntegrations {
    fn entry(&self, id: &str) -> Option<&StoredIntegration> {
        self.integrations.iter().find(|entry| entry.id == id)
    }

    fn entry_mut(&mut self, id: &str) -> Option<&mut StoredIntegration> {
        self.integrations.iter_mut().find(|entry| entry.id == id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredIntegration {
    id: String,
    enabled: bool,
    backend: StoredBackend,
}

/// The implementation chosen. Files from builds that offered the standalone
/// CuaDriver say `external`; those entries are dropped when the file loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredBackend {
    Embedded,
    #[serde(other)]
    Retired,
}

impl StoredIntegration {
    fn backend(&self) -> AgentIntegrationBackend {
        match self.backend {
            StoredBackend::Embedded | StoredBackend::Retired => AgentIntegrationBackend::Embedded,
        }
    }
}

fn integrations_path(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, String> {
    Ok(account_local_data_dir_path(paths, user_id)
        .map_err(|error| error.to_string())?
        .join(INTEGRATIONS_FILE_NAME))
}

fn load_stored_integrations(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<StoredIntegrations, String> {
    let path = integrations_path(paths, user_id)?;
    if !path
        .try_exists()
        .map_err(|error| format!("Failed to inspect device-local integration settings: {error}"))?
    {
        return Ok(StoredIntegrations::default());
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Failed to read device-local integration settings: {error}"))?;
    let parse_error = |error: serde_json::Error| {
        format!("Failed to parse device-local integration settings: {error}")
    };
    let version = serde_json::from_str::<Value>(&contents)
        .map_err(parse_error)?
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Device-local integration settings have no version".to_string())?;
    if version != u64::from(INTEGRATIONS_FILE_VERSION) {
        return Err(format!(
            "Unsupported device-local integration settings version {version}"
        ));
    }
    let mut stored: StoredIntegrations = serde_json::from_str(&contents).map_err(parse_error)?;
    // A choice of the retired standalone CuaDriver is dropped, not migrated:
    // the card reads as not set up until built-in CUA is enabled.
    let before = stored.integrations.len();
    stored
        .integrations
        .retain(|entry| entry.backend != StoredBackend::Retired);
    if stored.integrations.len() != before {
        log::warn!("Ignoring a device-local integration entry that selected a retired backend");
    }
    validate_stored_integrations(&stored)?;
    Ok(stored)
}

fn validate_stored_integrations(stored: &StoredIntegrations) -> Result<(), String> {
    if stored.version != INTEGRATIONS_FILE_VERSION {
        return Err(format!(
            "Unsupported device-local integration settings version {}",
            stored.version
        ));
    }
    let mut ids = HashSet::new();
    for entry in &stored.integrations {
        if (entry.id != CUA_INTEGRATION_ID && ExternalAgent::from_id(&entry.id).is_none())
            || !ids.insert(entry.id.as_str())
        {
            return Err(
                "Device-local integration settings contain an unknown or duplicate integration"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn save_stored_integrations(
    paths: &AgentPathLayout,
    user_id: &str,
    stored: &StoredIntegrations,
) -> Result<(), String> {
    validate_stored_integrations(stored)?;
    write_device_local_json_file(&integrations_path(paths, user_id)?, stored)
        .map_err(|error| format!("Failed to save device-local integration settings: {error}"))
}

/// What Maple found out about built-in CUA.
#[derive(Debug)]
struct CuaDetection {
    availability: AgentIntegrationAvailability,
    permissions: Option<AgentIntegrationPermissions>,
    setup_available: bool,
    detail: Option<String>,
}

impl CuaDetection {
    fn not_available() -> Self {
        Self {
            availability: AgentIntegrationAvailability::NotDetected,
            permissions: None,
            setup_available: false,
            detail: Some("Built-in CUA is not available in this build of Maple yet.".to_string()),
        }
    }

    fn ready(&self) -> bool {
        self.permissions
            .as_ref()
            .is_some_and(AgentIntegrationPermissions::ready)
    }

    fn card(&self, stored: Option<&StoredIntegration>) -> AgentIntegration {
        AgentIntegration {
            id: CUA_INTEGRATION_ID.to_string(),
            name: CUA_CARD_NAME.to_string(),
            description: CUA_CARD_DESCRIPTION.to_string(),
            availability: self.availability,
            backend: stored.map(StoredIntegration::backend),
            version: None,
            permissions: self.permissions.clone(),
            setup_available: self.setup_available,
            enabled_for_new_tasks: stored.is_some_and(|entry| entry.enabled),
            detail: self.detail.clone(),
        }
    }
}

/// Everything found out about the curated integrations on this device.
struct Detections {
    cua: CuaDetection,
    codex: CliDetection,
    claude: CliDetection,
}

impl Detections {
    /// Look on `search_path`, when the host knows a fuller one than the
    /// process's own, as after a launch from the Dock.
    async fn find(search_path: Option<&str>) -> Self {
        let (codex, claude) = tokio::join!(
            detect::detect_codex(search_path),
            detect::detect_claude(search_path)
        );
        Self {
            cua: CuaDetection::not_available(),
            codex,
            claude,
        }
    }

    fn of(&self, agent: ExternalAgent) -> &CliDetection {
        match agent {
            ExternalAgent::Codex => &self.codex,
            ExternalAgent::Claude => &self.claude,
        }
    }
}

fn project_integrations(
    paths: &AgentPathLayout,
    user_id: &str,
    detections: &Detections,
) -> Result<Vec<AgentIntegration>, String> {
    let stored = load_stored_integrations(paths, user_id)?;
    let mut cards = vec![detections.cua.card(stored.entry(CUA_INTEGRATION_ID))];
    cards.extend(
        ExternalAgent::ALL
            .into_iter()
            .map(|agent| agent.card(detections.of(agent), stored.entry(agent.id()))),
    );
    Ok(cards)
}

fn set_integration_default(
    paths: &AgentPathLayout,
    user_id: &str,
    request: &AgentSetIntegrationEnabledRequest,
    detections: &Detections,
) -> Result<Vec<AgentIntegration>, String> {
    let id = request.id.trim();
    require_known_integration(id)?;
    match ExternalAgent::from_id(id) {
        Some(agent) => set_external_agent_default(
            paths,
            user_id,
            request.enabled,
            agent,
            detections.of(agent),
        )?,
        None => set_cua_default(paths, user_id, request.enabled, &detections.cua)?,
    }
    project_integrations(paths, user_id, detections)
}

/// Enabling needs a usable installation; disabling never looks at it, so a
/// removed CLI can still be switched off.
fn set_external_agent_default(
    paths: &AgentPathLayout,
    user_id: &str,
    enabled: bool,
    agent: ExternalAgent,
    detection: &CliDetection,
) -> Result<(), String> {
    let mut stored = load_stored_integrations(paths, user_id)?;
    if enabled {
        let card = agent.card(detection, None);
        if card.availability != AgentIntegrationAvailability::Available {
            return Err(card
                .detail
                .unwrap_or_else(|| format!("Set up {} in Integrations first", agent.name())));
        }
    }
    match stored.entry_mut(agent.id()) {
        Some(entry) => entry.enabled = enabled,
        None if enabled => stored.integrations.push(StoredIntegration {
            id: agent.id().to_string(),
            enabled,
            backend: StoredBackend::Embedded,
        }),
        None => {}
    }
    save_stored_integrations(paths, user_id, &stored)
}

fn set_cua_default(
    paths: &AgentPathLayout,
    user_id: &str,
    enabled: bool,
    detection: &CuaDetection,
) -> Result<(), String> {
    let mut stored = load_stored_integrations(paths, user_id)?;
    if enabled {
        ensure_no_custom_integration_collision(&saved_mcp_servers(paths, user_id)?)?;
        if !detection.ready() {
            return Err(
                "Set up Maple's Accessibility and Screen Recording permissions before enabling built-in CUA"
                    .to_string(),
            );
        }
        match stored.entry_mut(CUA_INTEGRATION_ID) {
            Some(entry) => {
                entry.backend = StoredBackend::Embedded;
                entry.enabled = true;
            }
            None => stored.integrations.push(StoredIntegration {
                id: CUA_INTEGRATION_ID.to_string(),
                enabled: true,
                backend: StoredBackend::Embedded,
            }),
        }
    } else if let Some(entry) = stored.entry_mut(CUA_INTEGRATION_ID) {
        entry.enabled = false;
    }
    save_stored_integrations(paths, user_id, &stored)
}

/// Record built-in CUA as the backend of new tasks once the system reports
/// both permissions. An unfinished setup leaves the saved choice alone.
fn select_embedded_backend(
    paths: &AgentPathLayout,
    user_id: &str,
    detections: &Detections,
) -> Result<Vec<AgentIntegration>, String> {
    if detections.cua.ready() {
        ensure_no_custom_integration_collision(&saved_mcp_servers(paths, user_id)?)?;
        let mut stored = load_stored_integrations(paths, user_id)?;
        match stored.entry_mut(CUA_INTEGRATION_ID) {
            Some(entry) => entry.backend = StoredBackend::Embedded,
            None => stored.integrations.push(StoredIntegration {
                id: CUA_INTEGRATION_ID.to_string(),
                enabled: false,
                backend: StoredBackend::Embedded,
            }),
        }
        save_stored_integrations(paths, user_id, &stored)?;
    }
    project_integrations(paths, user_id, detections)
}

fn saved_mcp_servers(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<Vec<AgentMcpServer>, String> {
    normalize_mcp_servers(
        load_agent_config_inner(paths, user_id)
            .map_err(|error| format!("Failed to load MCP servers: {error}"))?
            .mcp_servers,
    )
}

impl AgentRuntimeHandle {
    /// Detect the curated integrations. Detection runs commands, so it runs
    /// before the settings are locked, and the handle is checked on each
    /// side so a signed-out account cannot read or change the saved choices.
    async fn detect_integrations(&self) -> Result<Detections, String> {
        self.verify_generation().await?;
        let search_path = super::login_path::known_login_search_path();
        Ok(Detections::find(search_path.as_deref()).await)
    }

    /// Maple-curated integrations found on this device.
    pub async fn list_integrations(&self) -> Result<Vec<AgentIntegration>, String> {
        let detections = self.detect_integrations().await?;
        let _settings = self.lock_settings().await;
        self.verify_generation().await?;
        project_integrations(self.paths(), &self.user_id, &detections)
    }

    /// Switch one curated integration on or off for new tasks. Existing
    /// tasks keep what they started with.
    pub async fn set_integration_enabled(
        &self,
        request: AgentSetIntegrationEnabledRequest,
    ) -> Result<Vec<AgentIntegration>, String> {
        require_known_integration(&request.id)?;
        let detections = self.detect_integrations().await?;
        let _settings = self.lock_settings().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        set_integration_default(self.paths(), &self.user_id, &request, &detections)
    }

    /// Finish the host-owned setup of a built-in integration, after
    /// [`begin_integration_setup`]: built-in CUA becomes the backend of new
    /// tasks once the system reports both permissions.
    pub async fn setup_integration(
        &self,
        request: AgentSetupIntegrationRequest,
    ) -> Result<Vec<AgentIntegration>, String> {
        require_known_integration(&request.id)?;
        let detections = self.detect_integrations().await?;
        let _settings = self.lock_settings().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        select_embedded_backend(self.paths(), &self.user_id, &detections)
    }
}

#[cfg(test)]
mod tests;
