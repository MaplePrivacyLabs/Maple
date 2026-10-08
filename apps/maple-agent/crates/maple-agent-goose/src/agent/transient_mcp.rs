//! Lease-scoped MCP clients supplied by an external Agent surface.
//!
//! These clients intentionally live outside Goose's `ExtensionManager`. In
//! particular, they must never enter Goose's extension persistence,
//! credential-store, or OAuth paths. The owning Agent-surface lease supplies a
//! cancellation token; revoking it fails in-flight calls closed and tears down
//! the transport.

use goose::agents::ToolCallContext;
use goose::agents::mcp_client::{Error as McpError, McpClientTrait};
use goose::session_context::{SESSION_ID_HEADER, TOOL_CALL_REQUEST_ID_HEADER, WORKING_DIR_HEADER};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, CancelledNotificationParam, ClientRequest, Extensions,
    GetPromptRequestParams, GetPromptResult, InitializeResult, JsonObject, ListPromptsResult,
    ListResourcesResult, ListToolsResult, MetaObject, Notification, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResult, Request, RequestOptionalParam, ServerResult,
    Tool,
};
use rmcp::service::{PeerRequestOptions, RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{Peer, ServiceError, ServiceExt};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

const TRANSIENT_MCP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSIENT_MCP_CANCEL_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const TRANSIENT_MCP_MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;
const TRANSIENT_MCP_MAX_CATALOG_PAGES: usize = 16;
const TRANSIENT_MCP_MAX_TOOLS_PER_SERVER: usize = 256;
const TRANSIENT_MCP_MAX_TOOLS_TOTAL: usize = 1024;
const TRANSIENT_MCP_MAX_TOOL_BYTES: usize = 256 * 1024;
const TRANSIENT_MCP_MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const TRANSIENT_MCP_MAX_PUBLIC_TOOL_NAME_BYTES: usize = 64;
const TRANSIENT_MCP_MAX_TOOL_RESULT_BYTES: usize = 4 * 1024 * 1024;

/// Complete, already-admitted configuration for one transient MCP server.
///
/// Validation at the ACP boundary remains authoritative. The checks here are
/// defense in depth at the network authority boundary.
pub(crate) struct TransientMcpConfig {
    pub(crate) name: String,
    pub(crate) session_id: String,
    pub(crate) request_timeout: Duration,
    pub(crate) url: String,
    pub(crate) headers: Vec<(String, String)>,
}

/// A cloneable, secret-free descriptor used internally by the frozen router.
///
/// The credential-bearing HTTP client is reachable only through `client` and
/// is dropped when the owning lease releases its final descriptor.
#[derive(Clone)]
struct TransientMcpDescriptor {
    key: Arc<str>,
    client: Arc<TransientMcpClient>,
}

impl fmt::Debug for TransientMcpDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransientMcpDescriptor")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl TransientMcpDescriptor {
    async fn connect(
        mut config: TransientMcpConfig,
        lease_cancel: CancellationToken,
    ) -> Result<Self, TransientMcpConnectError> {
        config.name = config.name.trim().to_owned();
        config.session_id = config.session_id.trim().to_owned();
        if config.name.is_empty() {
            return Err(TransientMcpConnectError::InvalidConfiguration(
                "MCP server name is empty",
            ));
        }
        if config.request_timeout.is_zero() {
            return Err(TransientMcpConnectError::InvalidConfiguration(
                "MCP request timeout is zero",
            ));
        }
        if config.session_id.is_empty() {
            return Err(TransientMcpConnectError::InvalidConfiguration(
                "MCP session ID is empty",
            ));
        }
        let key = goose::config::extensions::name_to_key(&config.name);
        if !valid_server_key(&key) {
            return Err(TransientMcpConnectError::InvalidConfiguration(
                "MCP server name has no usable key",
            ));
        }

        let client = TransientMcpClient::connect_streamable_http(
            config.url,
            config.headers,
            config.request_timeout,
            lease_cancel,
        )
        .await?;

        Ok(Self {
            key: key.into(),
            client,
        })
    }

    fn key(&self) -> &str {
        &self.key
    }

    async fn list_tools(
        &self,
        session_id: &str,
        next_cursor: Option<String>,
        cancel_token: CancellationToken,
    ) -> Result<ListToolsResult, McpError> {
        self.client
            .list_tools(session_id, next_cursor, cancel_token)
            .await
    }

    async fn call_tool(
        &self,
        context: &ToolCallContext,
        name: &str,
        arguments: Option<JsonObject>,
        cancel_token: CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        self.client
            .call_tool(context, name, arguments, cancel_token)
            .await
    }

    async fn shutdown(&self) {
        self.client.shutdown().await;
    }
}

