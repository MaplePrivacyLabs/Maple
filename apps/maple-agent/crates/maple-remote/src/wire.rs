//! The methods a client calls on a host, grouped by domain, and the
//! handshake both sides exchange first.
//!
//! Every request enum is tagged by the JSON-RPC method name with the
//! variant's fields as `params`, so `{ "method": "session.list", "params":
//! {...} }` decodes into `SessionRequest::List {...}`. A method without
//! params is a unit variant and carries no `params`. The server dispatches
//! by the prefix before the dot; each domain has its own controller.
//!
//! Compatibility: append-only. New params are `Option` with a default;
//! unknown fields are ignored on both sides. See the crate docs.

use std::collections::BTreeMap;

use maple_agent::agent::{
    AgentCreateSessionRequest, AgentMcpServer, AgentSendMessageRequest, AgentSessionDetail,
    AgentSessionIntegrationKind, AgentStartRequest, AgentTaskState, AgentTimelineItem,
    SideQuestionTurn,
};
use maple_agent::host::{HostBootstrap, HostEvent, HostSessionDefaults};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped only for a change no feature flag can express. Mismatch refuses
/// the connection.
pub const PROTOCOL_VERSION: u32 = 1;

/// Feature flags a side advertises. Absent means off.
pub type Features = BTreeMap<String, bool>;

/// The client streams images ahead of `run.send` and names them by upload
/// id; the host refuses images inline in the request. See
/// [`crate::uploads`].
pub const UPLOAD_STREAMS_FEATURE: &str = "uploadStreams";

/// Features this build implements. Both sides send the same table; a
/// client gates a new call on the host's answer.
pub fn features() -> Features {
    let mut features = Features::new();
    for name in [
        "timelinePaging",
        "attachmentStreams",
        "ping",
        UPLOAD_STREAMS_FEATURE,
    ] {
        features.insert(name.to_string(), true);
    }
    features
}

/// Whether `features` advertises `name`.
pub fn has_feature(features: &Features, name: &str) -> bool {
    features.get(name).copied().unwrap_or(false)
}

/// A peer's version for display: `0.1.0 (63bcff5c)`, or just `0.1.0`
/// when the peer sent no build.
pub fn version_label(version: &str, build: Option<&str>) -> String {
    match build {
        Some(build) => format!("{version} ({build})"),
        None => version.to_string(),
    }
}

/// The notification method that carries host events.
pub const EVENT_METHOD: &str = "event";

/// First request on a connection. Everything else is refused before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientHello {
    pub protocol: u32,
    pub app_version: String,
    /// The git revision the client was built from, when its build knew
    /// it. Absent from an older client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// The compile-time OpenSecret environment (`Production` or
    /// `Development`). Two binaries built for different enclaves cannot
    /// share a backend, so a mismatch is refused.
    pub pcr_environment: String,
    #[serde(default)]
    pub features: Features,
    pub device: DeviceInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    /// The device's static public key, or a placeholder before pairing
    /// exists.
    pub public_key: String,
    /// Display name, for the host's device list.
    pub name: String,
    /// The account the client is signed in as, for display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// The host's answer to [`ClientHello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostHello {
    pub protocol: u32,
    pub app_version: String,
    /// The git revision the host was built from, when its build knew it.
    /// Absent from an older host. Shown beside the version so two builds
    /// of one version can be told apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    pub pcr_environment: String,
    /// Minted when the host process started; every sequence is scoped to
    /// it. A different value after a reconnect means every cursor is
    /// stale.
    pub generation: String,
    /// The connection's event sequence starts after this value.
    pub seq: u64,
    #[serde(default)]
    pub features: Features,
    pub host: HostInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInfo {
    /// The host's identity: its static public key once pairing exists.
    pub id: String,
    pub name: String,
    /// The account the host is signed in as, for display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// One host event as delivered over a connection. `seq` increases by one
/// per event on that connection; a gap means events were lost and the
/// client must resync.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub seq: u64,
    pub event: HostEvent,
}

/// A task snapshot as sent over the wire. The timeline is paged separately
/// (see [`SessionRequest::Timeline`]) so one message never carries a whole
/// long transcript; `detail.timeline` is empty here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub detail: AgentSessionDetail,
    /// Timeline items in the snapshot the pages read from.
    pub timeline_len: usize,
}

