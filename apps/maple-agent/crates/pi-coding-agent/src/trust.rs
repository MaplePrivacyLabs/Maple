//! Project trust, after Pi's: whether a folder's own resources may be loaded.
//!
//! A project can carry instructions for the model: skills and prompt templates in its
//! project folder (`.maple`) or in `.agents/skills`, `SYSTEM.md` and
//! `APPEND_SYSTEM.md`, and settings. Opening a folder someone else wrote should not
//! hand it the agent, so the user decides once whether to trust it. Decisions are kept
//! in `trust.json` in the user's agent folder, keyed by folder; a folder inherits the
//! nearest decision above it. Hosts resolve trust before loading resources and pass
//! the answer to [`Resources::load`](crate::resources::Resources::load), and load no
//! project settings for an untrusted folder.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::extensions::ExtensionUi;
use crate::resources::ResourcePaths;

/// What to do with a folder that has project resources and no decision yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultProjectTrust {
    /// Ask the user, or do not trust when there is no one to ask.
    #[default]
    Ask,
    Always,
    Never,
}

/// The project folder entries that need trust before they are loaded.
const TRUST_REQUIRING_PROJECT_RESOURCES: [&str; 8] = [
    "settings.json",
    "mcp.json",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
];

/// `path` made absolute and resolved, as decisions are keyed. A path that does not
/// exist is kept as given.
fn normalize(path: &Path) -> PathBuf {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // Windows canonical paths start with \\?\, which no one types; drop it.
    #[cfg(windows)]
    if let Some(rest) = resolved
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        && !rest.starts_with("UNC\\")
    {
        return PathBuf::from(rest);
    }
    resolved
}

/// Whether `cwd` has project resources that wait for trust: one of the trust-requiring
/// entries in its project folder, or `.agents/skills` in it or a folder above it. The
/// user's own `~/.agents/skills` does not count, even when `cwd` is the home folder.
pub fn has_trust_requiring_project_resources(cwd: &Path, paths: &ResourcePaths) -> bool {
    let cwd = normalize(cwd);
    let project_dir = paths.project_dir(&cwd);
    if TRUST_REQUIRING_PROJECT_RESOURCES
        .iter()
        .any(|entry| project_dir.join(entry).exists())
    {
        return true;
    }
    let user_skills = paths
        .home_dir
        .as_deref()
        .map(|home| normalize(home).join(".agents").join("skills"));
    cwd.ancestors().any(|dir| {
        let skills = dir.join(".agents").join("skills");
        Some(&skills) != user_skills.as_ref() && skills.exists()
    })
}

/// A remembered decision and the folder it was made for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTrustEntry {
    pub path: PathBuf,
    pub decision: bool,
}

/// A change to the remembered decisions; `None` forgets the folder's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTrustUpdate {
    pub path: PathBuf,
    pub decision: Option<bool>,
}

/// One answer the user can give when asked whether to trust a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTrustOption {
    pub label: String,
    pub trusted: bool,
    /// The decisions it remembers; none for an answer for this session only.
    pub updates: Vec<ProjectTrustUpdate>,
    /// The folder whose decision it remembers.
    pub saved_path: Option<PathBuf>,
}

/// The answers to offer for `cwd`: trust it, trust the folder above it, or do not trust
/// it, each remembered, and with `include_session_only` the same for this session only.
pub fn project_trust_options(cwd: &Path, include_session_only: bool) -> Vec<ProjectTrustOption> {
    let path = normalize(cwd);
    let mut options = vec![ProjectTrustOption {
        label: "Trust".to_string(),
        trusted: true,
        updates: vec![ProjectTrustUpdate {
            path: path.clone(),
            decision: Some(true),
        }],
        saved_path: Some(path.clone()),
    }];
    if let Some(parent) = path.parent() {
        options.push(ProjectTrustOption {
            label: format!("Trust parent folder ({})", parent.display()),
            trusted: true,
            updates: vec![
                ProjectTrustUpdate {
                    path: parent.to_path_buf(),
                    decision: Some(true),
                },
                ProjectTrustUpdate {
                    path: path.clone(),
                    decision: None,
                },
            ],
            saved_path: Some(parent.to_path_buf()),
        });
    }
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Trust (this session only)".to_string(),
            trusted: true,
            updates: Vec::new(),
            saved_path: None,
        });
    }
    options.push(ProjectTrustOption {
        label: "Do not trust".to_string(),
        trusted: false,
        updates: vec![ProjectTrustUpdate {
            path: path.clone(),
            decision: Some(false),
        }],
        saved_path: Some(path),
    });
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Do not trust (this session only)".to_string(),
            trusted: false,
            updates: Vec::new(),
            saved_path: None,
        });
    }
    options
}

