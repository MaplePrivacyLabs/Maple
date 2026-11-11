//! Maple's account defaults and projects in the host's injected local store.
//!
//! Defaults use account settings; project names, order, trust and removal use
//! typed project records. No JSON sidecar is read, adopted or written. General
//! instruction Markdown, prompts and skills may be read from a separately
//! configured instruction namespace, which this module never creates.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::types::*;
use super::{AgentPathLayout, LEGACY_AGENT_DEFAULT_MODEL, PREVIOUS_RECOMMENDED_AGENT_MODEL};
use crate::maple_api::account_scope;
#[cfg(test)]
use crate::store::ProjectRecord;
use crate::store::{AccountId, Store};

const AGENT_DEFAULTS_KEY: &str = "agent.defaults";

/// Project data and MCP definitions have their own authoritative SQL records.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredAgentDefaults {
    default_project_root: Option<String>,
    #[serde(default = "default_agent_model")]
    default_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_thinking_level: Option<ThinkingLevel>,
}

pub(super) fn account_store<'a>(
    paths: &'a AgentPathLayout,
    user_id: &str,
) -> Result<(&'a Store, AccountId), anyhow::Error> {
    let store = paths.store().map_err(anyhow::Error::msg)?;
    let account = match store.account_id(user_id)? {
        Some(account) => account,
        None => store.upsert_account(user_id, store_now_ms())?,
    };
    Ok((store.as_ref(), account))
}

fn store_now_ms() -> i64 {
    i64::try_from(unix_ms()).unwrap_or(i64::MAX)
}

pub(super) fn stopped_status() -> AgentRuntimeStatus {
    AgentRuntimeStatus {
        running: false,
        project_root: None,
        model: None,
        active_runs: HashMap::new(),
    }
}

pub(super) fn resolve_project_root(
    requested: Option<&str>,
    config: &AgentConfig,
) -> Result<PathBuf, String> {
    if let Some(path) = requested.filter(|value| !value.trim().is_empty()) {
        return normalize_project_root(Path::new(path));
    }

    // A removed project retains a tombstone even when a saved default still
    // names it. Never boot into a hidden root.
    if let Some(path) = config
        .default_project_root
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        && !is_removed_project_root(path, &config.removed_project_roots)
        && let Ok(root) = normalize_project_root(Path::new(path))
        && !is_removed_project_root(&path_string(&root), &config.removed_project_roots)
    {
        return Ok(root);
    }

    std::env::current_dir()
        .map_err(|e| format!("Failed to read current directory: {e}"))
        .and_then(|path| normalize_project_root(&path))
}

pub(super) fn is_removed_project_root(path: &str, removed_project_roots: &[String]) -> bool {
    removed_project_roots.iter().any(|removed| removed == path)
}

pub(super) fn normalize_project_root(path: &Path) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a folder", canonical.display()));
    }
    Ok(canonical)
}

