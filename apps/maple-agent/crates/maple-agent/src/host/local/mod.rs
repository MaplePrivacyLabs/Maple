//! The in-process host: [`HostBackend`] over [`AgentRuntimeHandle`].
//!
//! The local window uses this directly. A server that publishes the same
//! runtime to remote clients is a sibling consumer of the runtime handle,
//! not a layer over this type. The submodules are what only a local host
//! needs: the filesystem, the git dir, and the account's SQLite stores.

mod directories;
mod git;
mod store;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{
    ContextUsage, DirectorySuggestion, HostBackend, HostBootstrap, HostEvent, HostEventHub, HostId,
    HostSessionDefaults, PERMISSION_MODE_AUTO, PERMISSION_MODE_SMART_APPROVE, UsageSummary,
    effective_harness_instructions,
};
use crate::agent::{
    AgentConfig, AgentCreateSessionRequest, AgentDesktopQueueSnapshot, AgentIntegration,
    AgentMcpServer, AgentPermissionDecision, AgentPermissionModeRequest, AgentPermissionResponse,
    AgentProjectRootRegistration, AgentProjectTrustStatus, AgentQueueControlRequest,
    AgentRenameSessionRequest, AgentRuntimeHandle, AgentRuntimeStatus, AgentSendMessageRequest,
    AgentSessionDetail, AgentSessionIntegrationKind, AgentSessionMcpServer, AgentSessionSummary,
    AgentSetIntegrationEnabledRequest, AgentSetSessionMcpServerRequest, AgentSetSessionWebRequest,
    AgentSetupIntegrationRequest, AgentSlashCommand, AgentStartRequest, AgentSubagent,
    AgentTaskState, MapleAgentService, RecentProjectRoot, SideQuestionTurn,
    account_sessions_db_path, account_tool_summaries_db_path,
};
use crate::maple_api::MapleApiSession;
use git::BranchWatchers;
use store::AccountStores;

/// Where the local host gets the validated OpenSecret session it needs to
/// start the runtime and rename tasks. The app implements this over its
/// persisted sign-in; it waits for a background credential restore first.
#[async_trait]
pub trait LocalHostAuth: Send + Sync + 'static {
    async fn api_session(&self, user_id: &str) -> Result<Arc<MapleApiSession>, String>;
}

/// Session defaults an older app kept in its own settings file. The local
/// host adopts them once into the account config; see
/// [`LocalHostBackend::migrate_session_defaults`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacySessionDefaults {
    pub permission_mode: Option<String>,
    pub web_enabled: Option<bool>,
    pub harness_instructions: Option<String>,
}

impl LegacySessionDefaults {
    pub fn is_empty(&self) -> bool {
        self.permission_mode.is_none()
            && self.web_enabled.is_none()
            && self.harness_instructions.is_none()
    }
}

/// Fallback context limit when the catalog lacks the model.
const DEFAULT_CONTEXT_LIMIT: i64 = 200_000;

/// How long a runtime start may take before a wedged enclave connection
/// surfaces as an error instead of an eternal spinner.
const RUNTIME_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub struct LocalHostBackend {
    id: HostId,
    service: MapleAgentService,
    user_id: String,
    auth: Arc<dyn LocalHostAuth>,
    events: Arc<HostEventHub>,
    stores: Arc<AccountStores>,
    branches: BranchWatchers,
}

