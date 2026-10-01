//! JSON-RPC 2.0 on the control channel.
//!
//! Requests carry a numeric id and get exactly one response. Notifications
//! (no id) carry host events in one direction and the keepalive in both.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Error codes. The JSON-RPC reserved range is honored; ours start at
/// -32000 as the spec allows. There is no parse error: a control frame
/// that is not a JSON-RPC message closes the connection instead of being
/// answered, because nothing in it can be trusted to carry an id.
pub mod code {
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    /// The host refused the handshake: mismatched environment or
    /// protocol. The connection closes after this answer.
    pub const HANDSHAKE_REFUSED: i64 = -32000;
    /// The method requires a handshake first.
    pub const NOT_READY: i64 = -32001;
    /// The host's `HostBackend` returned an error; the message is the
    /// user-facing text.
    pub const HOST_ERROR: i64 = -32002;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn host(message: impl Into<String>) -> Self {
        Self::new(code::HOST_ERROR, message)
    }
}

/// Any message on the control channel. Serde tries the shapes in order,
/// most demanding first: `Request` needs `id` and `method`,
/// `Notification` needs `method` without `id`, and `Response` is whatever
/// remains with an `id`. Unknown fields are ignored everywhere, so the
/// order is what keeps a request from reading as a response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub jsonrpc: Version,
    pub id: u64,
    pub method: String,
    /// Omitted when there are none.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub jsonrpc: Version,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub jsonrpc: Version,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// The literal `"2.0"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Version;

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as Deserialize>::deserialize(deserializer)?;
        if value == "2.0" {
            Ok(Version)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported JSON-RPC version {value:?}"
            )))
        }
    }
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: Version,
            id,
            method: method.into(),
            params,
        }
    }
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: Version,
            method: method.into(),
            params,
        }
    }
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Self {
            jsonrpc: Version,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, error: RpcError) -> Self {
        Self {
            jsonrpc: Version,
            id,
            result: None,
            error: Some(error),
        }
    }
}

pub fn encode(message: &Message) -> Result<Vec<u8>, String> {
    serde_json::to_vec(message).map_err(|error| format!("cannot encode message: {error}"))
}

/// A successful response whose result is already JSON text, so a large
/// answer is not built as a tree and serialized again.
#[derive(Serialize)]
struct EncodedResponse<'a> {
    jsonrpc: Version,
    id: u64,
    result: &'a serde_json::value::RawValue,
}

/// Encode `Response::ok(id, result)` from the result's JSON text.
pub fn encode_result(id: u64, result: &serde_json::value::RawValue) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&EncodedResponse {
        jsonrpc: Version,
        id,
        result,
    })
    .map_err(|error| format!("cannot encode response: {error}"))
}

pub fn decode(bytes: &[u8]) -> Result<Message, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("cannot decode message: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_classify() {
        let request = Message::Request(Request::new(1, "session.list", serde_json::json!({})));
        let bytes = encode(&request).unwrap();
        assert_eq!(decode(&bytes).unwrap(), request);

        let response = Message::Response(Response::ok(1, serde_json::json!([])));
        assert_eq!(decode(&encode(&response).unwrap()).unwrap(), response);

        let failure = Message::Response(Response::err(2, RpcError::host("no")));
        assert_eq!(decode(&encode(&failure).unwrap()).unwrap(), failure);

        let note = Message::Notification(Notification::new("event", serde_json::json!({"seq": 1})));
        assert_eq!(decode(&encode(&note).unwrap()).unwrap(), note);

        assert!(decode(br#"{"jsonrpc":"1.0","id":1,"method":"x"}"#).is_err());
        assert!(decode(b"not json").is_err());
    }

    #[test]
    fn an_encoded_result_reads_back_as_the_same_response() {
        let raw = serde_json::value::RawValue::from_string(
            r#"{"items":[1,2],"hasMore":false}"#.to_string(),
        )
        .unwrap();
        let bytes = encode_result(7, &raw).unwrap();
        assert_eq!(
            decode(&bytes).unwrap(),
            Message::Response(Response::ok(
                7,
                serde_json::json!({"items": [1, 2], "hasMore": false})
            ))
        );
    }
}