#[derive(Clone)]
struct TransientMcpRoute {
    descriptor: TransientMcpDescriptor,
    original_name: Arc<str>,
}

struct TransientMcpRouterInner {
    tools: Arc<[Tool]>,
    routes: HashMap<String, TransientMcpRoute>,
    descriptors: Arc<[TransientMcpDescriptor]>,
}

/// Immutable, lease-scoped routing table for transient MCP tools.
///
/// Tool discovery happens exactly once while the router is constructed. A
/// public tool name is always `<normalized-server-key>__<original-tool-name>`;
/// later server catalog changes cannot retarget a model-visible name to a
/// different client or operation.
#[derive(Clone)]
pub(crate) struct TransientMcpRouter {
    inner: Arc<TransientMcpRouterInner>,
}

impl fmt::Debug for TransientMcpRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransientMcpRouter")
            .field("servers", &self.inner.descriptors.len())
            .field("tools", &self.inner.tools.len())
            .finish()
    }
}

impl TransientMcpRouter {
    pub(crate) async fn connect(
        configs: Vec<TransientMcpConfig>,
        lease_cancel: CancellationToken,
    ) -> Result<Self, TransientMcpConnectError> {
        let mut server_keys = HashSet::with_capacity(configs.len());
        for config in &configs {
            let key = goose::config::extensions::name_to_key(config.name.trim());
            if !valid_server_key(&key) {
                return Err(TransientMcpConnectError::InvalidConfiguration(
                    "MCP server name has no usable key",
                ));
            }
            if !server_keys.insert(key) {
                return Err(TransientMcpConnectError::DuplicateServer);
            }
        }

        let mut descriptors = Vec::with_capacity(configs.len());
        let mut routes = HashMap::new();
        let mut public_tools = Vec::new();
        let mut catalog_bytes = 0usize;
        for config in configs {
            let session_id = config.session_id.clone();
            let descriptor =
                match TransientMcpDescriptor::connect(config, lease_cancel.clone()).await {
                    Ok(descriptor) => descriptor,
                    Err(error) => {
                        shutdown_descriptors(&descriptors).await;
                        return Err(error);
                    }
                };
            let discovered = match discover_tools(&descriptor, &session_id, &lease_cancel).await {
                Ok(tools) => tools,
                Err(error) => {
                    descriptor.shutdown().await;
                    shutdown_descriptors(&descriptors).await;
                    return Err(error);
                }
            };

            for (mut tool, original_name) in discovered {
                if public_tools.len() >= TRANSIENT_MCP_MAX_TOOLS_TOTAL {
                    descriptor.shutdown().await;
                    shutdown_descriptors(&descriptors).await;
                    return Err(TransientMcpConnectError::CatalogTooLarge);
                }
                let public_name = format!("{}__{}", descriptor.key(), original_name);
                if !valid_public_tool_name(&public_name) {
                    descriptor.shutdown().await;
                    shutdown_descriptors(&descriptors).await;
                    return Err(TransientMcpConnectError::InvalidCatalog);
                }
                if routes.contains_key(&public_name) {
                    descriptor.shutdown().await;
                    shutdown_descriptors(&descriptors).await;
                    return Err(TransientMcpConnectError::DuplicateTool);
                }

                sanitize_catalog_tool(&mut tool, &public_name);
                let tool_bytes = match serde_json::to_vec(&tool) {
                    Ok(tool) => tool.len(),
                    Err(_) => {
                        descriptor.shutdown().await;
                        shutdown_descriptors(&descriptors).await;
                        return Err(TransientMcpConnectError::InvalidCatalog);
                    }
                };
                if tool_bytes > TRANSIENT_MCP_MAX_TOOL_BYTES {
                    descriptor.shutdown().await;
                    shutdown_descriptors(&descriptors).await;
                    return Err(TransientMcpConnectError::CatalogTooLarge);
                }
                catalog_bytes = match catalog_bytes.checked_add(tool_bytes) {
                    Some(total) => total,
                    None => {
                        descriptor.shutdown().await;
                        shutdown_descriptors(&descriptors).await;
                        return Err(TransientMcpConnectError::CatalogTooLarge);
                    }
                };
                if catalog_bytes > TRANSIENT_MCP_MAX_CATALOG_BYTES {
                    descriptor.shutdown().await;
                    shutdown_descriptors(&descriptors).await;
                    return Err(TransientMcpConnectError::CatalogTooLarge);
                }

                routes.insert(
                    public_name,
                    TransientMcpRoute {
                        descriptor: descriptor.clone(),
                        original_name,
                    },
                );
                public_tools.push(tool);
            }
            descriptors.push(descriptor);
        }

        Ok(Self {
            inner: Arc::new(TransientMcpRouterInner {
                tools: public_tools.into(),
                routes,
                descriptors: descriptors.into(),
            }),
        })
    }

