//! The ACP connection's side of MCP over ACP: one channel per session
//! `{"type":"acp"}` MCP server, sending `mcp/connect`, `mcp/message` and
//! `mcp/disconnect` on the connection, and the task configs that name them.
//!
//! The channel lives as long as the connection's session attachment: its
//! lifetime is the connection's, and the registry in
//! [`crate::agent::mcp::acp_channel`] holds only a weak reference, so a task
//! that outlives its ACP session simply cannot connect again.

use super::session::{ACP_SESSION_MCP_TIMEOUT_SECONDS, AcpConnectionContext};
use super::transport::AcpOutboundTracker;
use crate::agent::mcp::acp_channel::{self, AcpMcpChannel};
use crate::agent::{AgentMcpServer, AgentMcpTransport};

use agent_client_protocol::{Client, ConnectionTo, UntypedMessage};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long one channel operation waits before giving up. Inner MCP requests
/// use the transport's own longer timeout; this bounds connect, the
/// notifications, and a disconnect.
const CHANNEL_OPERATION_TIMEOUT: Duration = Duration::from_secs(ACP_SESSION_MCP_TIMEOUT_SECONDS);

/// The four messages of the connection-based MCP-over-ACP binding, as
/// untyped JSON-RPC. T3 Code serves this binding and decodes nothing else.
/// The SDK dropped its types for the request-scoped binding that replaced
/// it, so Maple writes them itself, byte for byte what it has always sent:
/// absent params are omitted and nothing carries `_meta`. `wire_tests` pins
/// them.
pub(super) mod wire {
    use agent_client_protocol::UntypedMessage;
    use serde_json::{Map, Value, json};

    fn untyped(method: &str, params: Value) -> Result<UntypedMessage, String> {
        UntypedMessage::new(method, params)
            .map_err(|error| format!("the MCP-over-ACP message could not be encoded: {error}"))
    }

    /// `mcp/message`'s params, request or notification alike.
    fn inner(connection: &str, method: &str, params: Option<Map<String, Value>>) -> Value {
        let mut fields = Map::new();
        fields.insert("connectionId".to_string(), json!(connection));
        fields.insert("method".to_string(), json!(method));
        if let Some(params) = params {
            fields.insert("params".to_string(), Value::Object(params));
        }
        Value::Object(fields)
    }

    /// `mcp/connect`: open a connection to the client's server `server_id`.
    pub(in crate::acp) fn connect(server_id: &str) -> Result<UntypedMessage, String> {
        untyped("mcp/connect", json!({"serverId": server_id}))
    }

    /// `mcp/message` as a request: one inner MCP request on a connection.
    pub(in crate::acp) fn request(
        connection: &str,
        method: &str,
        params: Option<Map<String, Value>>,
    ) -> Result<UntypedMessage, String> {
        untyped("mcp/message", inner(connection, method, params))
    }

    /// `mcp/message` as a notification: one inner MCP notification.
    pub(in crate::acp) fn notification(
        connection: &str,
        method: &str,
        params: Option<Map<String, Value>>,
    ) -> Result<UntypedMessage, String> {
        untyped("mcp/message", inner(connection, method, params))
    }

    /// `mcp/disconnect`: close a connection.
    pub(in crate::acp) fn disconnect(connection: &str) -> Result<UntypedMessage, String> {
        untyped("mcp/disconnect", json!({"connectionId": connection}))
    }

