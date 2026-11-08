#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

//! Desktop and CLI facade for the profile's shared local host.
//! The host implementation retains the existing backend operations in `local`;
//! only this module knows about IPC. GPUI never owns a second agent runtime.

#[cfg_attr(not(feature = "desktop"), allow(unused_imports))]
pub use local::*;
pub(crate) mod attachments;
pub(crate) mod local;
pub(crate) mod paging;
#[cfg(any(feature = "desktop", test))]
pub use attachments::AttachmentUploadRequest;
pub use attachments::ImportAttachmentRequest;
#[cfg(feature = "desktop")]
pub use paging::*;
#[cfg(all(test, feature = "proxy"))]
mod local_api_tests;
mod mcp;

pub use crate::host::failure::HostFailure;
use crate::host::{RemoteClient, protocol::Request};
use maple_agent::agent::*;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{runtime::Runtime, sync::mpsc};

const RETIRED_AFTER_UPDATE: &str = "Maple has been updated. Quit and reopen Maple to continue.";

/// How long a remote viewer waits for the host's optional capabilities
/// before summarizing that call from its output instead.
const TOOL_SUMMARY_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What a tool summary asked as the call starts came back with.
#[derive(Debug, PartialEq, Eq)]
pub enum ToolStartSummary {
    /// The label, or `None` when the model gave nothing usable.
    Label(Option<String>),
    /// The remote host predates input-only summaries.
    NeedsOutput,
}

pub struct AgentBackend {
    runtime: tokio::runtime::Handle,
    owned_runtime: Option<Runtime>,
    client: BackendClient,
    api_url: String,
    controller: Option<Arc<AgentBackend>>,
    remote_host: Option<String>,
    retired_after_update: Arc<AtomicBool>,
    subscription_sequence: AtomicU64,
    /// Whether the remote host's validated peer information advertises
    /// input-only tool summaries; failed discovery is not remembered.
    tool_summary_start: std::sync::OnceLock<bool>,
}

impl Drop for AgentBackend {
    fn drop(&mut self) {
        // A completing UI/IPC future may release the last Arc on a Tokio
        // worker. Runtime::drop would panic there by trying to block. The
        // frontend owns no agent runs; host shutdown remains explicit.
        if let Some(runtime) = self.owned_runtime.take() {
            runtime.shutdown_background();
        }
    }
}

enum BackendClient {
    Host(Box<RemoteClient>),
    #[cfg(test)]
    InProcess(Box<LocalBackend>),
}

impl AgentBackend {
    #[cfg(feature = "proxy")]
    fn require_local_api(&self) -> Result<(), String> {
        self.ensure_active()?;
        if self.remote_host.is_some() {
            return Err("Local API settings require this computer's local host".into());
        }
        Ok(())
    }

    #[cfg(feature = "proxy")]
    pub async fn get_local_api_status(&self) -> Result<crate::local_api::LocalApiStatus, String> {
        self.require_local_api()?;
        self.call("get_local_api_status", ()).await
    }

    #[cfg(feature = "proxy")]
    // Recovery clients can stop without reading encrypted preferences. The
    // normal settings switch uses the durable configure operation instead.
    #[allow(dead_code)]
    pub async fn stop_local_api(&self) -> Result<crate::local_api::LocalApiStatus, String> {
        self.require_local_api()?;
        self.call("stop_local_api", ()).await
    }

    #[cfg(feature = "proxy")]
    pub async fn set_local_api_config(
        &self,
        user_id: &str,
        config: crate::local_api::LocalApiConfig,
    ) -> Result<crate::local_api::LocalApiStatus, String> {
        self.require_local_api()?;
        self.call("set_local_api_config", (user_id, config)).await
    }

    /// Store lifecycle commands always address this computer's host. A
    /// selected remote computer cannot forward passwords or erase requests.
    pub async fn local_control<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, String> {
        self.local_control_presented(method, params)
            .await
            .map_err(|failure| failure.message)
    }

    pub async fn local_control_presented<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, HostFailure> {
        if self.remote_host.is_some()
            || !matches!(
                method,
                "store_status"
                    | "cancel_oauth"
                    | "acknowledge_mode_change_notice"
                    | "host_status"
                    | "store_progress"
                    | "cancel_store_operation"
                    | "unlock"
                    | "setup_store"
                    | "start_fresh"
                    | "set_store_password"
                    | "change_store_password"
                    | "remove_store_password"
                    | "erase_local_data"
                    | "generate_store_passphrase"
                    | "assess_store_password"
                    | "retry_store_persistence"
                    | "repair_status"
                    | "repair_progress"
                    | "cancel_store_repair"
                    | "authenticate_repair_password"
                    | "check_store_integrity"
                    | "preview_salvage"
                    | "salvage_preview_page"
                    | "salvage_store"
            )
        {
            return Err(HostFailure::new(
                "local-desktop-required",
                "Store lifecycle commands require the local desktop",
            ));
        }
        self.call_presented(method, params).await
    }

    pub fn store_state(&self) -> maple_agent::store::StoreState {
        match &self.client {
            BackendClient::Host(client) => client.store_state(),
            #[cfg(test)]
            BackendClient::InProcess(backend) => backend
                .store()
                .map_or(maple_agent::store::StoreState::Open, |store| store.state()),
        }
    }
}

// Borrowed facade parameters become owned messages at the process boundary.
macro_rules! wire_type {
    ((&str)) => { String };
    ((Option<&str>)) => { Option<String> };
    (($ty:ty)) => { $ty };
}
macro_rules! local_arg {
    ($value:ident, (&str)) => {
        &$value
    };
    ($value:ident, (Option<&str>)) => {
        $value.as_deref()
    };
    ($value:ident, ($ty:ty)) => {
        $value
    };
}
macro_rules! authorize_arg {
    ($backend:ident, user_id, $value:ident) => {
        $backend.authorize_account_presented(&$value)?;
    };
    ($backend:ident, $name:ident, $value:ident) => {};
}
// Some operations are consumed through call_presented; keep their wire dispatch
// entries without generating an unused legacy String client wrapper.
macro_rules! operation_client {
    (dispatch_only; $name:ident ($( $arg:ident : $ty:tt ),*) -> $result:ty) => {};
    (test_only; $name:ident ($( $arg:ident : $ty:tt ),*) -> $result:ty) => {
        #[cfg(test)]
        operation_client!(; $name($($arg: $ty),*) -> $result);
    };
    (; $name:ident ($( $arg:ident : $ty:tt ),*) -> $result:ty) => {
        pub async fn $name(&self, $($arg: unpack_type!($ty)),*) -> Result<$result, String> {
            self.call(stringify!($name), ($($arg,)* )).await
        }
    };
}
macro_rules! operations {
    ($( $([$mode:ident])? $name:ident ( $( $arg:ident : $ty:tt ),* ) -> $result:ty; )*) => {
        impl AgentBackend {
            $(operation_client!($($mode)?; $name($($arg: $ty),*) -> $result);)*
        }
        pub(crate) async fn dispatch_inner(backend: &LocalBackend, request: Request) -> Result<serde_json::Value, HostFailure> {
            match request.method.as_str() {
                $(stringify!($name) => {
                    let ($($arg,)*): ($(wire_type!($ty),)*) = serde_json::from_value(request.params)
                        .map_err(|_| HostFailure::new("request-parameters", "Invalid host request parameters"))?;
                    $(authorize_arg!(backend, $arg, $arg);)*
                    let value = backend.$name($(local_arg!($arg, $ty)),*).await?;
                    serde_json::to_value(value).map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"))
                })*
                _ => dispatch_special(backend, request).await.map_err(HostFailure::from),
            }
        }
    }
}
macro_rules! unpack_type {
    (($ty:ty)) => {
        $ty
    };
}