impl LocalHostBackend {
    /// Bind the runtime to `user_id`. `events` must be the sink the
    /// service was built with, so runtime events and host events share one
    /// stream. The account's saved harness instructions reach the runtime
    /// on [`Self::apply_saved_harness`], which the bootstrap and runtime
    /// start call first.
    pub fn new(
        service: MapleAgentService,
        user_id: String,
        auth: Arc<dyn LocalHostAuth>,
        events: Arc<HostEventHub>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: HostId::local(),
            service,
            user_id,
            auth,
            events,
            stores: Arc::new(AccountStores::default()),
            branches: BranchWatchers::default(),
        })
    }

    /// Hand the account's saved harness instructions to the runtime.
    /// Applies to agents built afterwards.
    pub async fn apply_saved_harness(&self) -> Result<(), String> {
        let config = self.handle().await?.load_config().await?;
        self.apply_harness(&config);
        Ok(())
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    async fn handle(&self) -> Result<AgentRuntimeHandle, String> {
        self.service.handle_for_user(&self.user_id).await
    }

    async fn api_session(&self) -> Result<Arc<MapleApiSession>, String> {
        self.auth.api_session(&self.user_id).await
    }

    fn apply_harness(&self, config: &AgentConfig) {
        self.service
            .set_harness_instructions(effective_harness_instructions(
                config.harness_instructions.as_deref().unwrap_or(""),
            ));
    }

    /// Adopt defaults from an older app settings file into the account
    /// config, for each value the config never saved. Values the config
    /// already holds win.
    pub async fn migrate_session_defaults(
        &self,
        legacy: LegacySessionDefaults,
    ) -> Result<(), String> {
        if legacy.is_empty() {
            return Ok(());
        }
        let handle = self.handle().await?;
        let mut config = handle.load_config().await?;
        let mut changed = false;
        if config.default_permission_mode.is_none()
            && let Some(mode) = legacy.permission_mode
        {
            config.default_permission_mode = Some(normalize_permission_mode(&mode));
            changed = true;
        }
        if config.default_web_enabled.is_none()
            && let Some(enabled) = legacy.web_enabled
        {
            config.default_web_enabled = Some(enabled);
            changed = true;
        }
        if config.harness_instructions.is_none()
            && let Some(text) = legacy.harness_instructions
        {
            config.harness_instructions = Some(text);
            changed = true;
        }
        if changed {
            log::info!("adopted session defaults from the app settings into the account config");
            handle.save_config(config.clone()).await?;
            self.apply_harness(&config);
        }
        Ok(())
    }

    fn sessions_db_path(&self) -> Result<PathBuf, String> {
        account_sessions_db_path(self.service.paths(), &self.user_id)
    }

    fn summaries_db_path(&self) -> Result<PathBuf, String> {
        account_tool_summaries_db_path(self.service.paths(), &self.user_id)
    }
}

/// `smart_approve` or `auto`; anything else is the safer mode.
fn normalize_permission_mode(mode: &str) -> String {
    if mode == PERMISSION_MODE_AUTO {
        PERMISSION_MODE_AUTO.to_string()
    } else {
        PERMISSION_MODE_SMART_APPROVE.to_string()
    }
}

fn session_defaults_from(config: &AgentConfig) -> HostSessionDefaults {
    HostSessionDefaults {
        permission_mode: config
            .default_permission_mode
            .as_deref()
            .map(normalize_permission_mode)
            .unwrap_or_else(|| PERMISSION_MODE_SMART_APPROVE.to_string()),
        web_enabled: config.default_web_enabled.unwrap_or(true),
        harness_instructions: config.harness_instructions.clone().unwrap_or_default(),
        default_model: Some(config.default_model.clone()).filter(|model| !model.is_empty()),
    }
}

/// Root for a GUI start: the saved default when it still is a folder, else
/// the home directory. Never the process working directory: that is the
/// job of the `acp` command, not a windowed app started from a launcher.
fn gui_project_root(config: &AgentConfig) -> Option<String> {
    config
        .default_project_root
        .as_deref()
        .filter(|path| !path.trim().is_empty() && std::path::Path::new(path).is_dir())
        .map(str::to_owned)
        .or_else(|| dirs::home_dir().map(|path| path.to_string_lossy().to_string()))
}

/// Tasks that an ACP client created belong to that client's UI, not to a
/// desktop-class client's task list.
fn without_acp_sessions(mut sessions: Vec<AgentSessionSummary>) -> Vec<AgentSessionSummary> {
    sessions.retain(|session| !session.acp);
    sessions
}

