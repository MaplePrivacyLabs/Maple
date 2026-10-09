//! Maple's own account files and the project list.
//!
//! The account `config.json` (default project and model, MCP servers, project
//! trust), the device-local list of removed projects, the recent project
//! roots, the default Maple workspace, and the paths of the account folders.
//! None of it depends on how tasks run, so it carried over from the Goose
//! runtime unchanged.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use super::attachments::AgentAttachmentStore;
use super::types::*;
use super::{AgentPathLayout, LEGACY_AGENT_DEFAULT_MODEL, PREVIOUS_RECOMMENDED_AGENT_MODEL};
use crate::maple_api::account_scope;

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

    // A removed project persists only as a device-local tombstone; the
    // roaming default may still name it. Never boot into a hidden root.
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

pub(super) fn agent_root_dir(paths: &AgentPathLayout) -> Result<PathBuf, anyhow::Error> {
    let path = paths.config_root.clone();
    fs::create_dir_all(&path)?;
    set_owner_only_dir_permissions(&path);
    Ok(path)
}

pub(super) fn account_config_dir_path(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<PathBuf, anyhow::Error> {
    let scope = account_scope(user_id).map_err(anyhow::Error::msg)?;
    Ok(agent_root_dir(paths)?.join("accounts").join(scope))
}

pub(super) fn account_local_data_dir_path(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<PathBuf, anyhow::Error> {
    let scope = account_scope(user_id).map_err(anyhow::Error::msg)?;
    Ok(paths.local_data_root.join("accounts").join(scope))
}

/// The device-local data directory this crate owns for one account: task
/// sessions and their index, image attachments, and anything else keyed to
/// the signed-in user. Removing it removes the account's local state.
///
/// Pure path arithmetic; it creates nothing and touches no disk. Callers
/// outside this crate should use it instead of rebuilding the layout by
/// hand, because the on-disk shape is this crate's private business.
pub fn account_local_data_dir(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, String> {
    account_local_data_dir_path(paths, user_id).map_err(|error| error.to_string())
}

/// The store of model-written tool call summaries for one account.
///
/// The app owns this file; the agent runtime never opens it. It lives
/// beside the runtime's own account data so that deleting the account
/// removes the summaries with it.
pub fn account_tool_summaries_db_path(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<PathBuf, String> {
    Ok(account_local_data_dir(paths, user_id)?.join(AGENT_TOOL_SUMMARIES_DB_NAME))
}

pub(super) fn account_attachment_store(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<AgentAttachmentStore, String> {
    account_local_data_dir_path(paths, user_id)
        .map(AgentAttachmentStore::new)
        .map_err(|error| error.to_string())
}

pub(super) fn agent_config_dir(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<PathBuf, anyhow::Error> {
    let path = account_config_dir_path(paths, user_id)?;
    fs::create_dir_all(&path)?;
    set_owner_only_dir_permissions(&path);
    Ok(path)
}

/// Folder of the account's task session files, below its local data folder.
const AGENT_SESSIONS_SUBDIR: &str = "sessions";

/// File name of the account's task index, beside the session files.
pub(super) const AGENT_TASK_INDEX_NAME: &str = "tasks.db";

/// File name of the app-owned tool call summary store below an account
/// directory. See [`account_tool_summaries_db_path`].
const AGENT_TOOL_SUMMARIES_DB_NAME: &str = "tool_summaries.db";

/// The folder of one account's task session files, created owner-only.
///
/// Session history is device-local, so it lives in the local data root, next
/// to the attachments. The account `config.json` stays in the config root,
/// which a user may sync between machines.
pub(super) fn account_sessions_dir(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<PathBuf, String> {
    let account_dir =
        account_local_data_dir_path(paths, user_id).map_err(|error| error.to_string())?;
    sessions_dir_for_account_dir(&account_dir)
}

pub(super) fn sessions_dir_for_account_dir(account_dir: &Path) -> Result<PathBuf, String> {
    let sessions = account_dir.join(AGENT_SESSIONS_SUBDIR);
    create_owner_only_dir_all(&sessions)
        .map_err(|error| format!("Failed to create the task folder: {error}"))?;
    Ok(sessions)
}

/// Create `path` and its missing parents, making each folder this call
/// creates, and `path` itself, readable only by the owner.
pub(super) fn create_owner_only_dir_all(path: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut current = Some(path);
    while let Some(directory) = current {
        if directory.exists() {
            break;
        }
        missing.push(directory.to_path_buf());
        current = directory.parent();
    }
    fs::create_dir_all(path)?;
    for directory in missing.iter().rev() {
        set_owner_only_dir_permissions(directory);
    }
    set_owner_only_dir_permissions(path);
    Ok(())
}

pub(super) fn load_agent_config_inner(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<AgentConfig, anyhow::Error> {
    let path = agent_config_dir(paths, user_id)?.join("config.json");
    let removed_project_roots_path =
        account_local_data_dir_path(paths, user_id)?.join("removed_project_roots.json");
    load_agent_config_files(&path, &removed_project_roots_path)
}

pub(super) fn load_agent_config_files(
    config_path: &Path,
    removed_project_roots_path: &Path,
) -> Result<AgentConfig, anyhow::Error> {
    let had_legacy_project_skills_trust = config_file_uses_legacy_project_skills_trust(config_path);
    let mut config = load_agent_config_file(config_path)?;
    // This field was introduced by the unshipped remove-project work. Never
    // adopt it from the roaming config: on Windows it may have come from a
    // different device using the same roaming profile.
    let had_roaming_removed_project_roots = !config.removed_project_roots.is_empty();
    let migrated = migrate_agent_config(&mut config);
    config.removed_project_roots = load_removed_project_roots_file(removed_project_roots_path)?;
    if migrated || had_roaming_removed_project_roots || had_legacy_project_skills_trust {
        save_agent_config_file(config_path, &config)?;
    }
    Ok(config)
}

pub(super) fn config_file_uses_legacy_project_skills_trust(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
        .is_some_and(|config| config.get("projectSkillsTrust").is_some())
}

pub(super) fn load_agent_config_file(path: &Path) -> Result<AgentConfig, anyhow::Error> {
    if !path.exists() {
        return Ok(AgentConfig::default());
    }
    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&contents)?)
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
    let path = agent_config_dir(paths, user_id)?.join("config.json");
    save_agent_config_file(&path, config)
}

pub(super) fn save_agent_config_file(
    path: &Path,
    config: &AgentConfig,
) -> Result<(), anyhow::Error> {
    let mut roaming_config = config.clone();
    roaming_config.removed_project_roots.clear();
    write_json_file(path, &roaming_config)
}

pub(super) fn load_removed_project_roots_file(path: &Path) -> Result<Vec<String>, anyhow::Error> {
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)?;
    let roots = serde_json::from_str::<Vec<String>>(&contents)?;
    let sanitized = sanitize_project_root_paths(roots.clone());
    if sanitized != roots {
        write_device_local_json_file(path, &sanitized)?;
    }
    Ok(sanitized)
}

pub(super) fn save_removed_project_roots_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    roots: &[String],
) -> Result<(), anyhow::Error> {
    let path = account_local_data_dir_path(paths, user_id)?.join("removed_project_roots.json");
    write_device_local_json_file(&path, roots)
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

/// Path of the default workspace, without creating it.
///
/// Resolved once per process: the UI labels project roots while it
/// renders, so recognizing the workspace must not touch the disk. The
/// documents folder is canonicalized the way every project root is, so a
/// plain comparison matches the root the runtime stores for it.
pub fn default_maple_workspace_path() -> Option<PathBuf> {
    static WORKSPACE: once_cell::sync::OnceCell<Option<PathBuf>> = once_cell::sync::OnceCell::new();
    WORKSPACE
        .get_or_init(|| {
            let documents = document_dir()?;
            let documents = canonical_dir(&documents).unwrap_or(documents);
            Some(maple_workspace_directory(&documents))
        })
        .clone()
}

/// Create the default workspace when it is missing and return its canonical
/// path. Call this only when the workspace is about to be used (a runtime
/// start that picks it, or the user choosing or opening it), never at
/// launch: someone working elsewhere is not asked for Documents access,
/// and a folder the user deleted stays deleted until it is used again.
pub fn ensure_default_maple_workspace() -> Result<PathBuf, String> {
    let path = default_maple_workspace_path()
        .ok_or_else(|| "No documents folder for the Maple workspace".to_string())?;
    create_directory(&path)
}

pub(super) fn create_directory(path: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Cannot create {}: {error}", path.display()))?;
    canonical_dir(path).ok_or_else(|| format!("{} is not a folder", path.display()))
}

/// Whether `path` is the default Documents/Maple workspace. Project roots
/// are canonical, so this is a comparison with no disk access.
pub fn is_default_maple_workspace(path: &Path) -> bool {
    default_maple_workspace_path().is_some_and(|workspace| workspace == path)
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
/// recent-root file still names it. Listing never creates the folder, and
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
    });
    roots
}

/// Home, the directory the process was started in, and the default Maple
/// workspace are trusted until the user records a different answer. The
/// filesystem root is never implicit: a GUI launched from the Dock often
/// has cwd `/`.
pub(super) fn is_implicitly_trusted_project_root(project_root: &Path) -> bool {
    implicitly_trusted_project_root(
        project_root,
        home_dir().and_then(|path| canonical_dir(&path)).as_deref(),
        launch_dir()
            .filter(|path| !is_filesystem_root(path))
            .as_deref(),
        default_maple_workspace_path().as_deref(),
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
    protected_features: Vec<AgentProjectTrustFeature>,
) -> AgentProjectTrustStatus {
    AgentProjectTrustStatus {
        path: path_string(project_root),
        decision: project_trust_decision(config, project_root),
        available: true,
        protected_features,
    }
}

/// The decision saved for `project_root`, else the implicit trust of home,
/// the launch folder and the Maple workspace.
pub(super) fn project_trust_decision(config: &AgentConfig, project_root: &Path) -> Option<bool> {
    let path = path_string(project_root);
    config
        .project_trust
        .iter()
        .find(|entry| entry.path == path)
        .map(|entry| entry.trusted)
        .or_else(|| is_implicitly_trusted_project_root(project_root).then_some(true))
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

pub(super) fn load_recent_project_roots_inner(
    paths: &AgentPathLayout,
    user_id: &str,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let path = agent_config_dir(paths, user_id)?.join("recent_roots.json");
    load_recent_project_roots_file(&path)
}

pub(super) fn read_recent_project_roots_file(
    path: &Path,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&contents)?)
}

pub(super) fn load_recent_project_roots_file(
    path: &Path,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    Ok(sanitize_recent_project_roots(
        read_recent_project_roots_file(path)?,
    ))
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
    }
}

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

pub(super) fn register_explicit_project_root_file(
    file_path: &Path,
    project_root: &Path,
    last_used_ms: u128,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let roots = read_recent_project_roots_file(file_path)?;
    let (roots, changed) = register_explicit_project_root(roots, project_root, last_used_ms);
    if changed {
        write_json_file(file_path, &roots)?;
    }
    Ok(roots)
}

pub(super) fn register_explicit_project_root_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    project_root: &Path,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let file_path = agent_config_dir(paths, user_id)?.join("recent_roots.json");
    register_explicit_project_root_file(&file_path, project_root, unix_ms())
}

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

pub(super) fn restore_explicit_project_root_file(
    file_path: &Path,
    project_root: &Path,
    last_used_ms: u128,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let roots = restore_explicit_project_root(
        read_recent_project_roots_file(file_path)?,
        project_root,
        last_used_ms,
    );
    write_json_file(file_path, &roots)?;
    Ok(roots)
}

pub(super) fn restore_explicit_project_root_inner(
    paths: &AgentPathLayout,
    user_id: &str,
    project_root: &Path,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let file_path = agent_config_dir(paths, user_id)?.join("recent_roots.json");
    restore_explicit_project_root_file(&file_path, project_root, unix_ms())
}

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

pub(super) fn save_project_root_order_file(
    file_path: &Path,
    paths: Vec<String>,
    last_used_ms: u128,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let roots = read_recent_project_roots_file(file_path)?;
    let roots = apply_project_root_order(roots, paths, last_used_ms).map_err(anyhow::Error::msg)?;
    write_json_file(file_path, &roots)?;
    Ok(roots)
}

pub(super) fn save_project_root_order_inner(
    layout: &AgentPathLayout,
    user_id: &str,
    mut paths: Vec<String>,
) -> Result<Vec<RecentProjectRoot>, anyhow::Error> {
    let file_path = agent_config_dir(layout, user_id)?.join("recent_roots.json");
    let removed = load_agent_config_inner(layout, user_id)?
        .removed_project_roots
        .into_iter()
        .collect::<HashSet<_>>();
    let requested = paths.iter().cloned().collect::<HashSet<_>>();
    paths.extend(
        read_recent_project_roots_file(&file_path)?
            .into_iter()
            .filter(|root| removed.contains(&root.path) && !requested.contains(&root.path))
            .map(|root| root.path),
    );
    save_project_root_order_file(&file_path, paths, unix_ms())
}

pub(super) fn project_has_active_session_run(
    session_roots: &HashMap<String, String>,
    active_session_ids: &HashSet<String>,
    project_root: &str,
) -> bool {
    active_session_ids.iter().any(|session_id| {
        session_roots
            .get(session_id)
            .is_some_and(|root| root == project_root)
    })
}

pub(super) fn apply_project_root_removal(
    config: &mut AgentConfig,
    project_root: &str,
    fallback_path: Option<&str>,
) -> Result<(), String> {
    if fallback_path.is_some_and(|fallback| {
        config
            .removed_project_roots
            .iter()
            .any(|removed| removed == fallback)
    }) {
        return Err("Project fallback is already removed".to_string());
    }
    if !config
        .removed_project_roots
        .iter()
        .any(|removed| removed == project_root)
    {
        config.removed_project_roots.push(project_root.to_string());
    }
    if config.default_project_root.as_deref() == Some(project_root) {
        config.default_project_root = fallback_path.map(ToOwned::to_owned);
    }
    Ok(())
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

pub(super) fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<(), anyhow::Error> {
    crate::private_file::write_private_json(path, value)?;
    Ok(())
}

pub(super) fn write_device_local_json_file<T: Serialize + ?Sized>(
    path: &Path,
    value: &T,
) -> Result<(), anyhow::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Device-local Agent data path has no parent"))?;
    fs::create_dir_all(parent)?;
    set_owner_only_dir_permissions(parent);
    crate::private_file::write_private_json(path, value)?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn set_owner_only_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub(super) fn set_owner_only_permissions(_path: &Path) {}

#[cfg(unix)]
pub(super) fn set_owner_only_dir_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
pub(super) fn set_owner_only_dir_permissions(_path: &Path) {}

pub(super) fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

pub(super) fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}
