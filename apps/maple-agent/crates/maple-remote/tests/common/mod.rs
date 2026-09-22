//! Shared fixtures: a scripted host and handshake values.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use maple_agent::agent::{
    AgentCreateSessionRequest, AgentDesktopQueueSnapshot, AgentIntegration, AgentMcpServer,
    AgentProjectRootRegistration, AgentProjectTrustStatus, AgentRuntimeStatus,
    AgentSendMessageRequest, AgentSessionDetail, AgentSessionIntegrationKind,
    AgentSessionMcpServer, AgentSessionSummary, AgentSlashCommand, AgentStartRequest,
    AgentSubagent, AgentTaskState, AgentTimelineItem, RecentProjectRoot, SideQuestionTurn,
};
use maple_agent::host::{
    ContextUsage, DirectorySuggestion, HostBackend, HostBootstrap, HostEvent, HostEventHub, HostId,
    HostSessionDefaults, UsageSummary,
};
use maple_remote::client::ClientConfig;
use maple_remote::devices::DeviceStore;
use maple_remote::keys::StaticKey;
use maple_remote::listen::{HostStores, serve_listener};
use maple_remote::pairing::{PairingLimiter, PendingPairingStore};
use maple_remote::server::{HostIdentity, HostServer, HostServerConfig};
use maple_remote::wire::{ClientHello, DeviceInfo, HostInfo, PROTOCOL_VERSION};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A host listening on a real loopback port, with its stores in a
/// temporary directory that goes when the host is dropped.
pub struct Host {
    pub address: String,
    pub key: StaticKey,
    pub devices: Arc<DeviceStore>,
    pub pending: Arc<PendingPairingStore>,
    pub shutdown: CancellationToken,
    _dir: TempDir,
}