    pub(crate) fn tools(&self) -> &[Tool] {
        &self.inner.tools
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.inner.tools.is_empty()
    }

    pub(crate) async fn call(
        &self,
        public_name: &str,
        context: &ToolCallContext,
        arguments: Option<JsonObject>,
        cancel_token: CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let route = self
            .inner
            .routes
            .get(public_name)
            .ok_or(ServiceError::UnexpectedResponse)?;
        let mut result = route
            .descriptor
            .call_tool(context, &route.original_name, arguments, cancel_token)
            .await
            // Transport failures can contain a caller URL or malicious server
            // response. Keep those details out of model-visible errors/logs.
            .map_err(|_| ServiceError::UnexpectedResponse)?;
        sanitize_tool_result(&mut result);
        let result_bytes = serde_json::to_vec(&result)
            .map_err(|_| ServiceError::UnexpectedResponse)?
            .len();
        if result_bytes > TRANSIENT_MCP_MAX_TOOL_RESULT_BYTES {
            return Err(ServiceError::UnexpectedResponse);
        }
        Ok(result)
    }

    pub(crate) async fn shutdown(&self) {
        shutdown_descriptors(&self.inner.descriptors).await;
    }
}

async fn discover_tools(
    descriptor: &TransientMcpDescriptor,
    session_id: &str,
    lease_cancel: &CancellationToken,
) -> Result<Vec<(Tool, Arc<str>)>, TransientMcpConnectError> {
    let mut cursor = None;
    let mut seen_cursors = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut tools = Vec::new();
    for _ in 0..TRANSIENT_MCP_MAX_CATALOG_PAGES {
        let result = descriptor
            .list_tools(session_id, cursor, lease_cancel.child_token())
            .await
            .map_err(|_| TransientMcpConnectError::CatalogRequest)?;
        for tool in result.tools {
            let original_name = tool.name.trim();
            if !valid_original_tool_name(original_name)
                || !seen_names.insert(original_name.to_owned())
            {
                return Err(if !valid_original_tool_name(original_name) {
                    TransientMcpConnectError::InvalidCatalog
                } else {
                    TransientMcpConnectError::DuplicateTool
                });
            }
            if tools.len() >= TRANSIENT_MCP_MAX_TOOLS_PER_SERVER {
                return Err(TransientMcpConnectError::CatalogTooLarge);
            }
            let original_name: Arc<str> = original_name.to_owned().into();
            tools.push((tool, original_name));
        }
        let Some(next_cursor) = result.next_cursor else {
            return Ok(tools);
        };
        if next_cursor.is_empty() || !seen_cursors.insert(next_cursor.clone()) {
            return Err(TransientMcpConnectError::InvalidCatalog);
        }
        cursor = Some(next_cursor);
    }
    Err(TransientMcpConnectError::CatalogTooLarge)
}

fn valid_tool_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn valid_server_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= TRANSIENT_MCP_MAX_PUBLIC_TOOL_NAME_BYTES.saturating_sub(3)
        && key.bytes().all(valid_tool_name_byte)
}