operations! {
    get_ui_settings() -> crate::settings::AppSettings;
    set_ui_settings(patch: (serde_json::Value)) -> ();
    flush_ui_settings() -> ();
    dictionary(after: (Option<String>), limit: (u16)) -> maple_agent::store::DictionaryPage;
    add_word(word: (String)) -> bool;
    get_remote_sidebars(user_id: (&str)) -> HashMap<String, crate::settings::RemoteSidebarSettings>;
    set_remote_sidebar(user_id: (&str), computer: (String), patch: (serde_json::Value)) -> ();
    [dispatch_only] get_sidebar_order(user_id: (&str)) -> crate::settings::SidebarOrder;
    [dispatch_only] set_sidebar_order(user_id: (&str), order: (crate::settings::SidebarOrder)) -> ();
    [dispatch_only] set_task_pinned(user_id: (&str), task: (&str), pinned: (bool)) -> ();
    login(email: (String), password: (String)) -> AuthSession;
    login_guest(account_id: (String), password: (String)) -> AuthSession;
    logout_and_clear(account_user: (&str)) -> ();
    [dispatch_only] oauth_start(provider: (OAuthProvider)) -> String;
    [dispatch_only] oauth_wait_for_handoff(provider: (OAuthProvider)) -> AuthSession;
    [dispatch_only] oauth_complete(provider: (OAuthProvider), redirected_url: (String)) -> AuthSession;
    account(user_id: (&str)) -> MapleAccount;
    [dispatch_only] request_verification_email(user_id: (&str)) -> ();
    [dispatch_only] change_password(user_id: (&str), current_password: (String), new_password: (String)) -> ();
    [dispatch_only] request_account_deletion(user_id: (&str)) -> String;
    [dispatch_only] account_deletion_status() -> Option<AccountDeletionStatus>;
    [dispatch_only] confirm_account_deletion(account_user: (&str), confirmation_code: (String), plaintext_secret: (String)) -> ();
    [dispatch_only] list_api_keys(user_id: (&str)) -> Vec<MapleApiKey>;
    [dispatch_only] create_api_key(user_id: (&str), name: (String)) -> MapleApiKeyCreated;
    [dispatch_only] delete_api_key(user_id: (&str), name: (&str)) -> ();
    plan_usage(user_id: (&str)) -> Option<crate::billing::PlanUsage>;
    billing_status(user_id: (&str)) -> crate::billing::BillingStatus;
    [dispatch_only] billing_products() -> Vec<crate::billing::Product>;
    [dispatch_only] open_billing_portal(user_id: (&str)) -> ();
    [dispatch_only] start_checkout(user_id: (&str), product_id: (String)) -> ();
    [dispatch_only] api_credit_balance(user_id: (&str)) -> i64;
    [dispatch_only] purchase_api_credits(user_id: (&str), method: (crate::billing::PaymentMethod), credits: (u64)) -> ();
    [dispatch_only] start_runtime(user_id: (&str), request: (Option<AgentStartRequest>)) -> AgentRuntimeStatus;
    stop_runtime(user_id: (&str)) -> AgentRuntimeStatus;
    select_project_root(user_id: (&str), path: (String)) -> SelectedProject;
    browse_project_folders(user_id: (&str), request: (ProjectFolderRequest)) -> ProjectFolderPage;
    create_project_folder(user_id: (&str), parent_path: (String), name: (String)) -> String;
    [dispatch_only] create_scratch_project(user_id: (&str)) -> SelectedProject;
    recent_project_roots(user_id: (&str)) -> Vec<RecentProjectRoot>;
    rename_project_root(user_id: (&str), path: (String), name: (String)) -> RecentProjectRoot;
    archive_project_root(user_id: (&str), path: (String), fallback: (Option<String>)) -> ();
    [dispatch_only] create_session(user_id: (&str), request: (Option<AgentCreateSessionRequest>)) -> AgentSessionDetail;
    set_session_state(user_id: (&str), session_id: (&str), state: (AgentTaskState)) -> AgentSessionSummary;
    delete_session(user_id: (&str), session_id: (&str)) -> ();
    rename_session(user_id: (&str), session_id: (&str), title: (String)) -> AgentSessionSummary;
    project_trust(user_id: (&str), path: (String)) -> AgentProjectTrustStatus;
    set_project_trust(user_id: (&str), path: (String), trusted: (bool)) -> AgentProjectTrustStatus;
    [dispatch_only] cancel_queued_message(user_id: (&str), session_id: (&str), queue_id: (&str)) -> AgentDesktopQueueSnapshot;
    [dispatch_only] begin_queued_message_edit(user_id: (&str), session_id: (&str), queue_id: (&str)) -> ();
    [dispatch_only] end_queued_message_edit(user_id: (&str), session_id: (&str), queue_id: (&str)) -> ();
    [dispatch_only] cancel_run(user_id: (&str), run_id: (&str)) -> ();
    available_model_ids(user_id: (&str)) -> Vec<String>;
    model_catalog(user_id: (&str)) -> maple_sdk::ModelCatalogResponse;
    model_catalog_entry(user_id: (&str), model: (&str)) -> Option<maple_agent::agent::CatalogEntry>;
    audio_capabilities(user_id: (&str)) -> maple_agent::agent::AudioCapabilities;
    synthesize_speech(user_id: (&str), text: (String), voice: (String), speed: (f32)) -> Vec<u8>;
    transcribe_audio(user_id: (&str), wav: (Vec<u8>)) -> String;
    list_session_mcp_servers(user_id: (&str), session_id: (&str)) -> Vec<maple_agent::agent::AgentSessionMcpServer>;
    [dispatch_only] set_session_mcp_server_enabled(user_id: (&str), session_id: (&str), name: (&str), kind: (maple_agent::agent::AgentSessionIntegrationKind), enabled: (bool)) -> Vec<maple_agent::agent::AgentSessionMcpServer>;
    list_mcp_servers(user_id: (&str)) -> Vec<maple_agent::agent::AgentMcpServer>;
    list_mcp_definitions(user_id: (&str)) -> Vec<maple_agent::store::records::McpDefinition>;
    list_project_mcp_servers(user_id: (&str), project_root: (&str)) -> Vec<maple_agent::agent::AgentMcpServer>;
    [test_only] set_project_mcp_server_enabled(user_id: (&str), project_root: (&str), name: (&str), enabled: (bool)) -> ();
    list_integrations(user_id: (&str)) -> Vec<AgentIntegration>;
    set_integration_enabled(user_id: (&str), id: (&str), enabled: (bool)) -> Vec<AgentIntegration>;
    setup_integration(user_id: (&str), id: (&str), permissions: (AgentIntegrationPermissions)) -> Vec<AgentIntegration>;
    [dispatch_only] set_session_web_enabled(user_id: (&str), session_id: (&str), enabled: (bool)) -> AgentSessionSummary;
    [dispatch_only] set_session_permission_mode(user_id: (&str), session_id: (&str), mode: (maple_agent::agent::AgentPermissionMode)) -> AgentSessionSummary;
    [dispatch_only] compact_session(user_id: (&str), session_id: (&str)) -> ();
    session_subagents(user_id: (&str), session_id: (&str)) -> Vec<AgentSubagent>;
    [dispatch_only] cancel_external_agent(user_id: (&str), session_id: (&str), agent_id: (&str)) -> ();
    list_slash_commands(user_id: (&str), working_dir: (Option<String>)) -> Vec<AgentSlashCommand>;
    [dispatch_only] resolve_slash_command(user_id: (&str), working_dir: (Option<String>), command: (String), args: (String)) -> Option<String>;
    summarize_tool_call(user_id: (&str), session_id: (&str), tool_name: (String), input: (Option<serde_json::Value>), output_text: (String)) -> Option<String>;
    summarize_tool_call_start(user_id: (&str), session_id: (&str), tool_name: (String), input: (Option<serde_json::Value>)) -> Option<String>;
    summarize_thinking(user_id: (&str), session_id: (&str), thinking_text: (String)) -> Option<String>;
    ask_side_question(user_id: (&str), session_id: (&str), request_id: (String), prior: (Vec<maple_agent::agent::SideQuestionTurn>), question: (String)) -> ();
    context_usage(user_id: (&str), session_id: (&str), model: (Option<&str>)) -> Option<(i64, i64)>;
    [dispatch_only] answer_question(user_id: (&str), request_id: (&str), answer: (String)) -> bool;
    thinking_state(user_id: (&str), session_id: (Option<&str>), model: (&str)) -> maple_agent::agent::AgentThinkingState;
    set_thinking_level(user_id: (&str), session_id: (Option<&str>), model: (&str), level: (maple_agent::agent::ThinkingLevel)) -> maple_agent::agent::AgentThinkingState;
    save_default_model(user_id: (&str), model: (String)) -> ();
}