/// One page of a snapshot's timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelinePage {
    pub items: Vec<AgentTimelineItem>,
    /// More items follow this page; fetch again from `offset + items.len()`.
    pub has_more: bool,
}

/// A bootstrap as sent over the wire: like [`HostBootstrap`] but with the
/// newest task's timeline paged separately.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapSnapshot {
    pub bootstrap: HostBootstrap,
    /// Timeline length of `bootstrap.latest`, whose timeline is empty here.
    pub latest_timeline_len: usize,
}

/// Where the bytes of an attachment arrive: on a binary stream the host
/// opened just before answering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentHandle {
    pub stream: u16,
    pub len: u64,
}

/// An image the client streamed to the host before `run.send`, named by
/// the id it minted for the upload stream. The media type travelled with
/// the stream; the name is what the message shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadRef {
    pub upload_id: String,
    pub name: String,
}

// ---- Domain requests --------------------------------------------------------
//
// Each variant is one method. A variant with fields carries them as the
// `params` object, in camelCase; a unit variant has no `params`.

/// `host.*`: the connection, the runtime, and host configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all_fields = "camelCase")]
pub enum HostRequest {
    #[serde(rename = "host.hello")]
    Hello(ClientHello),
    #[serde(rename = "host.ping")]
    Ping,
    #[serde(rename = "host.bootstrap")]
    Bootstrap,
    #[serde(rename = "host.start_runtime")]
    StartRuntime { request: Option<AgentStartRequest> },
    #[serde(rename = "host.stop_runtime")]
    StopRuntime,
    #[serde(rename = "host.session_defaults")]
    SessionDefaults,
    #[serde(rename = "host.set_session_defaults")]
    SetSessionDefaults { defaults: HostSessionDefaults },
    #[serde(rename = "host.save_default_model")]
    SaveDefaultModel { model: String },
    #[serde(rename = "host.usage_summary")]
    UsageSummary,
    #[serde(rename = "host.context_usage")]
    ContextUsage {
        session_id: String,
        model: Option<String>,
    },
    #[serde(rename = "host.tool_summaries")]
    ToolSummaries { session_id: String },
    #[serde(rename = "host.store_tool_summary")]
    StoreToolSummary {
        session_id: String,
        item_id: String,
        summary: String,
    },
}

/// `project.*`: roots on the host's filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all_fields = "camelCase")]
pub enum ProjectRequest {
    #[serde(rename = "project.recent_roots")]
    RecentRoots,
    #[serde(rename = "project.select_root")]
    SelectRoot { path: String },
    #[serde(rename = "project.remove_root")]
    RemoveRoot {
        path: String,
        fallback: Option<String>,
    },
    #[serde(rename = "project.suggest_directories")]
    SuggestDirectories { query: String },
    #[serde(rename = "project.watch")]
    Watch { path: String },
    #[serde(rename = "project.unwatch")]
    Unwatch { path: String },
    #[serde(rename = "project.trust")]
    Trust { path: String },
    #[serde(rename = "project.set_trust")]
    SetTrust { path: String, trusted: bool },
}

/// `session.*`: tasks and their snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all_fields = "camelCase")]
pub enum SessionRequest {
    #[serde(rename = "session.list")]
    List { project_root: Option<String> },
    #[serde(rename = "session.create")]
    Create {
        request: Option<AgentCreateSessionRequest>,
    },
    /// Returns a [`SessionSnapshot`]; page its timeline with `Timeline`.
    #[serde(rename = "session.load")]
    Load { session_id: String },
    /// One page of the snapshot the last `Load` (or the bootstrap) built for
    /// this task on this connection. Returns a [`TimelinePage`].
    #[serde(rename = "session.timeline")]
    Timeline {
        session_id: String,
        offset: usize,
        limit: usize,
    },
    #[serde(rename = "session.rename")]
    Rename { session_id: String, title: String },
    #[serde(rename = "session.set_state")]
    SetState {
        session_id: String,
        state: AgentTaskState,
    },
    #[serde(rename = "session.delete")]
    Delete { session_id: String },
    #[serde(rename = "session.compact")]
    Compact { session_id: String },
    #[serde(rename = "session.subagents")]
    Subagents { session_id: String },
    #[serde(rename = "session.cancel_external_agent")]
    CancelExternalAgent {
        session_id: String,
        agent_id: String,
    },
    #[serde(rename = "session.set_permission_mode")]
    SetPermissionMode { session_id: String, mode: String },
    #[serde(rename = "session.set_web_enabled")]
    SetWebEnabled { session_id: String, enabled: bool },
    /// Returns an [`AttachmentHandle`]; the bytes arrive on its stream.
    #[serde(rename = "session.read_attachment")]
    ReadAttachment {
        session_id: String,
        attachment_id: String,
    },
}