/// Generated delegation skills are the only files Maple creates for an account.
/// User-authored general instruction files keep their separate read-only root.
pub(super) fn account_generated_resource_dir(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<PathBuf, anyhow::Error> {
    let scope = account_scope(user_id).map_err(anyhow::Error::msg)?;
    Ok(paths.generated_resource_root.join("accounts").join(scope))
}

/// The account's existing general instructions, prompt templates and skills.
/// This path is read-only: locating it must not create or change a prior app
/// namespace, including its directory permissions.
pub(super) fn account_instruction_dir_path(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<PathBuf, anyhow::Error> {
    let scope = account_scope(user_id).map_err(anyhow::Error::msg)?;
    Ok(paths.instruction_config_root().join("accounts").join(scope))
}

pub(super) fn load_agent_config_inner(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<AgentConfig, anyhow::Error> {
    let (store, account) = account_store(paths, user_id)?;
    let mut config = AgentConfig::default();
    if let Some(value) = store.account_setting(account, AGENT_DEFAULTS_KEY)? {
        let defaults: StoredAgentDefaults = serde_json::from_value(value)?;
        config.default_project_root = defaults.default_project_root;
        config.default_model = defaults.default_model;
        config.default_thinking_level = defaults.default_thinking_level;
    }
    // Defaults carry only their target project's state. Navigation never
    // reconstructs all account projects, trust entries or removal tombstones.
    if let Some(root) = config.default_project_root.clone() {
        load_project_state(store, account, &root, &mut config)?;
    }
    // The one well-known fallback also needs its own removal tombstone; a
    // missing saved folder must not resurrect a workspace the user hid.
    if let Some(workspace) = store.maple_workspace_path()? {
        load_project_state(store, account, &path_string(&workspace), &mut config)?;
    }
    config.mcp_servers =
        super::mcp::load_saved_mcp_servers(paths, user_id).map_err(anyhow::Error::msg)?;
    if migrate_agent_config(&mut config) {
        save_agent_config_inner(paths, user_id, &config)?;
    }
    Ok(config)
}

fn load_project_state(
    store: &Store,
    account: AccountId,
    root: &str,
    config: &mut AgentConfig,
) -> Result<(), anyhow::Error> {
    config.project_trust.retain(|entry| entry.path != root);
    config.removed_project_roots.retain(|entry| entry != root);
    if let Some(project) = store.project(account, root)? {
        if let Some(trusted) = project.trusted {
            config.project_trust.push(AgentProjectTrust {
                path: project.root.clone(),
                trusted,
            });
        }
        if project.removed_ms.is_some() {
            config.removed_project_roots.push(project.root);
        }
    }
    Ok(())
}

pub(super) fn load_project_config_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    project_root: &Path,
) -> Result<AgentConfig, anyhow::Error> {
    let mut config = load_agent_config_inner(paths, user_id)?;
    let (store, account) = account_store(paths, user_id)?;
    load_project_state(store, account, &path_string(project_root), &mut config)?;
    Ok(config)
}

/// Persist one default-root change without rewriting any project's trust,
/// removal state, display name, MCP defaults or manual order.
pub(super) fn save_default_project_root_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    root: Option<String>,
) -> Result<(), anyhow::Error> {
    let (store, account) = account_store(paths, user_id)?;
    let mut defaults = match store.account_setting(account, AGENT_DEFAULTS_KEY)? {
        Some(value) => serde_json::from_value::<StoredAgentDefaults>(value)?,
        None => StoredAgentDefaults {
            default_project_root: None,
            default_model: default_agent_model(),
            default_thinking_level: None,
        },
    };
    defaults.default_project_root = root;
    store.save_agent_defaults(
        account,
        &serde_json::to_value(defaults)?,
        &[],
        store_now_ms(),
    )?;
    Ok(())
}

pub(super) fn project_trust_decision_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    project_root: &Path,
) -> Result<Option<bool>, anyhow::Error> {
    let (store, account) = account_store(paths, user_id)?;
    let workspace = store.maple_workspace_path()?;
    Ok(store
        .project(account, &path_string(project_root))?
        .and_then(|project| project.trusted)
        .or_else(|| {
            is_implicitly_trusted_project_root(project_root, workspace.as_deref()).then_some(true)
        }))
}

pub(super) fn migrate_agent_config(config: &mut AgentConfig) -> bool {
    let mut changed = false;
    if config.default_model == LEGACY_AGENT_DEFAULT_MODEL
        || config.default_model == PREVIOUS_RECOMMENDED_AGENT_MODEL
        || config.default_model == "deepseek-v4-flash"
    {
        config.default_model = default_agent_model();
        changed = true;
    }
    let original_removed_roots = config.removed_project_roots.clone();
    config.removed_project_roots =
        sanitize_project_root_paths(std::mem::take(&mut config.removed_project_roots));
    changed || config.removed_project_roots != original_removed_roots
}