impl AgentBackend {
    /// UI state tests have no authenticated host and must not launch processes
    /// or attach to a developer's session. Runtime/IPC contracts are tested separately.
    #[cfg(test)]
    pub fn for_test(api_url: String, instructions: String) -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        let guard = runtime.enter();
        let mut backend = LocalBackend::new(api_url.clone(), instructions)?;
        backend.ui_fixture = true;
        drop(guard);
        Ok(Self {
            runtime: runtime.handle().clone(),
            owned_runtime: Some(runtime),
            client: BackendClient::InProcess(Box::new(backend)),
            api_url,
            controller: None,
            remote_host: None,
            retired_after_update: Arc::new(AtomicBool::new(false)),
            subscription_sequence: AtomicU64::new(0),
            tool_summary_start: std::sync::OnceLock::new(),
        })
    }

    #[cfg(all(test, feature = "desktop"))]
    pub(crate) fn for_remote_test(host: &str) -> Result<Self, String> {
        let mut backend = Self::for_test("http://127.0.0.1:9".into(), String::new())?;
        backend.remote_host = Some(host.into());
        Ok(backend)
    }

    /// Real-IPC fixtures supply their own contained endpoint and host lifetime.
    #[cfg(test)]
    pub(crate) fn from_host_client_for_test(client: RemoteClient) -> Self {
        Self {
            runtime: tokio::runtime::Handle::current(),
            owned_runtime: None,
            client: BackendClient::Host(Box::new(client)),
            api_url: "http://127.0.0.1:1".into(),
            controller: None,
            remote_host: None,
            retired_after_update: Arc::new(AtomicBool::new(false)),
            subscription_sequence: AtomicU64::new(0),
            tool_summary_start: std::sync::OnceLock::new(),
        }
    }

    /// Host calls sent and not yet answered. A fixture host records each
    /// request before it replies, so at zero it has seen every call sent.
    #[cfg(all(test, feature = "desktop"))]
    pub(crate) fn host_calls_in_flight_for_test(&self) -> usize {
        match &self.client {
            BackendClient::Host(client) => client.calls_in_flight_for_test(),
            BackendClient::InProcess(_) => 0,
        }
    }

    pub fn new(api_url: String, _harness_instructions: String) -> Result<Self, String> {
        let api_url = maple_agent::maple_api::validate_api_url(&api_url)?;
        let runtime = Runtime::new().map_err(|e| format!("Could not start host client: {e}"))?;
        let client = runtime.block_on(RemoteClient::connect(&api_url))?;
        Ok(Self {
            runtime: runtime.handle().clone(),
            owned_runtime: Some(runtime),
            client: BackendClient::Host(Box::new(client)),
            api_url,
            controller: None,
            remote_host: None,
            retired_after_update: Arc::new(AtomicBool::new(false)),
            subscription_sequence: AtomicU64::new(0),
            tool_summary_start: std::sync::OnceLock::new(),
        })
    }

    pub async fn remote(self: &Arc<Self>, peer: String) -> Result<Arc<Self>, String> {
        self.with_remote_connection(peer.clone(), || async {
            RemoteClient::connect_remote(&self.api_url, peer)
                .await
                .map(|client| BackendClient::Host(Box::new(client)))
        })
        .await
    }

    async fn with_remote_connection<F>(
        self: &Arc<Self>,
        peer: String,
        connect: impl FnOnce() -> F,
    ) -> Result<Arc<Self>, String>
    where
        F: Future<Output = Result<BackendClient, String>>,
    {
        self.ensure_active()?;
        let client = connect().await?;
        self.ensure_active()?;
        Ok(Arc::new(Self {
            runtime: self.runtime.clone(),
            owned_runtime: None,
            client,
            api_url: self.api_url.clone(),
            controller: Some(self.clone()),
            remote_host: Some(peer),
            retired_after_update: self.retired_after_update.clone(),
            subscription_sequence: AtomicU64::new(0),
            tool_summary_start: std::sync::OnceLock::new(),
        }))
    }

    /// Once the installed application has been replaced, this process must not
    /// submit work even if the desktop's restart helper fails. Remote views and
    /// their local controller share this terminal gate; cleanup can still spawn.
    pub fn retire_after_update(&self) {
        self.retired_after_update.store(true, Ordering::Release);
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn is_retired_after_update(&self) -> bool {
        self.retired_after_update.load(Ordering::Acquire)
    }

    fn ensure_active(&self) -> Result<(), String> {
        if self.retired_after_update.load(Ordering::Acquire) {
            Err(RETIRED_AFTER_UPDATE.into())
        } else {
            Ok(())
        }
    }
    pub fn remote_host(&self) -> Option<&str> {
        self.remote_host.as_deref()
    }

    /// Summarize a tool call from its input as it starts. A remote host
    /// must advertise the optional capability; otherwise the caller keeps
    /// the output-based `summarize_tool_call` once the call completes.
    pub async fn summarize_starting_tool_call(
        &self,
        user_id: &str,
        session_id: &str,
        tool_name: String,
        input: Option<serde_json::Value>,
    ) -> Result<ToolStartSummary, String> {
        if !self.host_summarizes_tool_calls_at_start().await {
            return Ok(ToolStartSummary::NeedsOutput);
        }
        self.summarize_tool_call_start(user_id, session_id, tool_name, input)
            .await
            .map(ToolStartSummary::Label)
    }

    async fn host_summarizes_tool_calls_at_start(&self) -> bool {
        // The local host always runs this build.
        if self.remote_host.is_none() {
            return true;
        }
        if let Some(&known) = self.tool_summary_start.get() {
            return known;
        }
        // Read the host's optional capabilities over this task connection.
        // The read changes no pairing or update state. Only a validated
        // answer is cached, for this connection; a probe that fails or
        // times out leaves this call on the output-based summary and the
        // next call asks again.
        let probe = tokio::time::timeout(
            TOOL_SUMMARY_PROBE_TIMEOUT,
            self.call::<crate::remote::update::PeerInfo>("peer_info", serde_json::json!({})),
        )
        .await;
        match probe {
            Ok(Ok(info)) if info.validate().is_ok() => *self
                .tool_summary_start
                .get_or_init(|| info.supports_tool_summary_start()),
            Ok(Ok(_)) => {
                log::debug!("Remote capabilities were invalid; summarizing from output");
                false
            }
            Ok(Err(_)) => {
                log::debug!("Remote capability probe failed; summarizing from output");
                false
            }
            Err(_) => {
                log::debug!("Remote capability probe timed out; summarizing from output");
                false
            }
        }
    }

    pub async fn remote_setting<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, String> {
        if !method.starts_with("remote_") || self.remote_host.is_some() {
            return Err("Remote settings require the local desktop".into());
        }
        self.call(method, params).await
    }

    async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, String> {
        self.call_presented(method, params)
            .await
            .map_err(|failure| failure.message)
    }

    pub async fn call_presented<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, HostFailure> {
        self.ensure_active()
            .map_err(|message| HostFailure::new("host-updated", message))?;
        // Catalog presentation, speech, account UI and remote-sidebar preferences
        // belong to the controller.
        // All task operations use the selected host's separate connection.
        let client = if let Some(controller) = &self.controller
            && matches!(
                method,
                "audio_capabilities"
                    | "model_catalog"
                    | "synthesize_speech"
                    | "transcribe_audio"
                    | "plan_usage"
                    | "account"
                    | "get_remote_sidebars"
                    | "set_remote_sidebar"
                    | "get_sidebar_order"
                    | "set_sidebar_order"
            ) {
            &controller.client
        } else {
            &self.client
        };
        match client {
            BackendClient::Host(client) => client.call_presented(method, params).await,
            #[cfg(test)]
            BackendClient::InProcess(backend) => {
                let value = dispatch_inner_presented(
                    backend,
                    Request {
                        id: 0,
                        method: method.into(),
                        send_id: None,
                        params: serde_json::to_value(params).map_err(|e| e.to_string())?,
                    },
                )
                .await?;
                serde_json::from_value(value).map_err(|e| HostFailure::from(e.to_string()))
            }
        }
    }
    /// The composer retains this ID through uncertain delivery. Only receipt
    /// queries, never another send, use it after a connection loss.
    #[cfg(test)]
    pub async fn send_message_identified(
        &self,
        user: &str,
        request: AgentSendMessageRequest,
        id: String,
    ) -> Result<String, String> {
        self.send_message_identified_presented(user, request, id)
            .await
            .map_err(|failure| failure.message)
    }
    pub async fn send_message_identified_presented(
        &self,
        user: &str,
        request: AgentSendMessageRequest,
        id: String,
    ) -> Result<String, HostFailure> {
        self.ensure_active()
            .map_err(|message| HostFailure::new("host-updated", message))?;
        match &self.client {
            BackendClient::Host(client) => {
                client
                    .call_identified_presented("send_message", (user, request), Some(id))
                    .await
            }
            #[cfg(test)]
            BackendClient::InProcess(backend) => backend
                .send_message(user, request)
                .await
                .map_err(HostFailure::from),
        }
    }
    #[cfg(test)]
    pub async fn send_outcome(&self, id: String) -> Result<crate::host::receipts::Outcome, String> {
        self.call("send_outcome", id).await
    }
    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.runtime.clone()
    }
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }
    pub async fn take_events(&self) -> Option<mpsc::Receiver<AgentServiceEvent>> {
        self.ensure_active().ok()?;
        match &self.client {
            BackendClient::Host(client) => client.take_events().await,
            #[cfg(test)]
            BackendClient::InProcess(backend) => {
                let mut source = backend.take_events().await?;
                let (sender, receiver) = mpsc::channel(256);
                self.spawn(async move {
                    while let Some(event) = source.recv().await {
                        if sender.send(event).await.is_err() {
                            break;
                        }
                    }
                });
                Some(receiver)
            }
        }
    }
    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn saved_user_id(&self) -> Option<String> {
        match &self.client {
            BackendClient::Host(client) => client.user_id(),
            #[cfg(test)]
            BackendClient::InProcess(_) => None,
        }
    }
    pub fn detach(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let backend = self.clone();
        self.spawn(async move {
            if backend.ensure_active().is_err() {
                return;
            }
            match &backend.client {
                BackendClient::Host(client) => client.detach().await,
                #[cfg(test)]
                BackendClient::InProcess(_) => {}
            }
        })
    }
    pub fn uses_hosted_oauth(&self) -> bool {
        crate::profile::AUTH_ORIGIN.is_some()
    }
    pub fn configured_model(&self) -> Option<String> {
        std::env::var("MAPLE_MODEL").ok()
    }
    pub fn default_start_request(&self) -> AgentStartRequest {
        AgentStartRequest {
            project_root: None,
            model: self.configured_model(),
        }
    }

    pub fn restore_in_background(self: &Arc<Self>) -> tokio::task::JoinHandle<RestoreOutcome> {
        let backend = self.clone();
        self.spawn(async move {
            backend
                .call("restore_auth", ())
                .await
                .unwrap_or(RestoreOutcome::Unavailable)
        })
    }
    pub fn set_harness_instructions(self: &Arc<Self>, instructions: String) {
        let backend = self.clone();
        self.spawn(async move {
            if let Err(error) = backend
                .call::<()>("set_harness_instructions", (instructions,))
                .await
            {
                log::warn!("Could not update host instructions: {error}");
            }
        });
    }
    pub fn cancel_oauth(self: &Arc<Self>) {
        let backend = self.clone();
        self.spawn(async move {
            let _ = backend.call::<()>("cancel_oauth", ()).await;
        });
    }
    pub fn begin_integration_setup(&self, id: &str) -> Result<AgentIntegrationPermissions, String> {
        self.ensure_active()?;
        if self.remote_host.is_some() {
            return Err("Open Maple on the execution computer to grant native permissions.".into());
        }
        // Native permission prompts remain on the signed app's UI thread.
        maple_agent::agent::begin_integration_setup(&AgentSetupIntegrationRequest { id: id.into() })
    }
    pub async fn saved_model(&self, user_id: &str) -> Option<String> {
        self.call("saved_model", (user_id,)).await.ok().flatten()
    }
}