#[async_trait]
impl HostBackend for LocalHostBackend {
    fn id(&self) -> &HostId {
        &self.id
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent> {
        self.events.subscribe()
    }

    async fn bootstrap(&self) -> Result<HostBootstrap, String> {
        let handle = self.handle().await?;
        let config = handle.load_config().await?;
        self.apply_harness(&config);
        let project_root = gui_project_root(&config);
        let sessions = without_acp_sessions(handle.list_sessions(None).await?);
        let recent_roots = handle
            .list_recent_project_roots()
            .await?
            .into_iter()
            .map(|root| root.path)
            .collect();
        // Same choice a session refresh makes: the newest unarchived task
        // under the root that the runtime will start in.
        let latest_id = sessions
            .iter()
            .find(|session| {
                // An empty task is a draft an older build persisted; the
                // client's own draft screen stands in for it.
                session.state != AgentTaskState::Archived
                    && session.message_count > 0
                    && Some(&session.project_root) == project_root.as_ref()
            })
            .map(|session| session.id.clone());
        let latest = match latest_id {
            Some(id) => match handle.load_session(id.clone()).await {
                Ok(detail) => Some(detail),
                Err(error) => {
                    // Bootstrap still succeeds without the transcript; the
                    // client loads it on demand and sees the error then.
                    log::debug!("bootstrap could not load the latest task {id}: {error}");
                    None
                }
            },
            None => None,
        };
        Ok(HostBootstrap {
            project_root,
            sessions,
            recent_roots,
            latest,
            session_defaults: session_defaults_from(&config),
        })
    }

    async fn start_runtime(
        &self,
        request: Option<AgentStartRequest>,
    ) -> Result<AgentRuntimeStatus, String> {
        let handle = self.handle().await?;
        let config = handle.load_config().await?;
        self.apply_harness(&config);
        let session = self.api_session().await?;
        // The agent falls back to the process working directory when no
        // root is given. That is right for `acp`, not for a client.
        let request = match request {
            Some(AgentStartRequest {
                project_root: None,
                model,
                mode,
            }) => Some(AgentStartRequest {
                project_root: gui_project_root(&config),
                model,
                mode,
            }),
            other => other,
        };
        tokio::time::timeout(RUNTIME_START_TIMEOUT, handle.start(session, request))
            .await
            .map_err(|_| "Runtime start timed out. Check your connection and retry.".to_string())?
    }

    async fn stop_runtime(&self) -> Result<AgentRuntimeStatus, String> {
        self.handle().await?.stop().await
    }

    async fn recent_project_roots(&self) -> Result<Vec<RecentProjectRoot>, String> {
        self.handle().await?.list_recent_project_roots().await
    }

    async fn select_project_root(
        &self,
        path: String,
    ) -> Result<AgentProjectRootRegistration, String> {
        self.handle().await?.save_recent_project_root(path).await
    }

    async fn remove_project_root(
        &self,
        path: String,
        fallback: Option<String>,
    ) -> Result<(), String> {
        self.handle()
            .await?
            .remove_project_root(path, fallback)
            .await
            .map(|_| ())
    }

    async fn suggest_directories(&self, query: String) -> Result<Vec<DirectorySuggestion>, String> {
        tokio::task::spawn_blocking(move || {
            directories::suggest(&query, dirs::home_dir().as_deref())
        })
        .await
        .map_err(|error| format!("Directory listing failed: {error}"))
    }

    async fn watch_project_root(&self, path: String) -> Result<(), String> {
        self.branches.watch(path, Arc::clone(&self.events));
        Ok(())
    }

    async fn unwatch_project_root(&self, path: String) -> Result<(), String> {
        self.branches.unwatch(&path);
        Ok(())
    }

    async fn project_trust(&self, path: String) -> Result<AgentProjectTrustStatus, String> {
        self.handle().await?.get_project_trust(path).await
    }

    async fn set_project_trust(
        &self,
        path: String,
        trusted: bool,
    ) -> Result<AgentProjectTrustStatus, String> {
        self.handle().await?.set_project_trust(path, trusted).await
    }

    async fn list_sessions(
        &self,
        project_root: Option<String>,
    ) -> Result<Vec<AgentSessionSummary>, String> {
        Ok(without_acp_sessions(
            self.handle().await?.list_sessions(project_root).await?,
        ))
    }

    async fn create_session(
        &self,
        request: Option<AgentCreateSessionRequest>,
    ) -> Result<AgentSessionDetail, String> {
        self.handle().await?.create_session(request).await
    }

    async fn load_session(&self, session_id: String) -> Result<AgentSessionDetail, String> {
        self.handle().await?.load_session(session_id).await
    }

    async fn rename_session(
        &self,
        session_id: String,
        title: String,
    ) -> Result<AgentSessionSummary, String> {
        let handle = self.handle().await?;
        let session = self.api_session().await?;
        handle
            .rename_session(session, AgentRenameSessionRequest { session_id, title })
            .await
    }

    async fn set_session_state(
        &self,
        session_id: String,
        state: AgentTaskState,
    ) -> Result<AgentSessionSummary, String> {
        self.handle()
            .await?
            .set_session_state(session_id, state)
            .await
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        self.handle().await?.delete_session(session_id).await
    }

    async fn compact_session(&self, session_id: String) -> Result<(), String> {
        self.handle().await?.compact_session(session_id).await
    }

    async fn session_subagents(&self, session_id: String) -> Result<Vec<AgentSubagent>, String> {
        Ok(self.handle().await?.session_subagents(&session_id).await)
    }

    async fn cancel_external_agent(
        &self,
        session_id: String,
        agent_id: String,
    ) -> Result<(), String> {
        self.handle()
            .await?
            .cancel_external_agent(&session_id, &agent_id)
            .await
    }

    async fn set_permission_mode(&self, session_id: String, mode: String) -> Result<(), String> {
        self.handle()
            .await?
            .set_permission_mode(AgentPermissionModeRequest { session_id, mode })
            .await
    }

    async fn set_session_web_enabled(
        &self,
        session_id: String,
        enabled: bool,
    ) -> Result<AgentSessionSummary, String> {
        self.handle()
            .await?
            .set_session_web_enabled(AgentSetSessionWebRequest {
                session_id,
                enabled,
            })
            .await
    }

    /// Latest context usage for a session from the goose usage ledger.
    /// The limit comes from the model catalog for the selected model;
    /// `MAPLE_CONTEXT_LIMIT` is a manual override; 200k is the fallback
    /// when the catalog lacks the model.
    async fn context_usage(
        &self,
        session_id: String,
        model: Option<String>,
    ) -> Result<Option<ContextUsage>, String> {
        let limit: i64 = match std::env::var("MAPLE_CONTEXT_LIMIT")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            Some(limit) if limit > 0 => limit,
            _ => match model {
                Some(model) => self
                    .handle()
                    .await?
                    .context_limit_for_model(&model)
                    .await?
                    .unwrap_or(DEFAULT_CONTEXT_LIMIT),
                None => DEFAULT_CONTEXT_LIMIT,
            },
        };
        let db = self.sessions_db_path()?;
        let stores = Arc::clone(&self.stores);
        // SQLite is synchronous; keep it off the async workers.
        let tokens = tokio::task::spawn_blocking(move || {
            stores
                .with_usage_db(&db, |conn| store::latest_context_tokens(conn, &session_id))
                .flatten()
        })
        .await
        .map_err(|error| format!("Context usage query failed: {error}"))?;
        Ok(tokens.map(|tokens| ContextUsage { tokens, limit }))
    }