    /// The connection id in `mcp/connect`'s answer.
    pub(in crate::acp) fn connection_id(answer: &Value) -> Result<String, String> {
        answer
            .get("connectionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "the ACP client's mcp/connect answer has no connectionId".to_string())
    }
}

/// An `{"type":"acp"}` MCP server the ACP client declared for a session, as
/// [`super::session::prepare_session_mcp`] read it.
#[derive(Debug)]
pub(super) struct PreparedAcpMcpServer {
    pub(super) name: String,
    pub(super) server_id: String,
}

/// One session attachment of an MCP-over-ACP server: the channel the task's
/// config names, the token it is registered under, and the config itself.
pub(super) struct AcpMcpAttachment {
    pub(super) token: String,
    /// The channel's strong reference, which keeps it registered: the
    /// registry holds only a weak one, so this field lives as long as the
    /// session and is read through the registry, not directly.
    #[allow(dead_code)]
    pub(super) channel: Arc<dyn AcpMcpChannel>,
    pub(super) config: AgentMcpServer,
}

/// Register a channel for each `{"type":"acp"}` MCP server the session
/// carries, and return the configs that name them. The caller adds the
/// configs to the session's servers and the attachments to its session, so
/// the channels are dropped — and their registry entries swept — when the
/// session is retired.
pub(super) fn attach_acp_mcp_channels(
    context: &AcpConnectionContext,
    cx: &ConnectionTo<Client>,
    servers: &[PreparedAcpMcpServer],
) -> Vec<AcpMcpAttachment> {
    servers
        .iter()
        .map(|server| {
            let token = format!(
                "acp-mcp-channel-{}",
                super::NEXT_ACP_MESSAGE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let channel: Arc<dyn AcpMcpChannel> = Arc::new(SessionAcpMcpChannel {
                connection: cx.clone(),
                outbound: Arc::clone(&context.outbound),
                lifetime: context.lifetime.clone(),
                server_id: server.server_id.clone(),
            });
            acp_channel::register_channel(&token, &channel);
            AcpMcpAttachment {
                config: AgentMcpServer {
                    name: server.name.clone(),
                    description: "ACP session MCP server".to_string(),
                    enabled: true,
                    timeout_seconds: ACP_SESSION_MCP_TIMEOUT_SECONDS,
                    transport: AgentMcpTransport::Acp {
                        server_id: server.server_id.clone(),
                        channel: token.clone(),
                    },
                },
                token,
                channel,
            }
        })
        .collect()
}

/// Take the attachments' tokens out of the registry. Their strong references
/// live until the session is dropped, so this only keeps the registry from
/// waiting for its own sweep of dead entries.
pub(super) fn remove_attached_channels(attachments: &[AcpMcpAttachment]) {
    for attachment in attachments {
        acp_channel::remove_channel(&attachment.token);
    }
}

/// One ACP connection's reach of one MCP-over-ACP server.
struct SessionAcpMcpChannel {
    connection: ConnectionTo<Client>,
    outbound: Arc<AcpOutboundTracker>,
    lifetime: CancellationToken,
    /// The ACP `serverId` of the server this channel reaches.
    server_id: String,
}

impl SessionAcpMcpChannel {
    /// Send one request and wait for its response, holding the connection's
    /// outbound reservation until it arrives.
    async fn request(&self, request: UntypedMessage, timeout: Duration) -> Result<Value, String> {
        let encoded_bytes = serde_json::to_vec(request.params())
            .map_err(|error| format!("the MCP-over-ACP request could not be encoded: {error}"))?
            .len();
        let reservation = self
            .outbound
            .reserve(encoded_bytes, &self.lifetime)
            .await
            .map_err(|_| "the ACP connection closed".to_string())?;
        let sent = self.connection.send_request(request);
        let mut response = Box::pin(sent.block_task());
        let answer = tokio::select! {
            biased;
            _ = self.lifetime.cancelled() => {
                super::retain_cancelled_caller_request(response, reservation);
                return Err("the ACP connection closed".to_string());
            }
            _ = tokio::time::sleep(timeout) => {
                super::retain_cancelled_caller_request(response, reservation);
                return Err("the ACP client did not answer in time".to_string());
            }
            answer = &mut response => answer,
        };
        drop(reservation);
        answer.map_err(|error| error.to_string())
    }

    /// Send one notification. Its reservation is released once the line is
    /// queued: only session updates hold theirs until the caller's reader
    /// keeps up, and an MCP notification is small and bounded per message.
    async fn send_notification_line(&self, notification: UntypedMessage) -> Result<(), String> {
        let encoded_bytes = serde_json::to_vec(notification.params())
            .map_err(|error| {
                format!("the MCP-over-ACP notification could not be encoded: {error}")
            })?
            .len();
        let reservation = self
            .outbound
            .reserve(encoded_bytes, &self.lifetime)
            .await
            .map_err(|_| "the ACP connection closed".to_string())?;
        let sent = self.connection.send_notification(notification);
        drop(reservation);
        sent.map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl AcpMcpChannel for SessionAcpMcpChannel {
    async fn connect(&self) -> Result<String, String> {
        let answer = self
            .request(wire::connect(&self.server_id)?, CHANNEL_OPERATION_TIMEOUT)
            .await
            .map_err(|error| format!("connecting over ACP failed: {error}"))?;
        wire::connection_id(&answer)
    }

    async fn message(
        &self,
        connection: &str,
        method: &str,
        params: Option<Map<String, Value>>,
    ) -> Result<Value, String> {
        // The answer is the inner MCP result as the client's server gave it.
        self.request(
            wire::request(connection, method, params)?,
            acp_channel::CHANNEL_REQUEST_TIMEOUT,
        )
        .await
    }

    async fn notify(
        &self,
        connection: &str,
        method: &str,
        params: Option<Map<String, Value>>,
    ) -> Result<(), String> {
        self.send_notification_line(wire::notification(connection, method, params)?)
            .await
    }

    async fn disconnect(&self, connection: &str) {
        let request = match wire::disconnect(connection) {
            Ok(request) => request,
            Err(error) => {
                log::debug!("MCP-over-ACP disconnect failed: {error}");
                return;
            }
        };
        if let Err(error) = self.request(request, CHANNEL_OPERATION_TIMEOUT).await {
            log::debug!("MCP-over-ACP disconnect failed: {error}");
        }
    }
}