async fn dispatch_special(
    backend: &LocalBackend,
    request: Request,
) -> Result<serde_json::Value, String> {
    if attachments::handles(&request.method) {
        return attachments::dispatch(backend, request).await;
    }
    if paging::is_paging(&request.method) {
        return paging::dispatch(backend, request, maple_agent::store::PageTransport::Local).await;
    }
    let invalid = |_| "Invalid host request parameters".to_string();
    fn encode(value: impl Serialize) -> Result<serde_json::Value, String> {
        serde_json::to_value(value).map_err(|_| "Could not encode host response".to_string())
    }
    match request.method.as_str() {
        "cancel_mcp_sign_in" => {
            let (user, name): (String, String) =
                serde_json::from_value(request.params).map_err(invalid)?;
            backend.authorize_account(&user)?;
            encode(backend.cancel_mcp_sign_in(&user, &name).await?)
        }
        "connect_mcp_app" => {
            let (user, id): (String, String) =
                serde_json::from_value(request.params).map_err(invalid)?;
            backend.authorize_account(&user)?;
            encode(backend.connect_mcp_app(&user, &id).await?)
        }
        "save_mcp_servers" => {
            let (user, servers): (String, Vec<AgentMcpServer>) =
                serde_json::from_value(request.params).map_err(invalid)?;
            backend.authorize_account(&user)?;
            encode(backend.save_mcp_servers(&user, servers).await?)
        }
        "save_mcp_definition_edits" => {
            let (user, edits): (String, Vec<maple_agent::store::records::McpServerEdit>) =
                serde_json::from_value(request.params).map_err(invalid)?;
            backend.authorize_account(&user)?;
            encode(backend.save_mcp_definition_edits(&user, edits).await?)
        }
        "send_message" => {
            let (user, message): (String, AgentSendMessageRequest) =
                serde_json::from_value(request.params)
                    .map_err(|_| "Invalid host request parameters")?;
            backend.authorize_account(&user)?;
            serde_json::to_value(backend.send_message(&user, message).await?)
                .map_err(|_| "Could not encode host response".into())
        }

        "restore_auth" => {
            serde_json::to_value(backend.validate_persisted_auth().await).map_err(|e| e.to_string())
        }
        #[cfg(feature = "proxy")]
        "get_local_api_status" => serde_json::to_value(backend.get_local_api_status().await?)
            .map_err(|_| "Could not encode Local API status".into()),
        #[cfg(feature = "proxy")]
        "stop_local_api" => serde_json::to_value(backend.stop_local_api().await?)
            .map_err(|_| "Could not encode Local API status".into()),
        #[cfg(feature = "proxy")]
        "set_local_api_config" => {
            let (user_id, config): (String, crate::local_api::LocalApiConfig) =
                serde_json::from_value(request.params).map_err(invalid)?;
            serde_json::to_value(backend.set_local_api_config(&user_id, config).await?)
                .map_err(|_| "Could not encode Local API status".into())
        }
        "cancel_oauth" => {
            backend.cancel_oauth();
            Ok(serde_json::Value::Null)
        }
        "set_harness_instructions" => {
            let (instructions,): (String,) =
                serde_json::from_value(request.params).map_err(invalid)?;
            backend.set_harness_instructions(instructions);
            Ok(serde_json::Value::Null)
        }
        "saved_model" => {
            let (user_id,): (String,) = serde_json::from_value(request.params).map_err(invalid)?;
            backend.authorize_account(&user_id)?;
            serde_json::to_value(backend.saved_model(&user_id).await).map_err(|e| e.to_string())
        }
        // Retained for authenticated host/remote callers; the GUI uses the
        // explicit archive operation and has no whole-project hiding client.
        "remove_project_root" => {
            let (user, path, fallback): (String, String, Option<String>) =
                serde_json::from_value(request.params).map_err(invalid)?;
            backend.authorize_account(&user)?;
            backend.remove_project_root(&user, path, fallback).await?;
            Ok(serde_json::Value::Null)
        }
        _ => Err("Unknown Maple host operation".into()),
    }
}