    async fn read_image_attachment(
        &self,
        session_id: String,
        attachment_id: String,
    ) -> Result<Vec<u8>, String> {
        self.handle()
            .await?
            .read_image_attachment(session_id, attachment_id)
            .await
    }

    async fn send_message(&self, request: AgentSendMessageRequest) -> Result<String, String> {
        Ok(self.handle().await?.send_message(request).await?.run_id)
    }

    async fn cancel_run(&self, run_id: String) -> Result<(), String> {
        self.handle().await?.cancel_desktop_run(run_id).await
    }

    async fn cancel_queued_message(
        &self,
        session_id: String,
        queue_id: String,
    ) -> Result<AgentDesktopQueueSnapshot, String> {
        self.handle()
            .await?
            .cancel_queued_message(AgentQueueControlRequest {
                session_id,
                queue_id,
            })
            .await
    }

    async fn begin_queued_message_edit(
        &self,
        session_id: String,
        queue_id: String,
    ) -> Result<(), String> {
        self.handle()
            .await?
            .begin_queued_message_edit(AgentQueueControlRequest {
                session_id,
                queue_id,
            })
            .await
    }

    async fn end_queued_message_edit(
        &self,
        session_id: String,
        queue_id: String,
    ) -> Result<(), String> {
        self.handle()
            .await?
            .end_queued_message_edit(AgentQueueControlRequest {
                session_id,
                queue_id,
            })
            .await
    }