fn valid_original_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= TRANSIENT_MCP_MAX_PUBLIC_TOOL_NAME_BYTES
        && name.bytes().all(valid_tool_name_byte)
}

fn valid_public_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= TRANSIENT_MCP_MAX_PUBLIC_TOOL_NAME_BYTES
        && name.bytes().all(valid_tool_name_byte)
}

fn sanitize_catalog_tool(tool: &mut Tool, public_name: &str) {
    // These descriptors are embedded only as variants of Maple's static
    // ask-before `external_mcp` wrapper. Server-supplied permission or UI hints
    // must not influence that host-owned boundary.
    tool.name = public_name.to_owned().into();
    tool.annotations = None;
    tool.meta = None;
    tool.icons = None;
}

fn sanitize_tool_result(result: &mut CallToolResult) {
    use rmcp::model::{ContentBlock, ResourceContents};

    // Protocol metadata and presentation annotations are untrusted server
    // hints. Maple only forwards the actual result payload and error bit.
    result.meta = None;
    result.content.retain_mut(|content| {
        match content {
            ContentBlock::Text(content) => {
                content.meta = None;
                content.annotations = None;
            }
            ContentBlock::Image(content) => {
                content.meta = None;
                content.annotations = None;
            }
            ContentBlock::Audio(content) => {
                content.meta = None;
                content.annotations = None;
            }
            ContentBlock::Resource(content) => {
                content.meta = None;
                content.annotations = None;
                match &mut content.resource {
                    ResourceContents::TextResourceContents { meta, .. }
                    | ResourceContents::BlobResourceContents { meta, .. } => *meta = None,
                    _ => return false,
                }
            }
            ContentBlock::ResourceLink(resource) => {
                resource.meta = None;
                resource.annotations = None;
                resource.icons = None;
            }
            // `ContentBlock` is non-exhaustive. With the exact rmcp pin all
            // current variants are handled above; fail closed if a future pin
            // adds a host-active content type.
            _ => return false,
        }
        true
    });
}

async fn shutdown_descriptors(descriptors: &[TransientMcpDescriptor]) {
    futures_util::future::join_all(descriptors.iter().map(TransientMcpDescriptor::shutdown)).await;
}

/// Safe, intentionally low-detail connection failures.
///
/// Source errors are not retained because transport errors can include a URL,
/// headers or other caller-owned configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransientMcpConnectError {
    InvalidConfiguration(&'static str),
    HttpClient,
    TimedOut,
    Cancelled,
    Initialize,
    DuplicateServer,
    CatalogRequest,
    InvalidCatalog,
    DuplicateTool,
    CatalogTooLarge,
}

impl fmt::Display for TransientMcpConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => formatter.write_str(message),
            Self::HttpClient => formatter.write_str("could not construct MCP HTTP client"),
            Self::TimedOut => formatter.write_str("MCP server initialization timed out"),
            Self::Cancelled => formatter.write_str("MCP server initialization was cancelled"),
            Self::Initialize => formatter.write_str("MCP server initialization failed"),
            Self::DuplicateServer => formatter.write_str("MCP server names conflict"),
            Self::CatalogRequest => formatter.write_str("MCP tool discovery failed"),
            Self::InvalidCatalog => {
                formatter.write_str("MCP server returned an invalid tool catalog")
            }
            Self::DuplicateTool => formatter.write_str("MCP tool names conflict"),
            Self::CatalogTooLarge => formatter.write_str("MCP tool catalog exceeds its limit"),
        }
    }
}

impl std::error::Error for TransientMcpConnectError {}

pub(crate) struct TransientMcpClient {
    peer: Peer<RoleClient>,
    server_info: Option<InitializeResult>,
    request_timeout: Duration,
    closed: CancellationToken,
    running: Mutex<Option<RunningService<RoleClient, ()>>>,
}

