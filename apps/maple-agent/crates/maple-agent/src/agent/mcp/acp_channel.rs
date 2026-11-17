//! MCP over ACP: the live bridge from one ACP connection to an MCP server
//! the ACP client serves on that connection, and the rmcp transport that
//! carries an MCP client session over it.
//!
//! An ACP server registers one channel per session `{"type":"acp"}` MCP
//! server under a key minted for that attachment; the
//! [`AgentMcpTransport::Acp`](super::super::AgentMcpTransport::Acp) config
//! names the key, so the generic connection machinery finds the live
//! connection at connect time without knowing about ACP. The registry holds
//! weak references: a channel lives as long as its ACP session, and a task
//! that outlives the session simply cannot connect again.
//!
//! On the wire, each MCP request becomes one ACP `mcp/message` request whose
//! response carries the inner MCP result, and each MCP notification becomes
//! one `mcp/message` notification. The ACP client answers every inner
//! request in place, so nothing arrives that this transport did not send,
//! and the stream side carries only the responses to the requests the sink
//! side sent.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, Weak};
use std::task::{Context, Poll};

use futures_util::Stream;
use rmcp::model::{
    ClientJsonRpcMessage, ErrorCode, ErrorData, JsonRpcError, JsonRpcVersion2_0, RequestId,
    ServerJsonRpcMessage,
};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

/// How long one inner MCP request may take over the channel. The rmcp side
/// keeps its own per-call timeouts; this is the last stopgap for a request
/// nothing answers, so its spawned task cannot linger forever.
pub(crate) const CHANNEL_REQUEST_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);

/// One live MCP-over-ACP server connection: `server_id`'s MCP server as the
/// ACP client serves it, reached through `mcp/connect`, `mcp/message` and
/// `mcp/disconnect` on the ACP connection.
#[async_trait::async_trait]
pub(crate) trait AcpMcpChannel: Send + Sync {
    /// Open a connection and return its id.
    async fn connect(&self) -> Result<String, String>;
    /// Send one inner MCP request; the answer is its inner MCP result.
    async fn message(
        &self,
        connection: &str,
        method: &str,
        params: Option<Map<String, Value>>,
    ) -> Result<Value, String>;
    /// Send one inner MCP notification.
    async fn notify(
        &self,
        connection: &str,
        method: &str,
        params: Option<Map<String, Value>>,
    ) -> Result<(), String>;
    /// Close a connection. Best effort: an error is not a failure of the
    /// task's MCP servers.
    async fn disconnect(&self, connection: &str);
}

type ChannelRegistry = std::sync::RwLock<HashMap<String, Weak<dyn AcpMcpChannel>>>;

fn registry() -> &'static ChannelRegistry {
    static REGISTRY: OnceLock<ChannelRegistry> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Register the channel a session attachment carries, under the key its
/// task's [`AgentMcpTransport::Acp`] config names. Only the ACP server
/// module attaches channels.
#[cfg(feature = "acp")]
pub(crate) fn register_channel(key: &str, channel: &Arc<dyn AcpMcpChannel>) {
    let mut registry = registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Sweep entries whose session ended while unrelated channels register.
    registry.retain(|_, weak| weak.strong_count() > 0);
    registry.insert(key.to_owned(), Arc::downgrade(channel));
}

/// Remove a session attachment's channel. Its strong references live as
/// long as the session, so this only keeps the registry from waiting for
/// the sweep.
#[cfg(feature = "acp")]
pub(crate) fn remove_channel(key: &str) {
    registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(key);
}

/// The live channel a task's config names, if its ACP session still runs.
pub(crate) fn acp_channel(key: &str) -> Option<Arc<dyn AcpMcpChannel>> {
    registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
        .and_then(|weak| weak.upgrade())
}

/// The rmcp transport for one MCP-over-ACP connection: a sink that sends
/// each MCP message as its own `mcp/message`, and a stream of the responses
/// those sends produce.
pub(crate) fn acp_transport(
    channel: Arc<dyn AcpMcpChannel>,
    connection: String,
) -> (AcpMcpSink, AcpMcpStream) {
    let (sender, receiver) = mpsc::channel(32);
    (
        AcpMcpSink {
            channel,
            connection,
            responses: sender,
        },
        AcpMcpStream { receiver },
    )
}

/// The transport's error, which rmcp requires to be a std error.
#[derive(Debug)]
pub(crate) struct AcpMcpTransportError(String);

impl std::fmt::Display for AcpMcpTransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for AcpMcpTransportError {}

impl From<serde_json::Error> for AcpMcpTransportError {
    fn from(error: serde_json::Error) -> Self {
        AcpMcpTransportError(format!("the MCP message could not be encoded: {error}"))
    }
}