    async fn answer_question(&self, request_id: String, answer: String) -> Result<bool, String> {
        self.handle()
            .await?
            .answer_question_via_handle(&request_id, answer)
            .await
    }

    async fn permission_respond(
        &self,
        session_id: String,
        request_id: String,
        allow: bool,
    ) -> Result<(), String> {
        self.handle()
            .await?
            .permission_respond(AgentPermissionResponse {
                session_id,
                request_id,
                decision: if allow {
                    AgentPermissionDecision::AllowOnce
                } else {
                    AgentPermissionDecision::DenyOnce
                }
                .as_str()
                .to_string(),
            })
            .await
    }

    async fn ask_side_question(
        &self,
        session_id: String,
        request_id: String,
        prior: Vec<SideQuestionTurn>,
        question: String,
    ) -> Result<(), String> {
        self.handle()
            .await?
            .ask_side_question(&session_id, request_id, prior, question)
            .await
    }

    async fn summarize_tool_call(
        &self,
        session_id: String,
        tool_name: String,
        input: Option<serde_json::Value>,
        output_text: String,
    ) -> Result<Option<String>, String> {
        self.handle()
            .await?
            .summarize_tool_call(&session_id, &tool_name, input.as_ref(), &output_text)
            .await
    }

    async fn summarize_thinking(
        &self,
        session_id: String,
        thinking_text: String,
    ) -> Result<Option<String>, String> {
        self.handle()
            .await?
            .summarize_thinking(&session_id, &thinking_text)
            .await
    }

    async fn tool_summaries(&self, session_id: String) -> Result<HashMap<String, String>, String> {
        let db = self.summaries_db_path()?;
        let stores = Arc::clone(&self.stores);
        tokio::task::spawn_blocking(move || {
            stores.with_summary_db(&db, |conn| store::load_tool_summaries(conn, &session_id))
        })
        .await
        .map_err(|error| format!("Tool summary read failed: {error}"))?
    }

    async fn store_tool_summary(
        &self,
        session_id: String,
        item_id: String,
        summary: String,
    ) -> Result<(), String> {
        let db = self.summaries_db_path()?;
        let stores = Arc::clone(&self.stores);
        tokio::task::spawn_blocking(move || {
            stores.with_summary_db(&db, |conn| {
                store::store_tool_summary(conn, &session_id, &item_id, &summary)
            })
        })
        .await
        .map_err(|error| format!("Tool summary write failed: {error}"))?
    }

    async fn available_model_ids(&self) -> Result<Vec<String>, String> {
        self.handle().await?.available_model_ids().await
    }

    async fn model_supports_vision(&self, model: String) -> Result<Option<bool>, String> {
        self.handle().await?.model_supports_vision(&model).await
    }

    /// Filesystem scan, so it runs on a blocking thread.
    async fn list_slash_commands(
        &self,
        working_dir: Option<String>,
    ) -> Result<Vec<AgentSlashCommand>, String> {
        let service = self.service.clone();
        let user_id = self.user_id.clone();
        tokio::task::spawn_blocking(move || {
            service.list_slash_commands(Some(&user_id), working_dir.as_deref())
        })
        .await
        .map_err(|error| format!("Slash command scan failed: {error}"))
    }

    async fn resolve_slash_command(
        &self,
        working_dir: Option<String>,
        command: String,
        args: String,
    ) -> Result<Option<String>, String> {
        let service = self.service.clone();
        tokio::task::spawn_blocking(move || {
            service.resolve_slash_command(working_dir.as_deref(), &command, &args)
        })
        .await
        .map_err(|error| format!("Slash command resolve failed: {error}"))?
    }

    async fn list_session_mcp_servers(
        &self,
        session_id: String,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        self.handle()
            .await?
            .list_session_mcp_servers(session_id)
            .await
    }

    async fn set_session_mcp_server_enabled(
        &self,
        session_id: String,
        name: String,
        kind: AgentSessionIntegrationKind,
        enabled: bool,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        self.handle()
            .await?
            .set_session_mcp_server_enabled(AgentSetSessionMcpServerRequest {
                session_id,
                name,
                kind,
                enabled,
            })
            .await
    }