pub(super) fn save_agent_config_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    config: &AgentConfig,
) -> Result<(), anyhow::Error> {
    let (store, account) = account_store(paths, user_id)?;
    let defaults = serde_json::to_value(StoredAgentDefaults {
        default_project_root: config.default_project_root.clone(),
        default_model: config.default_model.clone(),
        default_thinking_level: config.default_thinking_level,
    })?;
    let trust = config
        .project_trust
        .iter()
        .map(|entry| (entry.path.clone(), entry.trusted))
        .collect::<Vec<_>>();
    store.save_agent_defaults(account, &defaults, &trust, store_now_ms())?;
    Ok(())
}

#[cfg(test)]
pub(super) fn save_removed_project_roots_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    roots: &[String],
) -> Result<(), anyhow::Error> {
    let (store, account) = account_store(paths, user_id)?;
    let roots = sanitize_project_root_paths(roots.to_vec());
    store.set_removed_project_roots(account, &roots, store_now_ms())?;
    Ok(())
}

pub(super) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

pub(super) fn canonical_dir(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok().filter(|path| path.is_dir())
}

pub(super) fn launch_dir() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .and_then(|path| canonical_dir(&path))
}

pub(super) fn is_filesystem_root(path: &Path) -> bool {
    path.parent().is_none()
}