/// Remote origin is selected by the authenticated transport, never request
/// parameters. Neither compatibility saves nor typed patches can grant access
/// to an existing host secret through a changed server target.
async fn dispatch_remote_mcp(
    backend: &LocalBackend,
    request: Request,
) -> Result<serde_json::Value, String> {
    match request.method.as_str() {
        "save_mcp_servers" => {
            let (user, servers): (String, Vec<AgentMcpServer>) =
                serde_json::from_value(request.params)
                    .map_err(|_| "Invalid host request parameters".to_string())?;
            backend.authorize_account(&user)?;
            serde_json::to_value(backend.save_mcp_servers_remote(&user, servers).await?)
                .map_err(|_| "Could not encode host response".into())
        }
        "save_mcp_definition_edits" => {
            let (user, edits): (String, Vec<maple_agent::store::records::McpServerEdit>) =
                serde_json::from_value(request.params)
                    .map_err(|_| "Invalid host request parameters".to_string())?;
            backend.authorize_account(&user)?;
            serde_json::to_value(
                backend
                    .save_mcp_definition_edits_remote(&user, edits)
                    .await?,
            )
            .map_err(|_| "Could not encode host response".into())
        }
        _ => Err("Unknown remote MCP operation".into()),
    }
}

pub(crate) async fn dispatch(
    backend: &LocalBackend,
    request: Request,
) -> Result<serde_json::Value, String> {
    dispatch_presented(backend, request)
        .await
        .map_err(|failure| failure.message)
}