impl TransientMcpClient {
    async fn connect_streamable_http(
        url: String,
        headers: Vec<(String, String)>,
        request_timeout: Duration,
        lease_cancel: CancellationToken,
    ) -> Result<Arc<Self>, TransientMcpConnectError> {
        validate_http_url(&url)?;
        let headers = parse_http_headers(headers)?;
        let client = reqwest::Client::builder()
            // Ambient HTTP(S)_PROXY settings must never intercept a bearer
            // credential intended for a transient MCP endpoint.
            .no_proxy()
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(request_timeout)
            .timeout(request_timeout)
            .build()
            .map_err(|_| TransientMcpConnectError::HttpClient)?;
        let transport_config = StreamableHttpClientTransportConfig::with_uri(url)
            .custom_headers(headers)
            .max_sse_event_size(TRANSIENT_MCP_MAX_SSE_EVENT_BYTES)
            // Never replay an in-flight tool request after a session-expiry
            // response; callers must make retry decisions at a higher layer.
            .reinit_on_expired_session(false);
        let transport = StreamableHttpClientTransport::with_client(client, transport_config);
        let closed = lease_cancel.child_token();
        let connect = ().serve(transport);
        tokio::pin!(connect);
        let running = tokio::select! {
            biased;
            _ = closed.cancelled() => return Err(TransientMcpConnectError::Cancelled),
            result = &mut connect => result.map_err(|_| TransientMcpConnectError::Initialize)?,
            _ = tokio::time::sleep(request_timeout) => {
                return Err(TransientMcpConnectError::TimedOut);
            }
        };

        let peer = running.peer().clone();
        let server_info = running.peer_info().map(|info| {
            let mut result = InitializeResult::new(info.capabilities.clone())
                .with_protocol_version(info.protocol_version.clone());
            if let Some(implementation) = &info.server_info {
                result = result.with_server_info(implementation.clone());
            }
            result.instructions = info.instructions.clone();
            result.meta = info.meta.clone();
            result
        });
        let client = Arc::new(Self {
            peer,
            server_info,
            request_timeout,
            closed: closed.clone(),
            running: Mutex::new(Some(running)),
        });
        Self::watch_lease(&client, lease_cancel, closed);
        Ok(client)
    }

    fn watch_lease(client: &Arc<Self>, lease_cancel: CancellationToken, closed: CancellationToken) {
        let client = Arc::downgrade(client);
        tokio::spawn(async move {
            tokio::select! {
                _ = lease_cancel.cancelled() => {
                    if let Some(client) = client.upgrade() {
                        client.shutdown().await;
                    }
                }
                _ = closed.cancelled() => {}
            }
        });
    }

    pub(crate) async fn shutdown(&self) {
        self.closed.cancel();
        let running = self.running.lock().await.take();
        if let Some(running) = running {
            // Dropping the cancellation future on timeout also drops the
            // RunningService, whose guard cancels the transport.
            let _ = tokio::time::timeout(TRANSIENT_MCP_SHUTDOWN_TIMEOUT, running.cancel()).await;
        }
    }

    async fn send_request(
        &self,
        request: ClientRequest,
        cancel_token: CancellationToken,
    ) -> Result<ServerResult, ServiceError> {
        if self.closed.is_cancelled() || self.peer.is_transport_closed() {
            return Err(ServiceError::TransportClosed);
        }
        let enqueue = self
            .peer
            .send_cancellable_request(request, PeerRequestOptions::no_options());
        tokio::pin!(enqueue);
        let handle = tokio::select! {
            biased;
            _ = self.closed.cancelled() => return Err(ServiceError::TransportClosed),
            _ = cancel_token.cancelled() => {
                return Err(ServiceError::Cancelled { reason: None });
            }
            result = &mut enqueue => result?,
            _ = tokio::time::sleep(self.request_timeout) => {
                return Err(ServiceError::Timeout { timeout: self.request_timeout });
            }
        };
        let request_id = handle.id.clone();
        let peer = handle.peer.clone();
        let mut response = handle.rx;

        tokio::select! {
            biased;
            _ = self.closed.cancelled() => {
                send_cancel(&peer, request_id, "MCP lease revoked").await;
                Err(ServiceError::TransportClosed)
            }
            _ = cancel_token.cancelled() => {
                send_cancel(&peer, request_id, "operation cancelled").await;
                Err(ServiceError::Cancelled { reason: None })
            }
            result = &mut response => {
                result.map_err(|_| ServiceError::TransportClosed)?
            }
            _ = tokio::time::sleep(self.request_timeout) => {
                send_cancel(&peer, request_id, "operation timed out").await;
                Err(ServiceError::Timeout { timeout: self.request_timeout })
            }
        }
    }
}