pub(super) fn paths_are_same_dir(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (canonical_dir(left), canonical_dir(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Folder name under the platform documents directory.
pub const MAPLE_WORKSPACE_DIRECTORY_NAME: &str = "Maple";
const DEFAULT_WORKSPACE_DIRECTORY_NAME: &str = "Workspace";

/// Name shown for that folder in the project menus.
pub const MAPLE_WORKSPACE_DISPLAY_NAME: &str = "Maple Workspace";

/// `<documents>/Maple` for a documents directory the caller already resolved.
pub fn maple_workspace_directory(documents: &Path) -> PathBuf {
    documents.join(MAPLE_WORKSPACE_DIRECTORY_NAME)
}

/// Platform documents directory.
///
/// Linux uses the XDG documents directory, macOS uses `$HOME/Documents`,
/// and Windows uses `FOLDERID_Documents`. iOS uses `$HOME/Documents` when
/// the process home is set; the GPUI app has no iOS target, so this does
/// not call `NSDocumentDirectory`.
pub(super) fn document_dir() -> Option<PathBuf> {
    dirs::document_dir()
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join("Documents")))
        .filter(|path| path.is_absolute())
}

/// The parent of new projects, without creating it.
///
/// Resolved once per process: the UI labels project roots while it
/// renders, so recognizing the workspace must not touch the disk. The
/// documents folder is canonicalized the way every project root is, so a
/// plain comparison matches the root the runtime stores for it.
pub fn maple_project_container_path() -> Option<PathBuf> {
    static CONTAINER: once_cell::sync::OnceCell<Option<PathBuf>> = once_cell::sync::OnceCell::new();
    CONTAINER
        .get_or_init(|| {
            let documents = document_dir()?;
            let documents = canonical_dir(&documents).unwrap_or(documents);
            Some(maple_workspace_directory(&documents))
        })
        .clone()
}

pub(crate) fn maple_workspace_path(nested: bool) -> Option<PathBuf> {
    if nested {
        default_maple_workspace_path()
    } else {
        maple_project_container_path()
    }
}

/// Built-in workspace location for a newly initialized store.
pub fn default_maple_workspace_path() -> Option<PathBuf> {
    Some(maple_project_container_path()?.join(DEFAULT_WORKSPACE_DIRECTORY_NAME))
}

/// Create the default workspace when it is missing and return its canonical
/// path. Call this only when the workspace is about to be used (a runtime
/// start that picks it, or the user choosing or opening it), never at
/// launch: someone working elsewhere is not asked for Documents access,
/// and a folder the user deleted stays deleted until it is used again.
pub fn ensure_maple_workspace(path: &Path) -> Result<PathBuf, String> {
    let container = maple_project_container_path()
        .ok_or_else(|| "No documents folder for the Maple workspace".to_string())?;
    if path != container && path != container.join(DEFAULT_WORKSPACE_DIRECTORY_NAME) {
        return Err("Invalid Maple workspace path".into());
    }
    fs::create_dir_all(path)
        .map_err(|error| format!("Cannot create {}: {error}", path.display()))?;
    canonical_dir(path).ok_or_else(|| format!("{} is not a folder", path.display()))
}

/// Project the desktop opens when it has no usable saved root.
///
/// A saved folder that still exists and was not removed wins. Otherwise the
/// caller-supplied Maple workspace is the default. `None` means the caller
/// should use its own last resort (the home directory for the GUI).
pub fn startup_project_root(config: &AgentConfig, workspace: Option<&Path>) -> Option<String> {
    if let Some(path) = usable_saved_project_root(config) {
        return Some(path);
    }
    let workspace = path_string(workspace?);
    if is_removed_project_root(&workspace, &config.removed_project_roots) {
        return None;
    }
    Some(workspace)
}

pub(super) fn removed_contains_dir(removed: &[String], directory: &Path) -> bool {
    let path = path_string(directory);
    removed
        .iter()
        .any(|candidate| candidate == &path || paths_are_same_dir(Path::new(candidate), directory))
}

pub(super) fn usable_saved_project_root(config: &AgentConfig) -> Option<String> {
    let path = config.default_project_root.as_deref()?.trim();
    if path.is_empty() || !Path::new(path).is_dir() {
        return None;
    }
    if removed_contains_dir(&config.removed_project_roots, Path::new(path)) {
        return None;
    }
    Some(path.to_owned())
}

/// Offer the Maple workspace in the project list without moving roots the
/// user already saved. A removed workspace stays hidden even if an older
/// saved project record still names it. Listing never creates the folder, and
/// the comparisons are on the canonical path strings, so this does not
/// touch the Documents folder.
pub fn include_default_maple_workspace(
    mut roots: Vec<RecentProjectRoot>,
    removed: &[String],
    workspace: Option<&Path>,
) -> Vec<RecentProjectRoot> {
    let Some(workspace) = workspace else {
        return roots;
    };
    let path = path_string(workspace);
    let same_workspace = |root: &RecentProjectRoot| root.path == path;
    if is_removed_project_root(&path, removed) {
        roots.retain(|root| !same_workspace(root));
        return roots;
    }
    if roots.iter().any(same_workspace) {
        return roots;
    }
    roots.push(RecentProjectRoot {
        path,
        name: MAPLE_WORKSPACE_DISPLAY_NAME.to_string(),
        last_used_ms: 0,
        is_default_workspace: true,
        name_revision: 0,
    });
    roots
}

/// Home, the directory the process was started in, and the default Maple
/// workspace are trusted until the user records a different answer. The
/// filesystem root is never implicit: a GUI launched from the Dock often
/// has cwd `/`.
fn is_implicitly_trusted_project_root(project_root: &Path, workspace: Option<&Path>) -> bool {
    implicitly_trusted_project_root(
        project_root,
        home_dir().and_then(|path| canonical_dir(&path)).as_deref(),
        launch_dir()
            .filter(|path| !is_filesystem_root(path))
            .as_deref(),
        workspace,
    )
}

pub(super) fn implicitly_trusted_project_root(
    project_root: &Path,
    home: Option<&Path>,
    launch: Option<&Path>,
    maple_workspace: Option<&Path>,
) -> bool {
    if let Some(home) = home
        && paths_are_same_dir(project_root, home)
    {
        return true;
    }
    if let Some(launch) = launch
        && paths_are_same_dir(project_root, launch)
    {
        return true;
    }
    // A plain comparison: resolving the workspace on disk to rule out every
    // other project would read the Documents folder for each trust check.
    maple_workspace.is_some_and(|workspace| project_root == workspace)
}

/// The trust status of an existing project: the user's decision, and what
/// trusting the project adds to its tasks.
pub(super) fn project_trust_status(
    config: &AgentConfig,
    project_root: &Path,
    workspace: Option<&Path>,
    protected_features: Vec<AgentProjectTrustFeature>,
) -> AgentProjectTrustStatus {
    AgentProjectTrustStatus {
        path: path_string(project_root),
        decision: project_trust_decision(config, project_root, workspace),
        available: true,
        protected_features,
    }
}

/// The decision saved for `project_root`, else the implicit trust of home,
/// the launch folder and the Maple workspace.
pub(super) fn project_trust_decision(
    config: &AgentConfig,
    project_root: &Path,
    workspace: Option<&Path>,
) -> Option<bool> {
    let path = path_string(project_root);
    config
        .project_trust
        .iter()
        .find(|entry| entry.path == path)
        .map(|entry| entry.trusted)
        .or_else(|| is_implicitly_trusted_project_root(project_root, workspace).then_some(true))
}

pub(super) fn apply_project_trust(config: &mut AgentConfig, project_root: &Path, trusted: bool) {
    let path = path_string(project_root);
    if let Some(existing) = config
        .project_trust
        .iter_mut()
        .find(|entry| entry.path == path)
    {
        existing.trusted = trusted;
        return;
    }
    config
        .project_trust
        .push(AgentProjectTrust { path, trusted });
}

#[cfg(test)]
pub(super) fn load_recent_project_roots_inner(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let (store, account) = account_store(paths, user_id)?;
    recent_project_roots(store.account_projects(account)?)
}

#[cfg(test)]
fn recent_project_roots(
    projects: Vec<ProjectRecord>,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    projects
        .into_iter()
        .filter(|project| project.position.is_some() && project.removed_ms.is_none())
        .map(|project| {
            let used_ms = u128::try_from(project.last_used_ms)
                .map_err(|_| anyhow::anyhow!("Invalid saved project timestamp"))?;
            let mut root = project_root_record(project.root, used_ms);
            if let Some(name) = project.display_name {
                root.name = name;
            }
            Ok(root)
        })
        .collect()
}

#[cfg(test)]
fn persist_project_order(
    store: &Store,
    account: AccountId,
    roots: &[RecentProjectRoot],
) -> Result<(), anyhow::Error> {
    let order = roots
        .iter()
        .map(|root| {
            let used_ms = i64::try_from(root.last_used_ms)
                .map_err(|_| anyhow::anyhow!("Invalid project timestamp"))?;
            Ok((root.path.clone(), used_ms))
        })
        .collect::<Result<Vec<_>, anyhow::Error>>()?;
    store.replace_project_order(account, &order)?;
    Ok(())
}

pub(super) fn structurally_valid_project_root(path: &str) -> bool {
    !path.is_empty() && !path.contains('\0') && Path::new(path).is_absolute()
}

pub(super) fn sanitize_project_root_paths(paths: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| structurally_valid_project_root(path) && seen.insert(path.clone()))
        .collect()
}