async fn dispatch_inner_presented(
    backend: &LocalBackend,
    request: Request,
) -> Result<serde_json::Value, HostFailure> {
    if request.method == "oauth_start_localized" {
        let (provider, locale): (OAuthProvider, String) = serde_json::from_value(request.params)
            .map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
        let value = backend
            .oauth_start_localized_presented(provider, locale)
            .await?;
        return serde_json::to_value(value)
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
    }
    match request.method.as_str() {
        "account" => {
            let (user_id,): (String,) = serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(backend.account_presented(&user_id).await?).map_err(
                |_| HostFailure::new("response-encoding", "Could not encode host response"),
            );
        }
        "request_verification_email" => {
            let (user_id,): (String,) = serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(
                backend
                    .request_verification_email_presented(&user_id)
                    .await?,
            )
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
        }
        "change_password" => {
            let (user_id, current_password, new_password): (String, String, String) =
                serde_json::from_value(request.params).map_err(|_| {
                    HostFailure::new("request-parameters", "Invalid host request parameters")
                })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(
                backend
                    .change_password_presented(&user_id, current_password, new_password)
                    .await?,
            )
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
        }
        "request_account_deletion" => {
            let (user_id,): (String,) = serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(
                backend.request_account_deletion_presented(&user_id).await?,
            )
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
        }
        "list_api_keys" => {
            let (user_id,): (String,) = serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(backend.list_api_keys_presented(&user_id).await?).map_err(
                |_| HostFailure::new("response-encoding", "Could not encode host response"),
            );
        }
        "create_api_key" => {
            let (user_id, name): (String, String) = serde_json::from_value(request.params)
                .map_err(|_| {
                    HostFailure::new("request-parameters", "Invalid host request parameters")
                })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(backend.create_api_key_presented(&user_id, name).await?)
                .map_err(|_| {
                    HostFailure::new("response-encoding", "Could not encode host response")
                });
        }
        "delete_api_key" => {
            let (user_id, name): (String, String) = serde_json::from_value(request.params)
                .map_err(|_| {
                    HostFailure::new("request-parameters", "Invalid host request parameters")
                })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(backend.delete_api_key_presented(&user_id, &name).await?)
                .map_err(|_| {
                    HostFailure::new("response-encoding", "Could not encode host response")
                });
        }
        "plan_usage" => {
            let (user_id,): (String,) = serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(backend.plan_usage_presented(&user_id).await?).map_err(
                |_| HostFailure::new("response-encoding", "Could not encode host response"),
            );
        }
        "billing_status" => {
            let (user_id,): (String,) = serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(backend.billing_status_presented(&user_id).await?)
                .map_err(|_| {
                    HostFailure::new("response-encoding", "Could not encode host response")
                });
        }
        "billing_products" => {
            let (): () = serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
            return serde_json::to_value(backend.billing_products_presented().await?).map_err(
                |_| HostFailure::new("response-encoding", "Could not encode host response"),
            );
        }
        "open_billing_portal" => {
            let (user_id,): (String,) = serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(backend.open_billing_portal_presented(&user_id).await?)
                .map_err(|_| {
                    HostFailure::new("response-encoding", "Could not encode host response")
                });
        }
        "start_checkout" => {
            let (user_id, product_id): (String, String) = serde_json::from_value(request.params)
                .map_err(|_| {
                    HostFailure::new("request-parameters", "Invalid host request parameters")
                })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(
                backend
                    .start_checkout_presented(&user_id, product_id)
                    .await?,
            )
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
        }
        "api_credit_balance" => {
            let (user_id,): (String,) = serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(backend.api_credit_balance_presented(&user_id).await?)
                .map_err(|_| {
                    HostFailure::new("response-encoding", "Could not encode host response")
                });
        }
        "purchase_api_credits" => {
            let (user_id, method, credits): (String, crate::billing::PaymentMethod, u64) =
                serde_json::from_value(request.params).map_err(|_| {
                    HostFailure::new("request-parameters", "Invalid host request parameters")
                })?;
            backend.authorize_account_presented(&user_id)?;
            return serde_json::to_value(
                backend
                    .purchase_api_credits_presented(&user_id, method, credits)
                    .await?,
            )
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
        }
        _ => {}
    }
    if matches!(
        request.method.as_str(),
        "oauth_start" | "oauth_wait_for_handoff"
    ) {
        let (provider,): (OAuthProvider,) =
            serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
        return if request.method == "oauth_start" {
            serde_json::to_value(backend.oauth_start_presented(provider).await?)
        } else {
            serde_json::to_value(backend.oauth_wait_for_handoff_presented(provider).await?)
        }
        .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
    }
    if request.method == "oauth_complete" {
        let (provider, url): (OAuthProvider, String) = serde_json::from_value(request.params)
            .map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
        return serde_json::to_value(backend.oauth_complete_presented(provider, url).await?)
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
    }
    if request.method == "account_deletion_status" {
        let (): () = serde_json::from_value(request.params).map_err(|_| {
            HostFailure::new("request-parameters", "Invalid host request parameters")
        })?;
        return serde_json::to_value(backend.account_deletion_status_presented().await?)
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
    }
    if request.method == "confirm_account_deletion" {
        let (user, code, secret): (String, String, String) = serde_json::from_value(request.params)
            .map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
        backend
            .confirm_account_deletion_presented(&user, code, secret)
            .await?;
        return Ok(serde_json::Value::Null);
    }
    if attachments::handles(&request.method) {
        return attachments::dispatch_presented(backend, request).await;
    }
    if paging::is_paging(&request.method) {
        return paging::dispatch_presented(
            backend,
            request,
            maple_agent::store::PageTransport::Local,
        )
        .await;
    }
    if request.method == "browse_project_folders" {
        let (user, query): (String, ProjectFolderRequest) = serde_json::from_value(request.params)
            .map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
        let result = backend
            .browse_project_folders_presented(&user, query)
            .await?;
        return serde_json::to_value(result)
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
    }
    if request.method == "create_project_folder" {
        let (user, path, name): (String, String, String) = serde_json::from_value(request.params)
            .map_err(|_| {
            HostFailure::new("request-parameters", "Invalid host request parameters")
        })?;
        let result = backend
            .create_project_folder_presented(&user, path, name)
            .await?;
        return serde_json::to_value(result)
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
    }
    if request.method == "create_scratch_project" {
        let (user,): (String,) = serde_json::from_value(request.params).map_err(|_| {
            HostFailure::new("request-parameters", "Invalid host request parameters")
        })?;
        let result = backend.create_scratch_project_presented(&user).await?;
        return serde_json::to_value(result)
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
    }
    if request.method == "sign_in_mcp_server" {
        let (user, sign_in): (String, maple_agent::agent::McpSignIn) =
            serde_json::from_value(request.params).map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
        backend.authorize_account_presented(&user)?;
        return serde_json::to_value(backend.sign_in_mcp_server_presented(&user, sign_in).await?)
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
    }
    if matches!(request.method.as_str(), "login" | "login_guest") {
        let (identity, password): (String, String) = serde_json::from_value(request.params)
            .map_err(|_| {
                HostFailure::new("request-parameters", "Invalid host request parameters")
            })?;
        let value = if request.method == "login" {
            backend.login_presented(identity, password).await?
        } else {
            backend.login_guest_presented(identity, password).await?
        };
        return serde_json::to_value(value)
            .map_err(|_| HostFailure::new("response-encoding", "Could not encode host response"));
    }
    dispatch_inner(backend, request).await
}

pub(crate) async fn dispatch_presented(
    backend: &LocalBackend,
    request: Request,
) -> Result<serde_json::Value, HostFailure> {
    #[cfg(feature = "proxy")]
    if matches!(
        request.method.as_str(),
        "get_local_api_status" | "stop_local_api"
    ) {
        return dispatch_inner_presented(backend, request).await;
    }
    // Account switches cannot interleave with an operation admitted for the
    // previous account. Restoring that same account is a reader: its network
    // validation must not prevent local history, configuration, or stop calls.
    // The restore mutex still serializes credential validation/publication.
    // Cancelling an OAuth wait must remain independent.
    if matches!(
        request.method.as_str(),
        "cancel_oauth" | "import_attachment"
    ) {
        return dispatch_inner_presented(backend, request).await;
    }
    if matches!(
        request.method.as_str(),
        "login"
            | "login_guest"
            | "logout_and_clear"
            | "oauth_wait_for_handoff"
            | "oauth_complete"
            | "confirm_account_deletion"
    ) {
        #[cfg(feature = "proxy")]
        backend.local_api.stop().await;
        let _account = backend.account_gate.write().await;
        #[cfg(feature = "proxy")]
        backend.local_api.stop().await;
        #[cfg(feature = "proxy")]
        let generation = backend.local_api.generation();
        let result = dispatch_inner_presented(backend, request).await;
        #[cfg(feature = "proxy")]
        if result.is_ok() {
            let _ = backend.resume_local_api(generation).await;
        }
        result
    } else {
        let _account = backend.account_gate.read().await;
        #[cfg(feature = "proxy")]
        let resume = (request.method == "restore_auth").then(|| backend.local_api.generation());
        let result = dispatch_inner_presented(backend, request).await;
        #[cfg(feature = "proxy")]
        if result.is_ok()
            && let Some(generation) = resume
        {
            let _ = backend.resume_local_api(generation).await;
        }
        result
    }
}