/// `run.*`: messages, runs, and the prompts they raise.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all_fields = "camelCase")]
pub enum RunRequest {
    /// `request.attachments` must be empty on the wire: images are
    /// streamed first and named in `uploads`. The host rebuilds the
    /// request's attachments from them before it sends the message.
    #[serde(rename = "run.send")]
    Send {
        request: AgentSendMessageRequest,
        #[serde(default)]
        uploads: Vec<UploadRef>,
    },
    #[serde(rename = "run.cancel")]
    Cancel { run_id: String },
    #[serde(rename = "run.cancel_queued")]
    CancelQueued {
        session_id: String,
        queue_id: String,
    },
    #[serde(rename = "run.begin_queued_edit")]
    BeginQueuedEdit {
        session_id: String,
        queue_id: String,
    },
    #[serde(rename = "run.end_queued_edit")]
    EndQueuedEdit {
        session_id: String,
        queue_id: String,
    },
    #[serde(rename = "run.answer_question")]
    AnswerQuestion { request_id: String, answer: String },
    #[serde(rename = "run.permission_respond")]
    PermissionRespond {
        session_id: String,
        request_id: String,
        allow: bool,
    },
    #[serde(rename = "run.ask_side_question")]
    AskSideQuestion {
        session_id: String,
        request_id: String,
        prior: Vec<SideQuestionTurn>,
        question: String,
    },
    #[serde(rename = "run.summarize_tool_call")]
    SummarizeToolCall {
        session_id: String,
        tool_name: String,
        input: Option<Value>,
        output_text: String,
    },
    #[serde(rename = "run.summarize_thinking")]
    SummarizeThinking {
        session_id: String,
        thinking_text: String,
    },
}

/// `model.*`: the catalog and skills.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all_fields = "camelCase")]
pub enum ModelRequest {
    #[serde(rename = "model.list")]
    List,
    #[serde(rename = "model.supports_vision")]
    SupportsVision { model: String },
    #[serde(rename = "model.slash_commands")]
    SlashCommands { working_dir: Option<String> },
    #[serde(rename = "model.resolve_slash_command")]
    ResolveSlashCommand {
        working_dir: Option<String>,
        command: String,
        args: String,
    },
}

/// `integration.*`: MCP servers and curated integrations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all_fields = "camelCase")]
pub enum IntegrationRequest {
    #[serde(rename = "integration.list_session_mcp")]
    ListSessionMcp { session_id: String },
    #[serde(rename = "integration.set_session_mcp")]
    SetSessionMcp {
        session_id: String,
        name: String,
        kind: AgentSessionIntegrationKind,
        enabled: bool,
    },
    #[serde(rename = "integration.list_mcp")]
    ListMcp,
    #[serde(rename = "integration.save_mcp")]
    SaveMcp { servers: Vec<AgentMcpServer> },
    #[serde(rename = "integration.list")]
    List,
    #[serde(rename = "integration.set_enabled")]
    SetEnabled { id: String, enabled: bool },
}

/// A request decoded into its domain.
#[derive(Debug, Clone)]
pub enum Request {
    Host(HostRequest),
    Project(ProjectRequest),
    Session(SessionRequest),
    Run(RunRequest),
    Model(ModelRequest),
    Integration(IntegrationRequest),
}

/// Outcome of decoding a method name and its params.
#[derive(Debug)]
pub enum DecodeError {
    /// No domain owns this method.
    UnknownMethod(String),
    /// The domain knows the method but the params do not fit.
    InvalidParams(String),
}