/// The sending half of the transport.
pub(crate) struct AcpMcpSink {
    channel: Arc<dyn AcpMcpChannel>,
    connection: String,
    responses: mpsc::Sender<ServerJsonRpcMessage>,
}

impl futures_util::Sink<ClientJsonRpcMessage> for AcpMcpSink {
    type Error = AcpMcpTransportError;

    fn poll_ready(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: ClientJsonRpcMessage) -> Result<(), Self::Error> {
        let sink = self.get_mut();
        // The envelope holds the method and params of the inner MCP message,
        // flattened beside the JSON-RPC frame.
        let envelope = serde_json::to_value(&item)?;
        let Some(method) = envelope
            .get("method")
            .and_then(Value::as_str)
            .map(String::from)
        else {
            // A response to a server request. This transport never receives
            // one, since the ACP bridge answers each request in place; drop.
            return Ok(());
        };
        let params = envelope.get("params").and_then(Value::as_object).cloned();
        let channel = Arc::clone(&sink.channel);
        let connection = sink.connection.clone();
        let responses = sink.responses.clone();
        tokio::spawn(async move {
            match envelope.get("id") {
                Some(id) => {
                    let sent = tokio::time::timeout(
                        CHANNEL_REQUEST_TIMEOUT,
                        channel.message(&connection, &method, params),
                    )
                    .await;
                    let message = match sent {
                        Ok(Ok(result)) => response_message(id.clone(), result),
                        Ok(Err(text)) => error_message(id.clone(), text),
                        Err(_) => error_message(id.clone(), "it did not answer in time".into()),
                    };
                    let _ = responses.send(message).await;
                }
                None => {
                    let _ = tokio::time::timeout(
                        CHANNEL_REQUEST_TIMEOUT,
                        channel.notify(&connection, &method, params),
                    )
                    .await;
                }
            }
        });
        Ok(())
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// The receiving half of the transport: the responses the sink's sends
/// produce, in the order the channel produced them.
pub(crate) struct AcpMcpStream {
    receiver: mpsc::Receiver<ServerJsonRpcMessage>,
}

impl Stream for AcpMcpStream {
    type Item = ServerJsonRpcMessage;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

/// The inner MCP result as the response to the request `id` names.
fn response_message(id: Value, result: Value) -> ServerJsonRpcMessage {
    let envelope = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
    match serde_json::from_value(envelope) {
        Ok(message) => message,
        // A result no known MCP result shape matches still ends its request.
        Err(_) => error_message(id, "the MCP server's answer could not be read".into()),
    }
}

/// An inner MCP error response for the request `id` names.
fn error_message(id: Value, message: String) -> ServerJsonRpcMessage {
    let id = serde_json::from_value::<RequestId>(id)
        .map_err(|error| {
            // A request id this build cannot name cannot be answered by
            // name; an unnumbered id at least ends the request.
            log::debug!("MCP-over-ACP response id could not be read: {error}");
        })
        .ok();
    ServerJsonRpcMessage::Error(JsonRpcError {
        jsonrpc: JsonRpcVersion2_0,
        id,
        error: ErrorData::new(ErrorCode::INTERNAL_ERROR, message, None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;
    use std::time::Duration;

    /// The inner method and params of one recorded message.
    type SentMessage = (String, Option<Map<String, Value>>);

    /// A channel that records what was sent and answers from a script.
    struct FakeChannel {
        connect: tokio::sync::Mutex<Result<String, String>>,
        messages: std::sync::Mutex<Vec<SentMessage>>,
        notifications: std::sync::Mutex<Vec<SentMessage>>,
        disconnected: std::sync::Mutex<Vec<String>>,
        results: std::sync::Mutex<HashMap<String, Value>>,
    }

    impl FakeChannel {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                connect: tokio::sync::Mutex::new(Ok("connection-1".into())),
                messages: Default::default(),
                notifications: Default::default(),
                disconnected: Default::default(),
                results: Default::default(),
            })
        }

        fn answer(&self, method: &str, result: Value) {
            self.results
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(method.to_string(), result);
        }
    }

    #[async_trait::async_trait]
    impl AcpMcpChannel for FakeChannel {
        async fn connect(&self) -> Result<String, String> {
            self.connect.lock().await.clone()
        }

        async fn message(
            &self,
            connection: &str,
            method: &str,
            params: Option<Map<String, Value>>,
        ) -> Result<Value, String> {
            assert_eq!(connection, "connection-1");
            self.messages
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((method.to_string(), params));
            self.results
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(method)
                .cloned()
                .ok_or_else(|| format!("no scripted answer for {method}"))
        }

        async fn notify(
            &self,
            connection: &str,
            method: &str,
            params: Option<Map<String, Value>>,
        ) -> Result<(), String> {
            assert_eq!(connection, "connection-1");
            self.notifications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((method.to_string(), params));
            Ok(())
        }

        async fn disconnect(&self, connection: &str) {
            self.disconnected
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(connection.to_string());
        }
    }

    fn request(id: Value, method: &str, params: Value) -> ClientJsonRpcMessage {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .expect("the request is valid")
    }

    fn notification(method: &str, params: Value) -> ClientJsonRpcMessage {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .expect("the notification is valid")
    }

    #[tokio::test]
    #[cfg(feature = "acp")]
    async fn the_registry_holds_only_live_channels() {
        let key = "acp-channel-key";
        let channel: Arc<dyn AcpMcpChannel> = FakeChannel::new();
        register_channel(key, &channel);
        assert!(acp_channel(key).is_some());
        remove_channel(key);
        assert!(acp_channel(key).is_none(), "an explicit removal is final");

        register_channel(key, &channel);
        drop(channel);
        assert!(
            acp_channel(key).is_none(),
            "a dropped session's channel cannot be revived"
        );

        // A dead entry does not block the key's reuse by a later session.
        let channel: Arc<dyn AcpMcpChannel> = FakeChannel::new();
        register_channel(key, &channel);
        assert!(acp_channel(key).is_some());
        remove_channel(key);
    }

    #[tokio::test]
    async fn requests_travel_as_messages_and_come_back_as_responses() {
        let channel = FakeChannel::new();
        channel.answer(
            "tools/list",
            serde_json::json!({"tools": [{"name": "t3_code"}]}),
        );
        let (mut sink, mut stream) = acp_transport(channel.clone(), "connection-1".into());
        futures_util::Sink::start_send(
            Pin::new(&mut sink),
            request(serde_json::json!(1), "tools/list", serde_json::json!({})),
        )
        .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the response arrives")
            .expect("the stream stays open");
        match message {
            ServerJsonRpcMessage::Response(response) => {
                assert_eq!(response.id, RequestId::Number(1));
                assert_eq!(
                    serde_json::to_value(response.result).unwrap()["tools"][0]["name"],
                    "t3_code"
                );
            }
            other => panic!("the answer is a response, not {other:?}"),
        }
        let sent = channel.messages.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "tools/list");
    }

    #[tokio::test]
    async fn notifications_travel_without_a_response() {
        let channel = FakeChannel::new();
        let (mut sink, _stream) = acp_transport(channel.clone(), "connection-1".into());
        futures_util::Sink::start_send(
            Pin::new(&mut sink),
            notification(
                "notifications/initialized",
                serde_json::json!({"protocolVersion": "2025-11-25"}),
            ),
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let sent = channel.notifications.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "notifications/initialized");
        assert!(
            channel.messages.lock().unwrap().is_empty(),
            "a notification is not a request"
        );
    }

    #[tokio::test]
    async fn a_failed_message_becomes_an_error_response() {
        let channel = FakeChannel::new();
        let (mut sink, mut stream) = acp_transport(channel.clone(), "connection-1".into());
        futures_util::Sink::start_send(
            Pin::new(&mut sink),
            request(
                serde_json::json!("two"),
                "tools/call",
                serde_json::json!({"name": "missing"}),
            ),
        )
        .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the error arrives")
            .expect("the stream stays open");
        match message {
            ServerJsonRpcMessage::Error(error) => {
                assert_eq!(error.id, Some(RequestId::String("two".into())));
                assert_eq!(error.error.code, ErrorCode::INTERNAL_ERROR);
                assert!(
                    error.error.message.contains("no scripted answer"),
                    "{}",
                    error.error.message
                );
            }
            other => panic!("the answer is an error, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_result_no_known_shape_matches_still_ends_its_request() {
        let channel = FakeChannel::new();
        // No MCP result shape matches a bare string; rmcp carries it as a
        // custom result, so the request still completes with its own id.
        channel.answer("tools/call", serde_json::json!("just a string"));
        let (mut sink, mut stream) = acp_transport(channel.clone(), "connection-1".into());
        futures_util::Sink::start_send(
            Pin::new(&mut sink),
            request(
                serde_json::json!(7),
                "tools/call",
                serde_json::json!({"name": "t3_code"}),
            ),
        )
        .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the answer arrives")
            .expect("the stream stays open");
        match message {
            ServerJsonRpcMessage::Response(response) => {
                assert_eq!(response.id, RequestId::Number(7));
                assert_eq!(
                    serde_json::to_value(response.result).unwrap(),
                    serde_json::json!("just a string")
                );
            }
            other => panic!("the answer is a response, not {other:?}"),
        }
    }
}