/// The remote connection selects its transport after the method allowlist and
/// account checks; request parameters cannot opt into local page budgets.
pub(crate) async fn dispatch_remote_inner(
    backend: &LocalBackend,
    request: Request,
) -> Result<serde_json::Value, String> {
    dispatch_remote_inner_presented(backend, request)
        .await
        .map_err(|failure| failure.message)
}

pub(crate) async fn dispatch_remote_inner_presented(
    backend: &LocalBackend,
    request: Request,
) -> Result<serde_json::Value, HostFailure> {
    // An authenticated remote request must not open a browser on this
    // computer, even with malformed params.
    if matches!(
        request.method.as_str(),
        "sign_in_mcp_server" | "cancel_mcp_sign_in" | "connect_mcp_app"
    ) {
        return Err("Signing in to an MCP server requires the local desktop.".into());
    }
    if paging::is_paging(&request.method) {
        return paging::dispatch_presented(
            backend,
            request,
            maple_agent::store::PageTransport::Remote,
        )
        .await;
    }
    if matches!(
        request.method.as_str(),
        "save_mcp_servers" | "save_mcp_definition_edits"
    ) {
        return dispatch_remote_mcp(backend, request)
            .await
            .map_err(HostFailure::from);
    }
    dispatch_inner_presented(backend, request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Arc<AgentBackend> {
        Arc::new(AgentBackend::for_test("http://127.0.0.1:1".into(), String::new()).unwrap())
    }

    fn in_process_client(controller: &AgentBackend) -> BackendClient {
        let _runtime = controller.runtime.enter();
        let mut backend = LocalBackend::new(controller.api_url.clone(), String::new()).unwrap();
        backend.ui_fixture = true;
        BackendClient::InProcess(Box::new(backend))
    }

    #[test]
    fn legacy_tool_summary_call_keeps_its_wire_shape() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("remote/tool_summary_compat.json")).unwrap();
        let shipped = &fixtures["legacy_summarize_command"];
        // Old client → new host: the shipped five-argument command still
        // decodes and reaches the output-based summary.
        let command: crate::remote::wire::Command =
            serde_json::from_value(shipped.clone()).unwrap();
        assert_eq!(
            command.method,
            crate::remote::wire::Method::SummarizeToolCall
        );
        // New client → any host: a finished call on an older host sends
        // exactly the shipped arguments.
        assert_eq!(
            serde_json::to_value((
                "account",
                "task",
                "Terminal".to_string(),
                Some(serde_json::json!({"command": "ls"})),
                "main.rs".to_string(),
            ))
            .unwrap(),
            shipped["params"]
        );
        let backend = fixture();
        let runtime = backend.runtime_handle();
        runtime.block_on(async {
            let legacy = backend
                .call::<Option<String>>("summarize_tool_call", command.params.clone())
                .await;
            assert_ne!(legacy, Err("Invalid host request parameters".into()));
            // The input-only call is a separate operation with its own shape.
            let start = backend
                .call::<Option<String>>("summarize_tool_call_start", command.params)
                .await;
            assert_eq!(start, Err("Invalid host request parameters".into()));
        });
    }

    #[test]
    fn failed_tool_summary_capability_probe_is_not_remembered() {
        let mut backend =
            AgentBackend::for_test("http://127.0.0.1:1".into(), String::new()).unwrap();
        // The local host is always this build.
        let runtime = backend.runtime_handle();
        assert!(runtime.block_on(backend.host_summarizes_tool_calls_at_start()));
        // A remote host whose capabilities cannot be read keeps this call on
        // the output-based summary, and the next call asks again.
        backend.remote_host = Some("peer".into());
        assert!(!runtime.block_on(backend.host_summarizes_tool_calls_at_start()));
        assert!(backend.tool_summary_start.get().is_none());
    }

    #[test]
    fn retired_frontend_rejects_rpc_send_and_native_permissions_but_can_run_cleanup() {
        struct MustNotSerialize;
        impl Serialize for MustNotSerialize {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                panic!("retired requests must not reach serialization or dispatch")
            }
        }

        let backend = fixture();
        let runtime = backend.runtime_handle();
        runtime
            .block_on(backend.call::<()>("cancel_oauth", ()))
            .unwrap();
        backend.retire_after_update();
        backend.retire_after_update();
        runtime.block_on(async {
            assert_eq!(
                backend.call::<()>("cancel_oauth", MustNotSerialize).await,
                Err(RETIRED_AFTER_UPDATE.into())
            );
            assert_eq!(
                backend
                    .send_message_identified(
                        "fixture-user",
                        AgentSendMessageRequest {
                            session_id: "fixture-session".into(),
                            text: "must not be dispatched".into(),
                            model: None,
                            context_limit: None,
                            vision_capable: false,
                            steer: false,
                            queue_id: None,
                            attachments: Vec::new(),
                        },
                        "fixture-send".into(),
                    )
                    .await,
                Err(RETIRED_AFTER_UPDATE.into())
            );
            assert_eq!(
                backend.begin_integration_setup("fixture-integration").err(),
                Some(RETIRED_AFTER_UPDATE.into())
            );
            assert!(backend.take_events().await.is_none());
            assert_eq!(
                backend.spawn(async { "cleanup completed" }).await.unwrap(),
                "cleanup completed"
            );
        });
    }

    #[test]
    fn remote_and_controller_share_terminal_update_retirement() {
        let controller = fixture();
        let runtime = controller.runtime_handle();
        runtime.block_on(async {
            let remote = controller
                .with_remote_connection("fixture-peer".into(), || async {
                    Ok(in_process_client(&controller))
                })
                .await
                .unwrap();
            let other_remote = controller
                .with_remote_connection("other-fixture-peer".into(), || async {
                    Ok(in_process_client(&controller))
                })
                .await
                .unwrap();
            remote.call::<()>("cancel_oauth", ()).await.unwrap();
            remote.retire_after_update();
            for backend in [&controller, &remote, &other_remote] {
                assert_eq!(
                    backend.call::<()>("cancel_oauth", ()).await,
                    Err(RETIRED_AFTER_UPDATE.into())
                );
                // Controller-routed operations must be denied too.
                assert_eq!(
                    backend.account("fixture-user").await.err(),
                    Some(RETIRED_AFTER_UPDATE.into())
                );
            }
        });
    }

    #[test]
    fn retirement_rejects_remote_connection_before_connect_and_after_pending_connect() {
        let controller = fixture();
        let runtime = controller.runtime_handle();
        runtime.block_on(async {
            let client = in_process_client(&controller);
            let result = controller
                .with_remote_connection("fixture-peer".into(), || async {
                    // Simulate installation committing while connection setup
                    // is in flight. No active remote facade may be published.
                    controller.retire_after_update();
                    Ok(client)
                })
                .await;
            assert_eq!(result.err(), Some(RETIRED_AFTER_UPDATE.into()));

            let connected = AtomicBool::new(false);
            let result = controller
                .with_remote_connection("fixture-peer".into(), || async {
                    connected.store(true, Ordering::Relaxed);
                    Err("connection must not be attempted".into())
                })
                .await;
            assert_eq!(result.err(), Some(RETIRED_AFTER_UPDATE.into()));
            assert!(!connected.load(Ordering::Relaxed));
            assert_eq!(
                controller.remote("fixture-peer".into()).await.err(),
                Some(RETIRED_AFTER_UPDATE.into())
            );
        });
    }

    #[tokio::test]
    async fn remote_sidebar_preferences_stay_in_the_controller_store() {
        fn store(root: &std::path::Path) -> Arc<maple_agent::store::Store> {
            Arc::new(
                maple_agent::store::Store::open(maple_agent::store::StorePaths::new(root)).unwrap(),
            )
        }
        fn backend(store: Arc<maple_agent::store::Store>) -> AgentBackend {
            let mut local =
                LocalBackend::new_with_store("http://127.0.0.1:1".into(), String::new(), store)
                    .unwrap();
            local.ui_fixture = true;
            AgentBackend {
                runtime: tokio::runtime::Handle::current(),
                owned_runtime: None,
                client: BackendClient::InProcess(Box::new(local)),
                api_url: "http://127.0.0.1:1".into(),
                controller: None,
                remote_host: None,
                retired_after_update: Arc::new(AtomicBool::new(false)),
                subscription_sequence: AtomicU64::new(0),
                tool_summary_start: std::sync::OnceLock::new(),
            }
        }

        let local_root = tempfile::tempdir().unwrap();
        let remote_root = tempfile::tempdir().unwrap();
        let local_store = store(local_root.path());
        let remote_store = store(remote_root.path());
        let local_account = local_store.upsert_account("account", 1).unwrap();
        let remote_account = remote_store.upsert_account("account", 1).unwrap();
        let setting =
            serde_json::json!({"peer":{"display_name":"Local alias","pinned_tasks":["local-pin"]}});
        local_store
            .set_account_setting(local_account, "remote.sidebars", &setting, 1)
            .unwrap();
        let remote_setting = serde_json::json!({"peer":{"display_name":"Remote alias","pinned_tasks":["remote-pin"]}});
        remote_store
            .set_account_setting(remote_account, "remote.sidebars", &remote_setting, 1)
            .unwrap();

        let controller = Arc::new(backend(local_store.clone()));
        let mut selected = backend(remote_store.clone());
        selected.controller = Some(controller.clone());
        selected.remote_host = Some("peer".into());
        let settings = selected.get_remote_sidebars("account").await.unwrap();
        assert_eq!(settings["peer"].display_name, "Local alias");
        assert_eq!(settings["peer"].pinned_tasks, ["local-pin"]);
        selected
            .set_remote_sidebar(
                "account",
                "peer".into(),
                serde_json::json!({"display_name":"Saved nickname","pinned_tasks":["new-local-pin"]}),
            )
            .await
            .unwrap();
        let reloaded = controller.get_remote_sidebars("account").await.unwrap();
        assert_eq!(reloaded["peer"].pinned_tasks, ["new-local-pin"]);
        assert_eq!(
            remote_store
                .account_setting(remote_account, "remote.sidebars")
                .unwrap(),
            Some(remote_setting.clone())
        );
        // Retire every store/backend reference, then independently reopen both
        // stores. A warm facade/cache read would miss the original regression.
        drop(selected);
        drop(controller);
        drop(local_store);
        drop(remote_store);
        let reopened_local = store(local_root.path());
        let reopened_remote = store(remote_root.path());
        reopened_local.upsert_account("other-account", 2).unwrap();
        let controller = Arc::new(backend(reopened_local.clone()));
        let mut selected = backend(reopened_remote.clone());
        selected.controller = Some(controller);
        selected.remote_host = Some("peer".into());
        let restored = selected.get_remote_sidebars("account").await.unwrap();
        assert_eq!(restored["peer"].display_name, "Saved nickname");
        assert_eq!(restored["peer"].pinned_tasks, ["new-local-pin"]);
        assert!(
            selected
                .get_remote_sidebars("other-account")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reopened_remote
                .account_setting(remote_account, "remote.sidebars")
                .unwrap(),
            Some(remote_setting)
        );
    }

    #[tokio::test]
    async fn final_frontend_reference_can_be_dropped_by_an_async_callback() {
        let backend = tokio::task::spawn_blocking(|| {
            AgentBackend::for_test("http://127.0.0.1:1".into(), String::new()).unwrap()
        })
        .await
        .unwrap();
        tokio::spawn(async move {
            drop(backend);
        })
        .await
        .expect("frontend shutdown must not panic on a runtime worker");
    }
}