impl Drop for TransientMcpClient {
    fn drop(&mut self) {
        self.closed.cancel();
        if let Ok(mut running) = self.running.try_lock() {
            // RunningService's drop guard cancels its transport.
            running.take();
        }
    }
}

#[async_trait::async_trait]
impl McpClientTrait for TransientMcpClient {
    async fn list_tools(
        &self,
        session_id: &str,
        next_cursor: Option<String>,
        cancel_token: CancellationToken,
    ) -> Result<ListToolsResult, McpError> {
        let response = self
            .send_request(
                with_session_context(
                    ClientRequest::ListToolsRequest(RequestOptionalParam::with_param(
                        PaginatedRequestParams::default().with_cursor(next_cursor),
                    )),
                    Some(session_id),
                    None,
                    None,
                ),
                cancel_token,
            )
            .await?;
        match response {
            ServerResult::ListToolsResult(result) => Ok(result),
            _ => Err(ServiceError::UnexpectedResponse),
        }
    }

    async fn call_tool(
        &self,
        context: &ToolCallContext,
        name: &str,
        arguments: Option<JsonObject>,
        cancel_token: CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let mut params = CallToolRequestParams::new(name.to_owned());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let response = self
            .send_request(
                with_session_context(
                    ClientRequest::CallToolRequest(Request::new(params)),
                    Some(&context.session_id),
                    context.working_dir_str(),
                    context.tool_call_request_id.as_deref(),
                ),
                cancel_token,
            )
            .await?;
        match response {
            ServerResult::CallToolResult(result) => Ok(result),
            _ => Err(ServiceError::UnexpectedResponse),
        }
    }

    fn get_info(&self) -> Option<&InitializeResult> {
        self.server_info.as_ref()
    }

    async fn list_resources(
        &self,
        session_id: &str,
        next_cursor: Option<String>,
        cancel_token: CancellationToken,
    ) -> Result<ListResourcesResult, McpError> {
        let response = self
            .send_request(
                with_session_context(
                    ClientRequest::ListResourcesRequest(RequestOptionalParam::with_param(
                        PaginatedRequestParams::default().with_cursor(next_cursor),
                    )),
                    Some(session_id),
                    None,
                    None,
                ),
                cancel_token,
            )
            .await?;
        match response {
            ServerResult::ListResourcesResult(result) => Ok(result),
            _ => Err(ServiceError::UnexpectedResponse),
        }
    }

    async fn read_resource(
        &self,
        session_id: &str,
        uri: &str,
        cancel_token: CancellationToken,
    ) -> Result<ReadResourceResult, McpError> {
        let response = self
            .send_request(
                with_session_context(
                    ClientRequest::ReadResourceRequest(Request::new(
                        ReadResourceRequestParams::new(uri.to_owned()),
                    )),
                    Some(session_id),
                    None,
                    None,
                ),
                cancel_token,
            )
            .await?;
        match response {
            ServerResult::ReadResourceResult(result) => Ok(result),
            _ => Err(ServiceError::UnexpectedResponse),
        }
    }

    async fn list_prompts(
        &self,
        session_id: &str,
        next_cursor: Option<String>,
        cancel_token: CancellationToken,
    ) -> Result<ListPromptsResult, McpError> {
        let response = self
            .send_request(
                with_session_context(
                    ClientRequest::ListPromptsRequest(RequestOptionalParam::with_param(
                        PaginatedRequestParams::default().with_cursor(next_cursor),
                    )),
                    Some(session_id),
                    None,
                    None,
                ),
                cancel_token,
            )
            .await?;
        match response {
            ServerResult::ListPromptsResult(result) => Ok(result),
            _ => Err(ServiceError::UnexpectedResponse),
        }
    }