/// Every method name, by domain, in the order of the enum variants. The
/// test below checks each list against the variants serde knows, so a new
/// variant fails a test until it is listed here.
pub const HOST_METHODS: &[&str] = &[
    "host.hello",
    "host.ping",
    "host.bootstrap",
    "host.start_runtime",
    "host.stop_runtime",
    "host.session_defaults",
    "host.set_session_defaults",
    "host.save_default_model",
    "host.usage_summary",
    "host.context_usage",
    "host.tool_summaries",
    "host.store_tool_summary",
];
pub const PROJECT_METHODS: &[&str] = &[
    "project.recent_roots",
    "project.select_root",
    "project.remove_root",
    "project.suggest_directories",
    "project.watch",
    "project.unwatch",
    "project.trust",
    "project.set_trust",
];
pub const SESSION_METHODS: &[&str] = &[
    "session.list",
    "session.create",
    "session.load",
    "session.timeline",
    "session.rename",
    "session.set_state",
    "session.delete",
    "session.compact",
    "session.subagents",
    "session.cancel_external_agent",
    "session.set_permission_mode",
    "session.set_web_enabled",
    "session.read_attachment",
];
pub const RUN_METHODS: &[&str] = &[
    "run.send",
    "run.cancel",
    "run.cancel_queued",
    "run.begin_queued_edit",
    "run.end_queued_edit",
    "run.answer_question",
    "run.permission_respond",
    "run.ask_side_question",
    "run.summarize_tool_call",
    "run.summarize_thinking",
];
pub const MODEL_METHODS: &[&str] = &[
    "model.list",
    "model.supports_vision",
    "model.slash_commands",
    "model.resolve_slash_command",
];
pub const INTEGRATION_METHODS: &[&str] = &[
    "integration.list_session_mcp",
    "integration.set_session_mcp",
    "integration.list_mcp",
    "integration.save_mcp",
    "integration.list",
    "integration.set_enabled",
];

/// Whether a host answers `method` at all. Deciding this by name, rather
/// than by reading serde's error text, keeps a bad enum value inside the
/// params from reading as an unknown method.
pub fn is_known_method(method: &str) -> bool {
    [
        HOST_METHODS,
        PROJECT_METHODS,
        SESSION_METHODS,
        RUN_METHODS,
        MODEL_METHODS,
        INTEGRATION_METHODS,
    ]
    .iter()
    .any(|methods| methods.contains(&method))
}

/// Decode a JSON-RPC request by its method's domain prefix.
///
/// A method without params is a unit variant, which serde accepts with
/// `params` missing or `null` but not `{}`; a method whose params are all
/// optional is a struct variant, which needs `{}`. A missing, `null`, or
/// empty `params` is tried both ways so either client shape decodes.
pub fn decode_request(method: &str, params: Value) -> Result<Request, DecodeError> {
    if !is_known_method(method) {
        return Err(DecodeError::UnknownMethod(method.to_string()));
    }
    let empty = params.is_null() || params.as_object().is_some_and(|object| object.is_empty());
    if !empty {
        return decode_tagged(
            method,
            serde_json::json!({ "method": method, "params": params }),
        );
    }
    decode_tagged(
        method,
        serde_json::json!({ "method": method, "params": {} }),
    )
    .or_else(|first| {
        decode_tagged(method, serde_json::json!({ "method": method })).map_err(|_| first)
    })
}

fn decode_tagged(method: &str, tagged: Value) -> Result<Request, DecodeError> {
    fn decode<T: serde::de::DeserializeOwned>(tagged: Value) -> Result<T, DecodeError> {
        serde_json::from_value(tagged)
            .map_err(|error| DecodeError::InvalidParams(error.to_string()))
    }
    let (domain, _) = method
        .split_once('.')
        .ok_or_else(|| DecodeError::UnknownMethod(method.to_string()))?;
    Ok(match domain {
        "host" => Request::Host(decode(tagged)?),
        "project" => Request::Project(decode(tagged)?),
        "session" => Request::Session(decode(tagged)?),
        "run" => Request::Run(decode(tagged)?),
        "model" => Request::Model(decode(tagged)?),
        "integration" => Request::Integration(decode(tagged)?),
        _ => return Err(DecodeError::UnknownMethod(method.to_string())),
    })
}