#[cfg(test)]
pub(super) fn sanitize_recent_project_roots(
    roots: Vec<RecentProjectRoot>,
) -> Vec<RecentProjectRoot> {
    let mut seen = HashSet::new();
    roots
        .into_iter()
        .filter(|root| {
            structurally_valid_project_root(&root.path) && seen.insert(root.path.clone())
        })
        .collect()
}

#[cfg(test)]
pub(super) fn project_root_record(path: String, last_used_ms: u128) -> RecentProjectRoot {
    let name = Path::new(&path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(&path)
        .to_string();
    RecentProjectRoot {
        path,
        name,
        last_used_ms,
        is_default_workspace: false,
        name_revision: 0,
    }
}

#[cfg(test)]
pub(super) fn register_explicit_project_root(
    roots: Vec<RecentProjectRoot>,
    project_root: &Path,
    last_used_ms: u128,
) -> (Vec<RecentProjectRoot>, bool) {
    let original_len = roots.len();
    let mut roots = sanitize_recent_project_roots(roots);
    let sanitized = roots.len() != original_len;
    let path = path_string(project_root);
    if roots.iter().any(|root| root.path == path) {
        return (roots, sanitized);
    }

    roots.insert(0, project_root_record(path, last_used_ms));
    (roots, true)
}

#[cfg(test)]
pub(super) fn register_explicit_project_root_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    project_root: &Path,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let (store, account) = account_store(paths, user_id)?;
    let roots = recent_project_roots(store.account_projects(account)?)?;
    let (roots, changed) = register_explicit_project_root(roots, project_root, unix_ms());
    if changed {
        persist_project_order(store, account, &roots)?;
    }
    recent_project_roots(store.account_projects(account)?)
}