#[cfg(test)]
mod presented_failure_tests {
    #[tokio::test]
    async fn oauth_and_account_admission_emit_codes_before_network_or_filesystem_work() {
        let root = tempfile::tempdir().unwrap();
        let backend = super::LocalBackend::isolated_for_test(root.path());
        for (method, params, expected) in [
            (
                "oauth_complete",
                serde_json::json!([super::OAuthProvider::Google, "not a callback"]),
                "oauth-url",
            ),
            (
                "oauth_wait_for_handoff",
                serde_json::json!([super::OAuthProvider::Google]),
                "oauth-cancelled",
            ),
            (
                "browse_project_folders",
                serde_json::json!(["foreign-account", {"path":"/", "limit":1}]),
                "deletion-account-changed",
            ),
        ] {
            let failure = super::dispatch_presented(
                &backend,
                super::Request {
                    id: 1,
                    method: method.into(),
                    params,
                    send_id: None,
                },
            )
            .await
            .unwrap_err();
            assert_eq!(failure.code, expected);
            assert!(backend.saved_user_id().is_none());
        }
    }

    use super::*;

    #[tokio::test]
    async fn login_validation_preserves_codes_through_account_dispatch() {
        let backend = LocalBackend::new("http://127.0.0.1:1".into(), String::new()).unwrap();
        for (method, identity, password, code) in [
            ("login", "", "synthetic", "email-required"),
            ("login", "person@example.test", "", "password-required"),
            (
                "login_guest",
                "not-an-id",
                "synthetic",
                "account-id-invalid",
            ),
        ] {
            let failure = dispatch_presented(
                &backend,
                Request {
                    id: 1,
                    method: method.into(),
                    send_id: None,
                    params: serde_json::json!([identity, password]),
                },
            )
            .await
            .unwrap_err();
            assert_eq!(failure.code, code);
            assert!(!failure.message.is_empty());
        }
        assert!(backend.saved_user_id().is_none());
    }
}
