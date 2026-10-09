//! Tasks a calling surface drives, such as an ACP client's sessions.
//!
//! A surface holds each task it drives with a lease. While the lease lasts,
//! the task's tools run with the surface's tool context (the bridge's
//! variables), its runs are the surface's alone, and the desktop's own tools
//! stay out of it: the plan and questions, external agents and computer use.
//! The surface's own MCP servers run beside the task's. Releasing the lease
//! revokes the context and unloads the task's session, so whoever drives the
//! task next builds it afresh.
//!
//! A task a surface creates is provisional until its first prompt: no list
//! shows it, and releasing the lease of an untouched one discards it. A crash
//! can strand one; the next runtime start removes those older than ten
//! minutes.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use super::config::{
    account_attachment_store, apply_project_trust, load_agent_config_inner, normalize_project_root,
    path_string, save_agent_config_inner,
};
use super::mcp::{name_to_key, normalize_mcp_servers};
use super::runtime::AgentRuntime;
use super::store::{TaskKind, TaskRow, TaskStore};
use super::tool_context::SharedAgentToolContext;
use super::{
    AgentCreateSessionRequest, AgentMcpServer, AgentPathLayout, AgentProjectTrustStatus,
    AgentRuntimeHandle, AgentSessionDetail, AgentToolContextSpec,
};

/// Refused when a surface's lease no longer holds its task: the lease was
/// released, or the task is gone.
pub const AGENT_SURFACE_INACTIVE_ERROR: &str = "Agent tool context access is no longer active";
/// Refused to anyone else while a surface holds the task.
pub(super) const SURFACE_CONTROLLED_ERROR: &str =
    "This Agent task is controlled by another Agent surface";
/// The task setting that marks a surface's task before its first prompt.
const PROVISIONAL: &str = "surfaceProvisional";
/// How old a provisional task is before a runtime start removes it.
const STALE_PROVISIONAL_AGE_MS: i64 = 10 * 60 * 1000;

static NEXT_INSTALLATION: AtomicU64 = AtomicU64::new(1);

/// What a surface's lease gives the task it holds.
pub(super) struct SurfaceTask {
    pub(super) installation: u64,
    pub(super) context: SharedAgentToolContext,
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

/// A surface's hold on one task. Dropping it revokes the tool context at
/// once and releases the task in the background.
pub struct AgentSurfaceLease {
    runtime: Arc<AgentRuntime>,
    access: AgentSurfaceAccess,
    context: SharedAgentToolContext,
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

    /// Revoke the tool context: a command the task starts once this returns
    /// gets none of the surface's variables.
    pub fn revoke(&self) {
        self.context.revoke();
    }

    /// Release the task: revoke the context, stop the task's run and unload
    /// its session.
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
        self.context.revoke();
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

/// The servers a task runs: its own, and the surface's in place of one of
/// its own with the same name.
pub(super) fn with_surface_servers(
    task: Vec<AgentMcpServer>,
    surface: Vec<AgentMcpServer>,
) -> Vec<AgentMcpServer> {
    let mut servers: Vec<AgentMcpServer> = task
        .into_iter()
        .filter(|server| {
            let key = name_to_key(&server.name);
            !surface
                .iter()
                .any(|surface| name_to_key(&surface.name) == key)
        })
        .collect();
    servers.extend(surface);
    servers
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

/// A provisional task that nothing touched: not prompted, not renamed.
fn is_untouched(row: &TaskRow) -> bool {
    is_provisional(row) && !row.title_user_set && row.message_count == 0
}

/// Delete a task and its attachments.
fn delete_task(
    store: &TaskStore,
    paths: &AgentPathLayout,
    user_id: &str,
    session_id: &str,
) -> Result<(), String> {
    let attachments = account_attachment_store(paths, user_id)?;
    store.delete(session_id, || attachments.delete_session(session_id))?;
    Ok(())
}

/// Remove the provisional tasks a crash stranded: untouched, and older than
/// any a live surface would still be about to prompt.
pub(super) fn sweep_stale_provisional(store: &TaskStore, paths: &AgentPathLayout, user_id: &str) {
    let rows = match store.list(None) {
        Ok(rows) => rows,
        Err(error) => {
            log::warn!("Failed to look for stranded ACP tasks: {error}");
            return;
        }
    };
    let now = pi_ai::now_ms();
    for row in rows {
        if is_untouched(&row)
            && now.saturating_sub(row.created_ms) >= STALE_PROVISIONAL_AGE_MS
            && let Err(error) = delete_task(store, paths, user_id, &row.id)
        {
            log::warn!("Failed to remove the stranded ACP task {}: {error}", row.id);
        }
    }
}

impl AgentRuntime {
    /// A lease on the task, installed.
    async fn lease(
        self: &Arc<Self>,
        session_id: &str,
        tool_context: AgentToolContextSpec,
        mcp_servers: Vec<AgentMcpServer>,
        created: bool,
    ) -> Result<AgentSurfaceLease, String> {
        let context = SharedAgentToolContext::new(tool_context);
        let installation = NEXT_INSTALLATION.fetch_add(1, Ordering::Relaxed);
        self.install_surface(
            session_id,
            SurfaceTask {
                installation,
                context: context.clone(),
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
            context,
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
    async fn discard_if_provisional(&self, session_id: &str) {
        if self.runs.is_running(session_id) {
            return;
        }
        match self.store.get(session_id) {
            Ok(Some(row)) if is_untouched(&row) => {}
            _ => return,
        }
        self.failures.clear(session_id);
        if let Err(error) = delete_task(&self.store, &self.host.paths, &self.user_id, session_id) {
            log::warn!("Failed to discard the unused ACP task {session_id}: {error}");
        }
    }
}

impl AgentRuntimeHandle {
    /// Create a task for a calling surface and hold it: an ACP task in the
    /// requested project, provisional until its first prompt. Its tools run
    /// with `tool_context`, and `mcp_servers` run beside the account's.
    pub async fn create_surface_session(
        &self,
        request: AgentCreateSessionRequest,
        tool_context: AgentToolContextSpec,
        mcp_servers: Vec<AgentMcpServer>,
    ) -> Result<AgentSurfaceSession, String> {
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        let mcp_servers = normalize_mcp_servers(mcp_servers)?;
        let system_prompt = request.system_prompt.clone();
        let mut row = self.new_task_row(&runtime, request, TaskKind::Acp).await?;
        set_provisional(&mut row);
        runtime.store.insert(&row)?;
        runtime.set_system_prompt(&row.id, system_prompt);
        let lease = match runtime
            .lease(&row.id, tool_context, mcp_servers, true)
            .await
        {
            Ok(lease) => lease,
            Err(error) => {
                runtime.discard_if_provisional(&row.id).await;
                return Err(error);
            }
        };
        Ok(AgentSurfaceSession {
            detail: AgentSessionDetail {
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
        tool_context: AgentToolContextSpec,
        mcp_servers: Vec<AgentMcpServer>,
    ) -> Result<AgentSurfaceSession, String> {
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        let mcp_servers = normalize_mcp_servers(mcp_servers)?;
        if runtime.store.get(session_id)?.is_none() {
            return Err(format!("Failed to find Agent task {session_id}"));
        }
        let lease = runtime
            .lease(session_id, tool_context, mcp_servers, false)
            .await?;
        let detail = self.load_session(session_id.to_string()).await?;
        Ok(AgentSurfaceSession { detail, lease })
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
            runtime.unload_task(session_id).await;
        }
        Ok(self.project_trust_status(&config, &project_root))
    }
}

#[cfg(test)]
mod tests;