    async fn get_prompt(
        &self,
        session_id: &str,
        name: &str,
        arguments: Value,
        cancel_token: CancellationToken,
    ) -> Result<GetPromptResult, McpError> {
        let mut params = GetPromptRequestParams::new(name.to_owned());
        if let Value::Object(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let response = self
            .send_request(
                with_session_context(
                    ClientRequest::GetPromptRequest(Request::new(params)),
                    Some(session_id),
                    None,
                    None,
                ),
                cancel_token,
            )
            .await?;
        match response {
            ServerResult::GetPromptResult(result) => Ok(result),
            _ => Err(ServiceError::UnexpectedResponse),
        }
    }

    async fn subscribe(&self) -> mpsc::Receiver<rmcp::model::ServerNotification> {
        // `()` is deliberately used as the rmcp client handler so transient
        // servers cannot invoke sampling, elicitation, or other host callbacks.
        // Request-scoped results remain fully available; unsolicited server
        // notifications are intentionally not exposed by this minimal host.
        mpsc::channel(1).1
    }
}

async fn send_cancel(peer: &Peer<RoleClient>, request_id: rmcp::model::RequestId, reason: &str) {
    let notification = Notification::new(CancelledNotificationParam::new(
        Some(request_id),
        Some(reason.to_owned()),
    ));
    let _ = tokio::time::timeout(
        TRANSIENT_MCP_CANCEL_SEND_TIMEOUT,
        peer.send_notification(notification.into()),
    )
    .await;
}

fn with_session_context(
    request: ClientRequest,
    session_id: Option<&str>,
    working_dir: Option<&str>,
    tool_call_request_id: Option<&str>,
) -> ClientRequest {
    match request {
        ClientRequest::ListResourcesRequest(mut request) => {
            request.extensions = context_extensions(
                request.extensions,
                session_id,
                working_dir,
                tool_call_request_id,
            );
            ClientRequest::ListResourcesRequest(request)
        }
        ClientRequest::ReadResourceRequest(mut request) => {
            request.extensions = context_extensions(
                request.extensions,
                session_id,
                working_dir,
                tool_call_request_id,
            );
            ClientRequest::ReadResourceRequest(request)
        }
        ClientRequest::ListToolsRequest(mut request) => {
            request.extensions = context_extensions(
                request.extensions,
                session_id,
                working_dir,
                tool_call_request_id,
            );
            ClientRequest::ListToolsRequest(request)
        }
        ClientRequest::CallToolRequest(mut request) => {
            request.extensions = context_extensions(
                request.extensions,
                session_id,
                working_dir,
                tool_call_request_id,
            );
            ClientRequest::CallToolRequest(request)
        }
        ClientRequest::ListPromptsRequest(mut request) => {
            request.extensions = context_extensions(
                request.extensions,
                session_id,
                working_dir,
                tool_call_request_id,
            );
            ClientRequest::ListPromptsRequest(request)
        }
        ClientRequest::GetPromptRequest(mut request) => {
            request.extensions = context_extensions(
                request.extensions,
                session_id,
                working_dir,
                tool_call_request_id,
            );
            ClientRequest::GetPromptRequest(request)
        }
        request => request,
    }
}

fn context_extensions(
    mut extensions: Extensions,
    session_id: Option<&str>,
    working_dir: Option<&str>,
    tool_call_request_id: Option<&str>,
) -> Extensions {
    let mut metadata = extensions
        .get::<MetaObject>()
        .map(|metadata| metadata.0.clone())
        .unwrap_or_default();
    metadata.retain(|key, _| {
        !key.eq_ignore_ascii_case(SESSION_ID_HEADER)
            && !key.eq_ignore_ascii_case(WORKING_DIR_HEADER)
            && !key.eq_ignore_ascii_case(TOOL_CALL_REQUEST_ID_HEADER)
    });
    if let Some(session_id) = session_id.filter(|value| !value.is_empty()) {
        metadata.insert(
            SESSION_ID_HEADER.to_owned(),
            Value::String(session_id.to_owned()),
        );
    }
    if let Some(working_dir) = working_dir.filter(|value| !value.is_empty()) {
        metadata.insert(
            WORKING_DIR_HEADER.to_owned(),
            Value::String(working_dir.to_owned()),
        );
    }
    if let Some(request_id) = tool_call_request_id.filter(|value| !value.is_empty()) {
        metadata.insert(
            TOOL_CALL_REQUEST_ID_HEADER.to_owned(),
            Value::String(request_id.to_owned()),
        );
    }
    extensions.insert(MetaObject(metadata));
    extensions
}

fn validate_http_url(url: &str) -> Result<(), TransientMcpConnectError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| TransientMcpConnectError::InvalidConfiguration("MCP HTTP URL is invalid"))?;
    if parsed.host_str().is_none() {
        return Err(TransientMcpConnectError::InvalidConfiguration(
            "MCP HTTP URL has no host",
        ));
    }
    // Remote MCP servers are admissible: the ACP client owns the endpoint
    // choice for its session.
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(TransientMcpConnectError::InvalidConfiguration(
            "MCP URL must use HTTP or HTTPS",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(TransientMcpConnectError::InvalidConfiguration(
            "MCP HTTP URL contains credentials or a fragment",
        ));
    }
    Ok(())
}