pub struct TempDir(pub std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Serve `fake` on a loopback port with `identity()` and a device hook
/// that records every accepted hello, like the app's.
pub async fn start_host(fake: Arc<FakeHost>) -> Host {
    let dir = std::env::temp_dir().join(format!("maple-transport-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = StaticKey::load_or_create(&dir.join("host_key.json")).unwrap();
    let devices = Arc::new(DeviceStore::new(dir.join("devices.json")));
    let pending = Arc::new(PendingPairingStore::new(dir.join("pending_pairing.json")));
    let hook_devices = Arc::clone(&devices);
    let config = HostServerConfig {
        on_client_hello: Some(Arc::new(move |hello: &ClientHello| {
            hook_devices
                .touch(
                    &hello.device.public_key,
                    &hello.device.name,
                    hello.device.user_id.as_deref(),
                )
                .unwrap();
        })),
        ..Default::default()
    };
    let server = HostServer::new(
        fake,
        HostInfo {
            id: key.public_id(),
            ..info()
        },
        identity(),
        config,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let shutdown = CancellationToken::new();
    let stores = Arc::new(HostStores {
        key: key.clone(),
        devices: Arc::clone(&devices),
        pending_pairing: Arc::clone(&pending),
        limiter: PairingLimiter::new(3, Duration::from_secs(60)),
    });
    tokio::spawn(serve_listener(listener, server, stores, shutdown.clone()));
    Host {
        address,
        key,
        devices,
        pending,
        shutdown,
        _dir: TempDir(dir),
    }
}

/// A host whose answers are fixed and whose calls are counted.
pub struct FakeHost {
    pub id: HostId,
    pub events: Arc<HostEventHub>,
    pub timeline_len: usize,
    pub attachment: Vec<u8>,
    pub loads: AtomicUsize,
    /// Every root passed to `unwatch_project_root`, in order.
    pub unwatched: std::sync::Mutex<Vec<String>>,
    /// Every request passed to `send_message`, in order.
    pub sent: std::sync::Mutex<Vec<AgentSendMessageRequest>>,
}

pub fn summary(id: &str) -> AgentSessionSummary {
    AgentSessionSummary {
        id: id.to_string(),
        title: format!("Task {id}"),
        project_root: "/p".to_string(),
        created_ms: 1,
        updated_ms: 2,
        message_count: 0,
        model: None,
        mode: "smart_approve".to_string(),
        web_enabled: true,
        state: AgentTaskState::Active,
        acp: false,
    }
}

pub fn item(index: usize) -> AgentTimelineItem {
    AgentTimelineItem {
        id: format!("item-{index}"),
        item_type: "message".to_string(),
        role: Some("assistant".to_string()),
        title: None,
        text: Some("x".repeat(3000)),
        status: None,
        input: None,
        output: None,
        created_ms: index as u128,
        merge: "none".to_string(),
    }
}

impl FakeHost {
    pub fn new(timeline_len: usize) -> Arc<Self> {
        Arc::new(Self {
            id: HostId::new("fake"),
            events: Arc::new(HostEventHub::default()),
            timeline_len,
            attachment: (0..(600 * 1024)).map(|i| (i % 251) as u8).collect(),
            loads: AtomicUsize::new(0),
            unwatched: std::sync::Mutex::new(Vec::new()),
            sent: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn detail(&self, id: &str) -> AgentSessionDetail {
        AgentSessionDetail {
            session: summary(id),
            timeline: (0..self.timeline_len).map(item).collect(),
            mcp_errors: Vec::new(),
            queue: AgentDesktopQueueSnapshot {
                revision: 0,
                items: Vec::new(),
            },
        }
    }
}

fn unsupported<T>() -> Result<T, String> {
    Err("unsupported in the fake host".to_string())
}

#[async_trait]
impl HostBackend for FakeHost {
    fn id(&self) -> &HostId {
        &self.id
    }
    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent> {
        self.events.subscribe()
    }
    async fn bootstrap(&self) -> Result<HostBootstrap, String> {
        Ok(HostBootstrap {
            project_root: Some("/p".to_string()),
            sessions: vec![summary("s1"), summary("s2")],
            recent_roots: vec!["/p".to_string()],
            latest: Some(self.detail("s1")),
            session_defaults: HostSessionDefaults {
                permission_mode: "auto".to_string(),
                ..Default::default()
            },
        })
    }
    async fn start_runtime(
        &self,
        _request: Option<AgentStartRequest>,
    ) -> Result<AgentRuntimeStatus, String> {
        Ok(AgentRuntimeStatus {
            running: true,
            project_root: Some("/p".to_string()),
            model: None,
            mode: None,
            active_runs: HashMap::new(),
        })
    }
    async fn stop_runtime(&self) -> Result<AgentRuntimeStatus, String> {
        unsupported()
    }
    async fn recent_project_roots(&self) -> Result<Vec<RecentProjectRoot>, String> {
        unsupported()
    }
    async fn select_project_root(
        &self,
        path: String,
    ) -> Result<AgentProjectRootRegistration, String> {
        Err(format!("cannot select {path}"))
    }
    async fn remove_project_root(&self, _: String, _: Option<String>) -> Result<(), String> {
        unsupported()
    }
    async fn suggest_directories(&self, query: String) -> Result<Vec<DirectorySuggestion>, String> {
        Ok(vec![DirectorySuggestion {
            path: format!("{query}dir"),
            name: "dir".to_string(),
        }])
    }
    async fn watch_project_root(&self, path: String) -> Result<(), String> {
        self.events.publish(HostEvent::ProjectBranch {
            project_root: path,
            branch: Some("main".to_string()),
        });
        Ok(())
    }
    async fn unwatch_project_root(&self, path: String) -> Result<(), String> {
        self.unwatched.lock().unwrap().push(path);
        Ok(())
    }
    async fn project_trust(&self, _: String) -> Result<AgentProjectTrustStatus, String> {
        unsupported()
    }
    async fn set_project_trust(
        &self,
        _: String,
        _: bool,
    ) -> Result<AgentProjectTrustStatus, String> {
        unsupported()
    }
    async fn list_sessions(&self, _: Option<String>) -> Result<Vec<AgentSessionSummary>, String> {
        Ok(vec![summary("s1"), summary("s2")])
    }
    async fn create_session(
        &self,
        _: Option<AgentCreateSessionRequest>,
    ) -> Result<AgentSessionDetail, String> {
        unsupported()
    }
    async fn load_session(&self, session_id: String) -> Result<AgentSessionDetail, String> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        Ok(self.detail(&session_id))
    }
    async fn rename_session(
        &self,
        id: String,
        title: String,
    ) -> Result<AgentSessionSummary, String> {
        let mut renamed = summary(&id);
        renamed.title = title;
        Ok(renamed)
    }
    async fn set_session_state(
        &self,
        _: String,
        _: AgentTaskState,
    ) -> Result<AgentSessionSummary, String> {
        unsupported()
    }
    async fn delete_session(&self, _: String) -> Result<(), String> {
        unsupported()
    }
    async fn compact_session(&self, _: String) -> Result<(), String> {
        unsupported()
    }
    async fn session_subagents(&self, _: String) -> Result<Vec<AgentSubagent>, String> {
        Ok(Vec::new())
    }
    async fn cancel_external_agent(&self, _: String, _: String) -> Result<(), String> {
        unsupported()
    }
    async fn set_permission_mode(&self, _: String, _: String) -> Result<(), String> {
        Ok(())
    }
    async fn set_session_web_enabled(
        &self,
        _: String,
        _: bool,
    ) -> Result<AgentSessionSummary, String> {
        unsupported()
    }
    async fn context_usage(
        &self,
        _: String,
        _: Option<String>,
    ) -> Result<Option<ContextUsage>, String> {
        Ok(Some(ContextUsage {
            tokens: 10,
            limit: 100,
        }))
    }
    async fn read_image_attachment(&self, _: String, id: String) -> Result<Vec<u8>, String> {
        if id == "missing" {
            return Err("no such attachment".to_string());
        }
        Ok(self.attachment.clone())
    }
    async fn send_message(&self, request: AgentSendMessageRequest) -> Result<String, String> {
        let run = format!("run-for-{}", request.session_id);
        self.sent.lock().unwrap().push(request);
        Ok(run)
    }
    async fn cancel_run(&self, _: String) -> Result<(), String> {
        Ok(())
    }
    async fn cancel_queued_message(
        &self,
        _: String,
        _: String,
    ) -> Result<AgentDesktopQueueSnapshot, String> {
        unsupported()
    }
    async fn begin_queued_message_edit(&self, _: String, _: String) -> Result<(), String> {
        unsupported()
    }
    async fn end_queued_message_edit(&self, _: String, _: String) -> Result<(), String> {
        unsupported()
    }
    async fn answer_question(&self, _: String, _: String) -> Result<bool, String> {
        Ok(true)
    }
    async fn permission_respond(&self, _: String, _: String, _: bool) -> Result<(), String> {
        Ok(())
    }
    async fn ask_side_question(
        &self,
        _: String,
        _: String,
        _: Vec<SideQuestionTurn>,
        _: String,
    ) -> Result<(), String> {
        unsupported()
    }
    async fn summarize_tool_call(
        &self,
        _: String,
        _: String,
        _: Option<serde_json::Value>,
        _: String,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn summarize_thinking(&self, _: String, _: String) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn tool_summaries(&self, _: String) -> Result<HashMap<String, String>, String> {
        Ok(HashMap::from([(
            "item-1".to_string(),
            "did a thing".to_string(),
        )]))
    }
    async fn store_tool_summary(&self, _: String, _: String, _: String) -> Result<(), String> {
        Ok(())
    }
    async fn available_model_ids(&self) -> Result<Vec<String>, String> {
        Ok(vec!["m1".to_string()])
    }
    async fn model_supports_vision(&self, _: String) -> Result<Option<bool>, String> {
        Ok(Some(true))
    }
    async fn list_slash_commands(
        &self,
        _: Option<String>,
    ) -> Result<Vec<AgentSlashCommand>, String> {
        Ok(Vec::new())
    }
    async fn resolve_slash_command(
        &self,
        _: Option<String>,
        _: String,
        _: String,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn list_session_mcp_servers(
        &self,
        _: String,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        Ok(Vec::new())
    }
    async fn set_session_mcp_server_enabled(
        &self,
        _: String,
        _: String,
        _: AgentSessionIntegrationKind,
        _: bool,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        unsupported()
    }
    async fn list_mcp_servers(&self) -> Result<Vec<AgentMcpServer>, String> {
        Ok(Vec::new())
    }
    async fn save_mcp_servers(
        &self,
        _: Vec<AgentMcpServer>,
    ) -> Result<Vec<AgentMcpServer>, String> {
        unsupported()
    }
    async fn list_integrations(&self) -> Result<Vec<AgentIntegration>, String> {
        Ok(Vec::new())
    }
    async fn set_integration_enabled(
        &self,
        _: String,
        _: bool,
    ) -> Result<Vec<AgentIntegration>, String> {
        unsupported()
    }
    async fn setup_integration(&self, _: String) -> Result<Vec<AgentIntegration>, String> {
        unsupported()
    }
    async fn session_defaults(&self) -> Result<HostSessionDefaults, String> {
        Ok(HostSessionDefaults::default())
    }
    async fn set_session_defaults(&self, _: HostSessionDefaults) -> Result<(), String> {
        Ok(())
    }
    async fn save_default_model(&self, _: String) -> Result<(), String> {
        Ok(())
    }
    async fn usage_summary(&self) -> Result<UsageSummary, String> {
        Ok(UsageSummary::default())
    }
}

pub fn identity() -> HostIdentity {
    HostIdentity {
        app_version: "0.1.0".to_string(),
        build: Some("abc1234".to_string()),
        pcr_environment: "Development".to_string(),
    }
}

pub fn info() -> HostInfo {
    HostInfo {
        id: "host-key".to_string(),
        name: "workstation".to_string(),
        user_id: None,
    }
}

pub fn hello() -> ClientHello {
    ClientHello {
        protocol: PROTOCOL_VERSION,
        app_version: "0.1.0".to_string(),
        build: None,
        pcr_environment: "Development".to_string(),
        features: maple_remote::wire::features(),
        device: DeviceInfo {
            public_key: "device-key".to_string(),
            name: "laptop".to_string(),
            user_id: None,
        },
    }
}

pub fn client_config() -> ClientConfig {
    ClientConfig {
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        long_request_timeout: Duration::from_secs(5),
        ping_interval: Duration::from_millis(50),
        ping_timeout: Duration::from_millis(200),
        ping_misses: 2,
        timeline_page_items: 50,
        ..Default::default()
    }
}