/// The question the user is asked about `cwd`.
pub fn project_trust_prompt(app_name: &str, paths: &ResourcePaths, cwd: &Path) -> String {
    format!(
        "Trust project folder?\n{}\n\nThis allows {app_name} to load {} settings and resources.",
        cwd.display(),
        paths.project_dir_name
    )
}

type TrustFile = BTreeMap<String, Option<bool>>;

/// Decisions remembered in `trust.json` in the user's agent folder. Reads and writes
/// hold a lock on `trust.json.lock`, so processes sharing the folder take turns.
#[derive(Clone, Debug)]
pub struct ProjectTrustStore {
    path: PathBuf,
}

impl ProjectTrustStore {
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            path: agent_dir.join("trust.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The decision for `cwd`, from it or the nearest folder above it.
    pub fn get(&self, cwd: &Path) -> io::Result<Option<bool>> {
        Ok(self.get_entry(cwd)?.map(|entry| entry.decision))
    }

    pub fn get_entry(&self, cwd: &Path) -> io::Result<Option<ProjectTrustEntry>> {
        self.locked(|| {
            let data = self.read()?;
            Ok(normalize(cwd).ancestors().find_map(|dir| {
                let decision = data
                    .get(dir.to_string_lossy().as_ref())
                    .copied()
                    .flatten()?;
                Some(ProjectTrustEntry {
                    path: dir.to_path_buf(),
                    decision,
                })
            }))
        })
    }

    pub fn set(&self, cwd: &Path, decision: Option<bool>) -> io::Result<()> {
        self.set_many(&[ProjectTrustUpdate {
            path: cwd.to_path_buf(),
            decision,
        }])
    }

    pub fn set_many(&self, updates: &[ProjectTrustUpdate]) -> io::Result<()> {
        self.locked(|| {
            let mut data = self.read()?;
            for update in updates {
                let key = normalize(&update.path).to_string_lossy().into_owned();
                match update.decision {
                    Some(decision) => {
                        data.insert(key, Some(decision));
                    }
                    None => {
                        data.remove(&key);
                    }
                }
            }
            data.retain(|_, decision| decision.is_some());
            let mut text = serde_json::to_string_pretty(&data).map_err(io::Error::other)?;
            text.push('\n');
            // Written beside it and moved over it, so a reader never sees half a file.
            let partial = self.path.with_extension("json.partial");
            fs::write(&partial, text)?;
            fs::rename(&partial, &self.path)
        })
    }

    fn read(&self) -> io::Result<TrustFile> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(TrustFile::new()),
            Err(error) => return Err(error),
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        serde_json::from_str::<TrustFile>(text).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Invalid trust store {}: values must be true, false or null ({error})",
                    self.path.display()
                ),
            )
        })
    }

    fn locked<T>(&self, work: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let lock: File = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension("json.lock"))?;
        lock.lock()?;
        let result = work();
        // Closing the file releases the lock too; unlocking first says so.
        let _ = lock.unlock();
        result
    }
}

/// How a host decides whether to trust a folder.
pub struct ProjectTrustRequest<'a> {
    pub cwd: &'a Path,
    pub paths: &'a ResourcePaths,
    pub store: &'a ProjectTrustStore,
    /// A decision made elsewhere, such as on the command line, which wins.
    pub trust_override: Option<bool>,
    pub default_trust: DefaultProjectTrust,
    /// The product name the question uses.
    pub app_name: &'a str,
}

/// Whether to load `cwd`'s project resources, as Pi decides it: an override wins; a
/// folder without project resources is trusted; then the remembered decision for it or
/// a folder above it; then the default, where `Ask` asks through `ui` and remembers the
/// answer. Without an interface to ask, or without an answer, the folder is not trusted.
pub async fn resolve_project_trusted(
    request: ProjectTrustRequest<'_>,
    ui: &dyn ExtensionUi,
) -> io::Result<bool> {
    if let Some(trusted) = request.trust_override {
        return Ok(trusted);
    }
    if !has_trust_requiring_project_resources(request.cwd, request.paths) {
        return Ok(true);
    }
    if let Some(decision) = request.store.get(request.cwd)? {
        return Ok(decision);
    }
    match request.default_trust {
        DefaultProjectTrust::Always => return Ok(true),
        DefaultProjectTrust::Never => return Ok(false),
        DefaultProjectTrust::Ask => {}
    }
    if !ui.has_ui() {
        return Ok(false);
    }
    let options = project_trust_options(request.cwd, true);
    let labels: Vec<String> = options.iter().map(|option| option.label.clone()).collect();
    let question = project_trust_prompt(request.app_name, request.paths, request.cwd);
    let Some(chosen) = ui
        .select(&question, &labels)
        .await
        .and_then(|index| options.get(index))
    else {
        return Ok(false);
    };
    if !chosen.updates.is_empty() {
        request.store.set_many(&chosen.updates)?;
    }
    Ok(chosen.trusted)
}

#[cfg(test)]
mod tests;