    async fn list_mcp_servers(&self) -> Result<Vec<AgentMcpServer>, String> {
        self.handle().await?.list_mcp_servers().await
    }

    async fn save_mcp_servers(
        &self,
        servers: Vec<AgentMcpServer>,
    ) -> Result<Vec<AgentMcpServer>, String> {
        self.handle().await?.save_mcp_servers(servers).await
    }

    async fn list_integrations(&self) -> Result<Vec<AgentIntegration>, String> {
        self.handle().await?.list_integrations().await
    }

    async fn set_integration_enabled(
        &self,
        id: String,
        enabled: bool,
    ) -> Result<Vec<AgentIntegration>, String> {
        self.handle()
            .await?
            .set_integration_enabled(AgentSetIntegrationEnabledRequest { id, enabled })
            .await
    }

    async fn setup_integration(&self, id: String) -> Result<Vec<AgentIntegration>, String> {
        self.handle()
            .await?
            .setup_integration(AgentSetupIntegrationRequest { id })
            .await
    }

    async fn session_defaults(&self) -> Result<HostSessionDefaults, String> {
        let config = self.handle().await?.load_config().await?;
        Ok(session_defaults_from(&config))
    }

    /// Save the settings-screen defaults. `default_model` is not among
    /// them: the chat screen owns it through [`Self::save_default_model`],
    /// and a settings snapshot taken before a model change would put the
    /// old model back if it were written here.
    async fn set_session_defaults(&self, defaults: HostSessionDefaults) -> Result<(), String> {
        let handle = self.handle().await?;
        let mut config = handle.load_config().await?;
        config.default_permission_mode = Some(normalize_permission_mode(&defaults.permission_mode));
        config.default_web_enabled = Some(defaults.web_enabled);
        config.harness_instructions = Some(defaults.harness_instructions);
        handle.save_config(config.clone()).await?;
        self.apply_harness(&config);
        Ok(())
    }

    async fn save_default_model(&self, model: String) -> Result<(), String> {
        let handle = self.handle().await?;
        let mut config = handle.load_config().await?;
        config.default_model = model;
        handle.save_config(config).await
    }

    async fn usage_summary(&self) -> Result<UsageSummary, String> {
        let db = self.sessions_db_path()?;
        let stores = Arc::clone(&self.stores);
        tokio::task::spawn_blocking(move || {
            stores
                .with_usage_db(&db, store::usage_from_ledger)
                .unwrap_or_default()
        })
        .await
        .map_err(|error| format!("Usage query failed: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_defaults_read_the_config_with_safe_fallbacks() {
        let mut config = AgentConfig::default();
        let defaults = session_defaults_from(&config);
        assert_eq!(defaults.permission_mode, PERMISSION_MODE_SMART_APPROVE);
        assert!(defaults.web_enabled);
        assert_eq!(defaults.harness_instructions, "");
        assert!(defaults.default_model.is_some());

        config.default_permission_mode = Some("auto".to_string());
        config.default_web_enabled = Some(false);
        config.harness_instructions = Some("custom".to_string());
        config.default_model = String::new();
        let defaults = session_defaults_from(&config);
        assert_eq!(defaults.permission_mode, PERMISSION_MODE_AUTO);
        assert!(!defaults.web_enabled);
        assert_eq!(defaults.harness_instructions, "custom");
        assert_eq!(defaults.default_model, None);

        config.default_permission_mode = Some("garbage".to_string());
        assert_eq!(
            session_defaults_from(&config).permission_mode,
            PERMISSION_MODE_SMART_APPROVE
        );
    }

    #[test]
    fn acp_sessions_stay_out_of_client_lists() {
        let mut acp = sample_session("acp");
        acp.acp = true;
        let kept = without_acp_sessions(vec![sample_session("desktop"), acp]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "desktop");
    }

    fn sample_session(id: &str) -> AgentSessionSummary {
        AgentSessionSummary {
            id: id.to_string(),
            title: id.to_string(),
            project_root: "/p".to_string(),
            created_ms: 0,
            updated_ms: 0,
            message_count: 0,
            model: None,
            mode: "smart_approve".to_string(),
            web_enabled: true,
            state: AgentTaskState::Active,
            acp: false,
        }
    }
}
