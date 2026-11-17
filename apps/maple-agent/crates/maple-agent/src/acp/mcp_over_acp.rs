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

use agent_client_protocol::schema::v1::{
    ConnectMcpRequest, ConnectMcpResponse, DisconnectMcpRequest, McpConnectionId,
    MessageMcpNotification, MessageMcpRequest, MessageMcpResponse,
};
use agent_client_protocol::{Client, ConnectionTo, JsonRpcRequest};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long one channel operation waits before giving up. Inner MCP requests
/// use the transport's own longer timeout; this bounds connect, the
/// notifications, and a disconnect.
const CHANNEL_OPERATION_TIMEOUT: Duration = Duration::from_secs(ACP_SESSION_MCP_TIMEOUT_SECONDS);

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
    async fn request<Req>(&self, request: Req, timeout: Duration) -> Result<Req::Response, String>
    where
        Req: JsonRpcRequest + serde::Serialize,
        Req::Response: Send,
    {
        let encoded_bytes = serde_json::to_vec(&request)
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
    async fn send_notification_line(
        &self,
        notification: MessageMcpNotification,
    ) -> Result<(), String> {
        let encoded_bytes = serde_json::to_vec(&notification)
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
        let response: ConnectMcpResponse = self
            .request(
                ConnectMcpRequest::new(self.server_id.clone()),
                CHANNEL_OPERATION_TIMEOUT,
            )
            .await
            .map_err(|error| format!("connecting over ACP failed: {error}"))?;
        Ok(response.connection_id.0.to_string())
    }

    async fn message(
        &self,
        connection: &str,
        method: &str,
        params: Option<Map<String, Value>>,
    ) -> Result<Value, String> {
        let request = MessageMcpRequest::new(McpConnectionId::new(connection), method.to_string())
            .params(params);
        let response: MessageMcpResponse = self
            .request(request, acp_channel::CHANNEL_REQUEST_TIMEOUT)
            .await?;
        serde_json::from_str(response.0.get())
            .map_err(|error| format!("the MCP server's answer could not be read: {error}"))
    }

    async fn notify(
        &self,
        connection: &str,
        method: &str,
        params: Option<Map<String, Value>>,
    ) -> Result<(), String> {
        let notification =
            MessageMcpNotification::new(McpConnectionId::new(connection), method.to_string())
                .params(params);
        self.send_notification_line(notification).await
    }

    async fn disconnect(&self, connection: &str) {
        if let Err(error) = self
            .request(
                DisconnectMcpRequest::new(McpConnectionId::new(connection)),
                CHANNEL_OPERATION_TIMEOUT,
            )
            .await
        {
            log::debug!("MCP-over-ACP disconnect failed: {error}");
        }
    }
}
