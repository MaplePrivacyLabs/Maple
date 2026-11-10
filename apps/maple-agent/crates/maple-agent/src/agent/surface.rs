//! ACP attachments to host-owned tasks. A lease identifies temporary client
//! bindings; it never grants exclusive execution ownership. Dropping it detaches
//! that client without cancelling shared work. Unprompted drafts stay unlisted.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::config::{
    apply_project_trust, load_agent_config_inner, normalize_project_root, path_string,
    save_agent_config_inner,
};
use super::mcp::{name_to_key, normalize_mcp_servers};
use super::runtime::AgentRuntime;
use super::store::{TaskKind, TaskRow, TaskStore};
use super::{
    AgentCreateSessionRequest, AgentMcpServer, AgentPathLayout, AgentProjectTrustStatus,
    AgentRuntimeHandle, AgentSessionDetail,
};

/// Refused when a surface's lease no longer holds its task: the lease was
/// released, or the task is gone.
pub const AGENT_SURFACE_INACTIVE_ERROR: &str = "Agent surface access is no longer active";
/// The task setting that marks a surface's task before its first prompt.
const PROVISIONAL: &str = "surfaceProvisional";
/// How old a provisional task is before a runtime start removes it.
const STALE_PROVISIONAL_AGE_MS: i64 = 10 * 60 * 1000;

static NEXT_INSTALLATION: AtomicU64 = AtomicU64::new(1);

/// What a surface's lease gives the task it holds.
pub(super) struct SurfaceTask {
    pub(super) installation: u64,
    /// Cancelled once the lease no longer holds the task.
    pub(super) revoked: CancellationToken,
    /// The surface's own MCP servers.
    pub(super) mcp_servers: Vec<AgentMcpServer>,
}

/// Names one lease's hold on its task, for the calls made under it.
#[derive(Clone, Debug)]
pub struct AgentSurfaceAccess {
    session_id: Arc<str>,
    installation: u64,
}

impl AgentSurfaceAccess {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(super) fn installation(&self) -> u64 {
        self.installation
    }
}

/// A surface's hold on one task. Dropping it ends the hold at once and
/// releases the task in the background.
pub struct AgentSurfaceLease {
    runtime: Arc<AgentRuntime>,
    access: AgentSurfaceAccess,
    revoked: CancellationToken,
    /// The surface created the task, which may then be discarded.
    created: bool,
    settled: bool,
}

/// A task a surface holds now, as it is.
pub struct AgentSurfaceSession {
    pub detail: AgentSessionDetail,
    pub lease: AgentSurfaceLease,
}

impl AgentSurfaceLease {
    pub fn access(&self) -> AgentSurfaceAccess {
        self.access.clone()
    }

    /// End the hold at once: nothing is accepted under it once this
    /// returns, before the task is released.
    pub fn revoke(&self) {
        self.revoked.cancel();
    }

    /// Detach this client without stopping the shared task.
    pub async fn release(mut self) {
        self.settled = true;
        self.revoke();
        self.runtime
            .release_surface(&self.access.session_id, self.access.installation)
            .await;
    }

    /// Release the task, and delete it when the surface created it and it
    /// was never prompted or renamed.
    pub async fn discard_created_if_untouched(mut self) {
        self.settled = true;
        self.revoke();
        self.runtime
            .release_surface(&self.access.session_id, self.access.installation)
            .await;
        if self.created {
            self.runtime
                .discard_if_provisional(&self.access.session_id)
                .await;
        }
    }
}

impl Drop for AgentSurfaceLease {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.revoked.cancel();
        let runtime = Arc::clone(&self.runtime);
        let access = self.access.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                runtime
                    .release_surface(&access.session_id, access.installation)
                    .await;
            });
        }
    }
}

/// Merge distinct host and client bindings. Conflicts are unavailable until
/// explicitly reconfigured; neither interface may replace the other's tool.
pub(super) fn with_surface_servers(
    mut task: Vec<AgentMcpServer>,
    surface: Vec<AgentMcpServer>,
) -> Result<Vec<AgentMcpServer>, String> {
    for server in surface {
        if task
            .iter()
            .any(|saved| name_to_key(&saved.name) == name_to_key(&server.name))
        {
            return Err(format!(
                "MCP server '{}' conflicts with another binding",
                server.name
            ));
        }
        task.push(server);
    }
    Ok(task)
}