fn parse_http_headers(
    headers: Vec<(String, String)>,
) -> Result<
    HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    TransientMcpConnectError,
> {
    let mut parsed = HashMap::with_capacity(headers.len());
    let mut names = HashSet::with_capacity(headers.len());
    for (name, value) in headers {
        let normalized = name.to_ascii_lowercase();
        if reserved_http_header(&normalized) {
            return Err(TransientMcpConnectError::InvalidConfiguration(
                "MCP HTTP header is reserved by the transport",
            ));
        }
        if !names.insert(normalized) {
            return Err(TransientMcpConnectError::InvalidConfiguration(
                "MCP HTTP headers contain a duplicate name",
            ));
        }
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            TransientMcpConnectError::InvalidConfiguration("MCP HTTP header name is invalid")
        })?;
        let value = reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
            TransientMcpConnectError::InvalidConfiguration("MCP HTTP header value is invalid")
        })?;
        parsed.insert(name, value);
    }
    Ok(parsed)
}

fn reserved_http_header(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "connection"
            | "content-length"
            | "content-type"
            | "forwarded"
            | "host"
            | "keep-alive"
            | "last-event-id"
            | "mcp-method"
            | "mcp-name"
            | "mcp-protocol-version"
            | "mcp-session-id"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "via"
            | "x-real-ip"
    ) || name.starts_with("mcp-param-")
        || name.starts_with("proxy-")
        || name.starts_with("x-forwarded-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_catalog_tool_has_no_persistable_hints_or_host_metadata() {
        let mut tool = Tool::new("original", "description", JsonObject::new());
        tool.annotations = Some(rmcp::model::ToolAnnotations::new().read_only(true));
        tool.meta = Some(MetaObject(JsonObject::new()));
        tool.icons = Some(Vec::new());

        sanitize_catalog_tool(&mut tool, "paseo__original");

        assert_eq!(tool.name, "paseo__original");
        assert!(tool.annotations.is_none());
        assert!(tool.meta.is_none());
        assert!(tool.icons.is_none());
    }

    #[test]
    fn public_tool_names_use_the_provider_safe_subset_and_limit() {
        assert!(valid_public_tool_name("paseo__read_file"));
        assert!(!valid_public_tool_name("paseo__read.file"));
        assert!(!valid_public_tool_name(
            &"a".repeat(TRANSIENT_MCP_MAX_PUBLIC_TOOL_NAME_BYTES + 1)
        ));
    }
}

#[cfg(test)]
mod http_url_tests {
    use super::validate_http_url;

    #[test]
    fn http_urls_allow_remote_hosts_and_reject_other_schemes() {
        assert!(validate_http_url("http://[::1]:8080/mcp").is_ok());
        assert!(validate_http_url("http://127.0.0.1:8080/mcp").is_ok());
        assert!(validate_http_url("http://[2606:4700::1111]:8080/mcp").is_ok());
        assert!(validate_http_url("http://10.0.0.1:8080/mcp").is_ok());
        assert!(validate_http_url("https://mcp.example.com/sse").is_ok());
        assert!(validate_http_url("ftp://127.0.0.1:8080/mcp").is_err());
        assert!(validate_http_url("ws://127.0.0.1:8080/mcp").is_err());
        assert!(validate_http_url("http://user:pw@127.0.0.1:8080/mcp").is_err());
    }
}