/// The method name and params of a typed request, for the client side. A
/// unit variant has no params and yields `Value::Null`, which the request
/// omits.
pub fn encode_request<T: Serialize>(request: &T) -> Result<(String, Value), String> {
    let value = serde_json::to_value(request).map_err(|error| error.to_string())?;
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| "request has no method".to_string())?
        .to_string();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    Ok((method, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_decode_by_domain_and_refuse_unknown_methods() {
        let (method, params) = encode_request(&SessionRequest::List {
            project_root: Some("/p".to_string()),
        })
        .unwrap();
        assert_eq!(method, "session.list");
        match decode_request(&method, params).unwrap() {
            Request::Session(SessionRequest::List { project_root }) => {
                assert_eq!(project_root.as_deref(), Some("/p"));
            }
            other => panic!("wrong decode: {other:?}"),
        }
        assert!(matches!(
            decode_request("session.nope", Value::Null),
            Err(DecodeError::UnknownMethod(_))
        ));
        assert!(matches!(
            decode_request("nodot", Value::Null),
            Err(DecodeError::UnknownMethod(_))
        ));
        assert!(matches!(
            decode_request("run.cancel", serde_json::json!({"wrong": 1})),
            Err(DecodeError::InvalidParams(_))
        ));
        // A bad enum value inside the params is a params error, not an
        // unknown method, even though serde reports it as a variant.
        let bad_kind = serde_json::json!({
            "sessionId": "s1", "name": "n", "kind": "teleporter", "enabled": true
        });
        match decode_request("integration.set_session_mcp", bad_kind) {
            Err(DecodeError::InvalidParams(text)) => {
                assert!(text.contains("teleporter"), "{text}");
            }
            other => panic!("wrong decode: {other:?}"),
        }
    }

    #[test]
    fn params_keep_their_wire_shape_and_empty_params_decode_both_ways() {
        // A method with params: the same JSON as before the variants held
        // their own structs.
        let (method, params) = encode_request(&SessionRequest::Rename {
            session_id: "s1".to_string(),
            title: "T".to_string(),
        })
        .unwrap();
        assert_eq!(method, "session.rename");
        assert_eq!(params, serde_json::json!({"sessionId": "s1", "title": "T"}));
        // A method without params has none.
        let (method, params) = encode_request(&HostRequest::Ping).unwrap();
        assert_eq!(method, "host.ping");
        assert_eq!(params, Value::Null);
        for params in [Value::Null, serde_json::json!({})] {
            assert!(matches!(
                decode_request("host.ping", params.clone()),
                Ok(Request::Host(HostRequest::Ping))
            ));
            // All-optional params decode from nothing as well.
            assert!(matches!(
                decode_request("session.list", params),
                Ok(Request::Session(SessionRequest::List {
                    project_root: None
                }))
            ));
        }
        assert!(matches!(
            decode_request("host.ping", serde_json::json!({"extra": 1})),
            Err(DecodeError::InvalidParams(_))
        ));
        assert!(matches!(
            decode_request("session.rename", Value::Null),
            Err(DecodeError::InvalidParams(_))
        ));
    }

    /// The names serde accepts as tags for `T`, read from its error for a
    /// tag it does not know.
    fn serde_variants<T: serde::de::DeserializeOwned + std::fmt::Debug>(
        domain: &str,
    ) -> Vec<String> {
        let probe = serde_json::json!({ "method": format!("{domain}.__probe__"), "params": {} });
        let text = serde_json::from_value::<T>(probe).unwrap_err().to_string();
        let (_, expected) = text
            .split_once("expected one of ")
            .unwrap_or_else(|| panic!("no variant list in {text:?}"));
        expected
            .split(", ")
            .map(|name| name.trim().trim_matches('`').to_string())
            .filter(|name| !name.is_empty())
            .collect()
    }

    #[test]
    fn the_method_lists_match_the_enums() {
        assert_eq!(serde_variants::<HostRequest>("host"), HOST_METHODS);
        assert_eq!(serde_variants::<ProjectRequest>("project"), PROJECT_METHODS);
        assert_eq!(serde_variants::<SessionRequest>("session"), SESSION_METHODS);
        assert_eq!(serde_variants::<RunRequest>("run"), RUN_METHODS);
        assert_eq!(serde_variants::<ModelRequest>("model"), MODEL_METHODS);
        assert_eq!(
            serde_variants::<IntegrationRequest>("integration"),
            INTEGRATION_METHODS
        );
        for method in HOST_METHODS.iter().chain(SESSION_METHODS) {
            assert!(is_known_method(method));
            assert!(method.starts_with("host.") || method.starts_with("session."));
        }
        assert!(!is_known_method("host.__probe__"));
    }

    #[test]
    fn unknown_fields_are_ignored_and_optional_fields_default() {
        let params = serde_json::json!({
            "protocol": 1,
            "appVersion": "0.1.0",
            "pcrEnvironment": "Production",
            "device": {"publicKey": "k", "name": "laptop", "futureField": true},
            "somethingNew": {"nested": 1}
        });
        let hello: ClientHello = serde_json::from_value(params).unwrap();
        assert!(hello.features.is_empty());
        assert_eq!(hello.device.user_id, None);
        assert_eq!(hello.build, None);
        let json = serde_json::to_value(&hello).unwrap();
        assert!(json.get("somethingNew").is_none());
        assert!(json.get("build").is_none(), "an absent build is not sent");
    }

    #[test]
    fn hellos_round_trip_with_and_without_a_build() {
        let mut client = ClientHello {
            protocol: PROTOCOL_VERSION,
            app_version: "0.1.0".to_string(),
            build: Some("63bcff5c".to_string()),
            pcr_environment: "Production".to_string(),
            features: features(),
            device: DeviceInfo {
                public_key: "k".to_string(),
                name: "laptop".to_string(),
                user_id: None,
            },
        };
        let json = serde_json::to_value(&client).unwrap();
        assert_eq!(json["build"], "63bcff5c");
        assert_eq!(serde_json::from_value::<ClientHello>(json).unwrap(), client);
        client.build = None;
        let json = serde_json::to_value(&client).unwrap();
        assert!(json.get("build").is_none());
        assert_eq!(serde_json::from_value::<ClientHello>(json).unwrap(), client);

        let mut host = HostHello {
            protocol: PROTOCOL_VERSION,
            app_version: "0.1.0".to_string(),
            build: Some("63bcff5c".to_string()),
            pcr_environment: "Production".to_string(),
            generation: "gen".to_string(),
            seq: 0,
            features: features(),
            host: HostInfo {
                id: "h".to_string(),
                name: "workstation".to_string(),
                user_id: None,
            },
        };
        let json = serde_json::to_value(&host).unwrap();
        assert_eq!(json["build"], "63bcff5c");
        assert_eq!(serde_json::from_value::<HostHello>(json).unwrap(), host);
        host.build = None;
        let json = serde_json::to_value(&host).unwrap();
        assert!(json.get("build").is_none());
        assert_eq!(serde_json::from_value::<HostHello>(json).unwrap(), host);

        // What an older host sends: no `build` at all.
        let older: HostHello = serde_json::from_value(serde_json::json!({
            "protocol": 1,
            "appVersion": "0.1.0",
            "pcrEnvironment": "Production",
            "generation": "gen",
            "seq": 0,
            "host": {"id": "h", "name": "workstation"}
        }))
        .unwrap();
        assert_eq!(older.build, None);
        assert_eq!(
            version_label(&older.app_version, older.build.as_deref()),
            "0.1.0"
        );
        assert_eq!(version_label("0.1.0", Some("63bcff5c")), "0.1.0 (63bcff5c)");
    }

    #[test]
    fn features_table_has_this_builds_flags() {
        assert_eq!(features().get("timelinePaging"), Some(&true));
        assert!(has_feature(&features(), UPLOAD_STREAMS_FEATURE));
        assert!(!has_feature(&Features::new(), UPLOAD_STREAMS_FEATURE));
    }

    #[test]
    fn a_send_without_uploads_decodes_from_an_older_client() {
        let params = serde_json::json!({
            "request": {"sessionId": "s1", "text": "hi", "model": null, "mode": null}
        });
        match decode_request("run.send", params).unwrap() {
            Request::Run(RunRequest::Send { request, uploads }) => {
                assert_eq!(request.session_id, "s1");
                assert!(uploads.is_empty());
            }
            other => panic!("wrong decode: {other:?}"),
        }
    }
}