fn is_provisional(row: &TaskRow) -> bool {
    row.settings
        .get(PROVISIONAL)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn set_provisional(row: &mut TaskRow) {
    if !row.settings.is_object() {
        row.settings = Value::Object(Default::default());
    }
    row.settings[PROVISIONAL] = Value::Bool(true);
}

/// Whether a list shows the task: a surface's task once it was prompted.
pub(super) fn is_listed(row: &TaskRow) -> bool {
    !is_provisional(row)
}

/// Remove the provisional tasks a crash stranded: untouched, and older than
/// any a live surface would still be about to prompt.
pub(super) fn sweep_stale_provisional(store: &TaskStore, _paths: &AgentPathLayout, _user_id: &str) {
    let before = pi_ai::now_ms().saturating_sub(STALE_PROVISIONAL_AGE_MS);
    loop {
        match store
            .store()
            .sweep_provisional_tasks(store.account(), before, 200)
        {
            Ok(200) => continue,
            Ok(_) => return,
            Err(error) => {
                log::warn!("Failed to collect stranded ACP drafts: {error}");
                return;
            }
        }
    }
}

impl AgentRuntime {
    /// A lease on the task, installed.
    async fn lease(
        self: &Arc<Self>,
        session_id: &str,
        mcp_servers: Vec<AgentMcpServer>,
        created: bool,
    ) -> Result<AgentSurfaceLease, String> {
        let revoked = CancellationToken::new();
        let installation = NEXT_INSTALLATION.fetch_add(1, Ordering::Relaxed);
        self.install_surface(
            session_id,
            SurfaceTask {
                installation,
                revoked: revoked.clone(),
                mcp_servers,
            },
        )
        .await?;
        Ok(AgentSurfaceLease {
            runtime: Arc::clone(self),
            access: AgentSurfaceAccess {
                session_id: Arc::from(session_id),
                installation,
            },
            revoked,
            created,
            settled: false,
        })
    }

    /// The task was prompted: lists show it from now on.
    pub(super) fn settle_provisional(&self, session_id: &str) {
        let settled = self.store.update(session_id, |row| {
            if let Some(settings) = row.settings.as_object_mut() {
                settings.remove(PROVISIONAL);
            }
        });
        if let Err(error) = settled {
            log::warn!("Failed to record the ACP task's first prompt: {error}");
        }
    }

    /// Delete a surface's task that nothing touched.
    async fn discard_if_provisional(self: &Arc<Self>, session_id: &str) {
        let runtime = Arc::clone(self);
        let session_id = session_id.to_owned();
        let result = tokio::task::spawn_blocking(move || {
            runtime.maintenance_while_idle(&session_id, || {
                if runtime.has_unpersisted_session(&session_id)? {
                    return Err(
                        "Draft has pending source history; retaining its loaded context".into(),
                    );
                }
                // The exact untouched predicate is rechecked by the tombstone
                // transaction, so a metadata edit after an earlier read cannot
                // be hidden. Run/launch admission stays held through the hide.
                if runtime.store.discard_provisional(&session_id)? {
                    runtime.failures.clear(&session_id);
                }
                Ok(())
            })
        })
        .await;
        if let Err(error) = result.unwrap_or_else(|_| Err("Draft cleanup was interrupted".into())) {
            log::debug!("Unused ACP task cleanup deferred: {error}");
        }
    }
}

impl AgentRuntimeHandle {
    /// Create a task for a calling surface and hold it: an ACP task in the
    /// requested project, provisional until its first prompt, with
    /// `mcp_servers` beside the account's.
    pub async fn create_surface_session(
        &self,
        request: AgentCreateSessionRequest,
        mcp_servers: Vec<AgentMcpServer>,
    ) -> Result<AgentSurfaceSession, String> {
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        let mcp_servers = normalize_mcp_servers(mcp_servers)?;
        let mut row = self.new_task_row(&runtime, request, TaskKind::Acp).await?;
        set_provisional(&mut row);
        runtime.store.insert(&row)?;
        let lease = match runtime.lease(&row.id, mcp_servers, true).await {
            Ok(lease) => lease,
            Err(error) => {
                runtime.discard_if_provisional(&row.id).await;
                return Err(error);
            }
        };
        Ok(AgentSurfaceSession {
            detail: AgentSessionDetail {
                timeline_revision: 0,
                session: row.summary(),
                timeline: Vec::new(),
                mcp_errors: Vec::new(),
                queue: runtime.runs.queue_snapshot(&row.id),
            },
            lease,
        })
    }

    /// Hold an existing task for a calling surface, as `create_surface_session`
    /// holds a new one, and read it.
    pub async fn attach_surface_session(
        &self,
        session_id: &str,
        mcp_servers: Vec<AgentMcpServer>,
    ) -> Result<AgentSurfaceSession, String> {
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        let mcp_servers = normalize_mcp_servers(mcp_servers)?;
        if runtime.store.get(session_id)?.is_none() {
            return Err(format!("Failed to find Agent task {session_id}"));
        }
        let lease = runtime.lease(session_id, mcp_servers, false).await?;
        let detail = self.load_session(session_id.to_string()).await?;
        Ok(AgentSurfaceSession { detail, lease })
    }

    /// Acquire the same surface lease while leaving persisted history in the
    /// paged store. The detail carries only the currently live overlay; a caller
    /// must stream committed history through history_forward_page separately.
    pub async fn attach_surface_session_paged(
        &self,
        session_id: &str,
        mcp_servers: Vec<AgentMcpServer>,
    ) -> Result<AgentSurfaceSession, String> {
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        self.task_summary(session_id.to_string()).await?;
        let mcp_servers = normalize_mcp_servers(mcp_servers)?;
        let lease = runtime.lease(session_id, mcp_servers, false).await?;
        let summary = self.task_summary(session_id.to_string()).await?;
        let live = self.live_task_page(session_id, 500, 512 * 1024).await?;
        if live.has_more {
            return Err(
                "The active task is too large to replay completely; reload after its turn ends"
                    .into(),
            );
        }
        let timeline_revision = live.timeline_revision;
        let timeline = live.items;
        let state = match summary.state.as_str() {
            "active" => super::AgentTaskState::Active,
            "settled" => super::AgentTaskState::Settled,
            "archived" => super::AgentTaskState::Archived,
            _ => return Err("Invalid task state".to_string()),
        };
        Ok(AgentSurfaceSession {
            detail: AgentSessionDetail {
                timeline_revision,
                session: super::AgentSessionSummary {
                    id: summary.uuid,
                    title: summary.title,
                    project_root: summary.project_root,
                    created_ms: summary.created_ms,
                    updated_ms: summary.updated_ms,
                    message_count: usize::try_from(summary.message_count)
                        .map_err(|_| "Invalid task message count")?,
                    model: summary.model,
                    web_enabled: summary.web_enabled,

                    permission_mode: super::AgentPermissionMode::from_saved(
                        summary.permission_mode.as_deref(),
                    ),
                    state,
                    acp: summary.kind == "acp",
                },
                timeline,
                mcp_errors: Vec::new(),
                queue: runtime.runs.queue_snapshot(session_id),
            },
            lease,
        })
    }

    /// Save the account's trust decision for the project of a task a surface
    /// holds, as the surface asked its user. The task's session is built
    /// again with it.
    pub async fn set_project_trust_for_surface(
        &self,
        access: &AgentSurfaceAccess,
        path: String,
        trusted: bool,
    ) -> Result<AgentProjectTrustStatus, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        let session_id = access.session_id();
        if !runtime.surface_holds(access) {
            return Err(AGENT_SURFACE_INACTIVE_ERROR.to_string());
        }
        if runtime.runs.is_running(session_id) {
            return Err("Project trust cannot change while this Agent task is running".to_string());
        }
        let project_root = normalize_project_root(Path::new(&path))?;
        let row = runtime
            .store
            .get(session_id)?
            .ok_or_else(|| AGENT_SURFACE_INACTIVE_ERROR.to_string())?;
        if row.project_root != path_string(&project_root) {
            return Err("Project trust path does not match this Agent task".to_string());
        }
        let config = {
            let _settings = self.lock_settings().await;
            let mut config =
                load_agent_config_inner(self.paths(), &self.user_id).map_err(|e| e.to_string())?;
            apply_project_trust(&mut config, &project_root, trusted);
            save_agent_config_inner(self.paths(), &self.user_id, &config)
                .map_err(|e| e.to_string())?;
            config
        };
        if !runtime.runs.is_running(session_id) {
            runtime.unload_task(session_id).await?;
        }
        self.project_trust_status(&config, &project_root)
    }
}

#[cfg(test)]
mod tests;