#[cfg(test)]
pub(super) fn restore_explicit_project_root(
    roots: Vec<RecentProjectRoot>,
    project_root: &Path,
    last_used_ms: u128,
) -> Vec<RecentProjectRoot> {
    let path = path_string(project_root);
    let mut roots = sanitize_recent_project_roots(roots)
        .into_iter()
        .filter(|root| root.path != path)
        .collect::<Vec<_>>();
    roots.insert(0, project_root_record(path, last_used_ms));
    roots
}

#[cfg(test)]
pub(super) fn restore_explicit_project_root_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    project_root: &Path,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let (store, account) = account_store(paths, user_id)?;
    let roots = restore_explicit_project_root(
        recent_project_roots(store.account_projects(account)?)?,
        project_root,
        unix_ms(),
    );
    // The caller clears the removal tombstone only after its other saves
    // succeed. Assigning order must not unhide the root prematurely.
    persist_project_order(store, account, &roots)?;
    Ok(roots)
}

#[cfg(test)]
pub(super) fn apply_project_root_order(
    roots: Vec<RecentProjectRoot>,
    paths: Vec<String>,
    last_used_ms: u128,
) -> Result<Vec<RecentProjectRoot>, String> {
    let roots = sanitize_recent_project_roots(roots);
    let mut requested_paths = Vec::new();
    let mut requested_set = HashSet::new();
    for path in paths {
        if structurally_valid_project_root(&path) && requested_set.insert(path.clone()) {
            requested_paths.push(path);
        }
    }

    let missing_paths = roots
        .iter()
        .filter(|root| !requested_set.contains(&root.path))
        .map(|root| root.path.clone())
        .collect::<Vec<_>>();
    if !missing_paths.is_empty() {
        return Err(format!(
            "Project order is stale and omitted known project roots: {}",
            missing_paths.join(", ")
        ));
    }

    let mut roots_by_path = roots
        .into_iter()
        .map(|root| (root.path.clone(), root))
        .collect::<HashMap<_, _>>();
    Ok(requested_paths
        .into_iter()
        .map(|path| {
            roots_by_path
                .remove(&path)
                .unwrap_or_else(|| project_root_record(path, last_used_ms))
        })
        .collect())
}

#[cfg(test)]
pub(super) fn save_project_root_order_inner(
    layout: &AgentPathLayout,
    user_id: &str,
    paths: Vec<String>,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let (store, account) = account_store(layout, user_id)?;
    let roots = recent_project_roots(store.account_projects(account)?)?;
    let roots = apply_project_root_order(roots, paths, unix_ms()).map_err(anyhow::Error::msg)?;
    persist_project_order(store, account, &roots)?;
    recent_project_roots(store.account_projects(account)?)
}

pub(super) fn update_runtime_project_root_after_removal(
    runtime_project_root: &mut PathBuf,
    removed_project_root: &str,
    fallback_path: Option<&str>,
) {
    if runtime_project_root == Path::new(removed_project_root) {
        *runtime_project_root = fallback_path.map(PathBuf::from).unwrap_or_default();
    }
}

pub(super) fn ensure_session_project_root_is_visible(
    project_root: &Path,
    removed_project_roots: &[String],
) -> Result<(), String> {
    let project_root = path_string(project_root);
    if project_root.is_empty()
        || removed_project_roots
            .iter()
            .any(|removed| removed == &project_root)
    {
        return Err("Select a project folder before creating an Agent task".to_string());
    }
    Ok(())
}

pub(super) fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

pub(super) fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests;
