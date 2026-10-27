//! Private, versioned local host messages. Payloads are never logged.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

pub const VERSION: u32 = 3;
pub const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), ":", env!("MAPLE_GIT_SOURCE_SHA"));
// Bounded local history/control replies retain their existing frame budget.
// Attachments are imported by staged local path, not sent as inline JSON bytes.
// Reads check length before allocating; connections bound admitted calls.
pub const MAX_FRAME: usize = 160 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub profile: String,
    pub build: String,
    pub mode: ConnectionMode,
    #[serde(default)]
    pub remote_host: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConnectionMode {
    Client,
    Acp,
    Stop,
    /// Same-build, host-authorized stop for one acknowledged remote update.
    /// Older hosts reject this variant; it must never degrade to plain Stop.
    RemoteUpdateStop {
        operation: [u8; 16],
    },
}

#[derive(Serialize, Deserialize)]
pub struct Welcome {
    pub version: u32,
    pub pid: u32,
    pub build: String,
    pub user_id: Option<String>,
    #[serde(default = "open_store_state")]
    pub store: maple_agent::store::StoreState,
}

fn open_store_state() -> maple_agent::store::StoreState {
    maple_agent::store::StoreState::Open
}

#[derive(Serialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    pub send_id: Option<String>,
    pub params: serde_json::Value,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Request")
            .field("id", &self.id)
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

/// Own the decoded sensitive JSON until dispatch completes. Borrow password
/// fields during decoding so this guard wipes the original owned copy too.
#[derive(Serialize)]
#[serde(transparent)]
pub struct SensitiveRequest(pub Request);

impl SensitiveRequest {
    pub fn into_request(mut self) -> Request {
        std::mem::replace(
            &mut self.0,
            Request {
                id: 0,
                method: String::new(),
                send_id: None,
                params: serde_json::Value::Null,
            },
        )
    }
}

impl std::ops::Deref for SensitiveRequest {
    type Target = Request;
    fn deref(&self) -> &Request {
        &self.0
    }
}

impl Drop for SensitiveRequest {
    fn drop(&mut self) {
        wipe_json(&mut self.0.params);
    }
}

#[cfg(test)]
thread_local! { static WIPED_TEST_SECRET: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
pub(super) fn reset_test_secret_wipes() {
    WIPED_TEST_SECRET.with(|count| count.set(0));
}

#[cfg(test)]
pub(super) fn test_secret_wipes() -> usize {
    WIPED_TEST_SECRET.with(|count| count.get())
}

fn wipe_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(value) => {
            #[cfg(test)]
            if value.starts_with("maple-wipe-test-secret") {
                WIPED_TEST_SECRET.with(|count| count.set(count.get() + 1));
            }
            value.zeroize();
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(wipe_json),
        serde_json::Value::Object(values) => {
            for (mut key, mut value) in std::mem::take(values) {
                key.zeroize();
                wipe_json(&mut value);
            }
        }
        _ => {}
    }
}

// Each partially parsed child owns a wiping guard. A syntax/type error after a
// password string therefore wipes it before serde discards the partial tree.
struct WipingValue(serde_json::Value);
impl WipingValue {
    fn take(mut self) -> serde_json::Value {
        std::mem::take(&mut self.0)
    }
}
impl Drop for WipingValue {
    fn drop(&mut self) {
        wipe_json(&mut self.0);
    }
}
impl<'de> Deserialize<'de> for WipingValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = WipingValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(WipingValue(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(WipingValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(WipingValue(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|v| WipingValue(v.into()))
                    .ok_or_else(|| E::custom("Invalid JSON number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(WipingValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(WipingValue(v.into()))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(WipingValue(serde_json::Value::Null))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(WipingValue(serde_json::Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut array = WipingValue(serde_json::Value::Array(Vec::new()));
                while let Some(value) = seq.next_element::<WipingValue>()? {
                    array
                        .0
                        .as_array_mut()
                        .expect("JSON array")
                        .push(value.take());
                }
                Ok(array)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut object = WipingValue(serde_json::Value::Object(serde_json::Map::new()));
                while let Some(mut key) = map.next_key::<Zeroizing<String>>()? {
                    let object = object.0.as_object_mut().expect("JSON object");
                    if object.contains_key(key.as_str()) {
                        return Err(serde::de::Error::custom("Duplicate host field"));
                    }
                    let value = map.next_value::<WipingValue>()?;
                    object.insert(std::mem::take(&mut *key), value.take());
                }
                Ok(object)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut value = WipingValue::deserialize(deserializer)?;
        let object = value
            .0
            .as_object_mut()
            .ok_or_else(|| D::Error::custom("Invalid host request"))?;
        let id = object
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| D::Error::custom("Invalid request identity"))?;
        let method = object
            .get("method")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| D::Error::custom("Invalid host method"))?
            .to_owned();
        let send_id = match object.get("send_id") {
            Some(serde_json::Value::String(id)) => Some(id.clone()),
            None | Some(serde_json::Value::Null) => None,
            _ => return Err(D::Error::custom("Invalid send identity")),
        };
        let params = object
            .remove("params")
            .ok_or_else(|| D::Error::custom("Missing host parameters"))?;
        Ok(Request {
            id,
            method,
            send_id,
            params,
        })
    }
}

#[derive(Serialize, Deserialize)]
pub enum Message {
    Resync,
    Closed {
        reason: String,
    },
    Response {
        id: u64,
        result: Result<serde_json::Value, String>,
    },
    Event(Box<maple_agent::agent::AgentServiceEvent>),
}

/// A reply keeps its original JSON guarded until a caller either borrows it
/// for secret decoding or explicitly transfers an ordinary page to serde.
/// Dropped queues, absent callers and schema errors all wipe the owned tree.
pub(super) struct SensitiveValue(serde_json::Value);

impl SensitiveValue {
    pub(super) fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    pub(super) fn into_value(mut self) -> serde_json::Value {
        std::mem::take(&mut self.0)
    }
}

impl std::ops::Deref for SensitiveValue {
    type Target = serde_json::Value;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Debug for SensitiveValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SensitiveValue([REDACTED])")
    }
}

impl Drop for SensitiveValue {
    fn drop(&mut self) {
        wipe_json(&mut self.0);
    }
}

impl<'de> Deserialize<'de> for SensitiveValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        WipingValue::deserialize(deserializer).map(|value| Self(value.take()))
    }
}

/// The same wire variants as Message, with the response guard installed during
/// parsing rather than after parsing. If another field fails validation, serde
/// drops the guarded partial response. Ordinary pages are moved, never cloned.
#[derive(Deserialize)]
pub(super) enum IncomingMessage {
    Resync,
    Closed {
        reason: String,
    },
    Response {
        id: u64,
        result: Result<SensitiveValue, String>,
    },
    Event(Box<maple_agent::agent::AgentServiceEvent>),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostStatus {
    pub acp_clients: usize,
    pub running_tasks: usize,
    pub remote_controllers: usize,
    pub background_tasks: usize,
    #[serde(default)]
    pub local_api_running: bool,
    #[serde(default)]
    pub local_api_requests: usize,
}

pub struct SensitiveMessage(pub Message);
impl Drop for SensitiveMessage {
    fn drop(&mut self) {
        if let Message::Response {
            result: Ok(value), ..
        } = &mut self.0
        {
            wipe_json(value);
        }
    }
}

pub async fn read<T: DeserializeOwned>(input: &mut (impl AsyncRead + Unpin)) -> Result<T, String> {
    let length = input
        .read_u32()
        .await
        .map_err(|_| "Host connection closed".to_string())? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err("Host message exceeds the size limit".into());
    }
    let mut bytes = Zeroizing::new(vec![0; length]);
    input
        .read_exact(&mut bytes)
        .await
        .map_err(|_| "Host connection closed".to_string())?;
    // Validate syntax without materializing decoded strings before the wiping
    // request visitor runs, including malformed escape sequences.
    let _: &serde_json::value::RawValue =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid host message".to_owned())?;
    serde_json::from_slice(&bytes).map_err(|_| "Invalid host message".into())
}

pub async fn write<T: Serialize>(
    output: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<(), String> {
    let bytes = crate::framing::encode(value, MAX_FRAME).map_err(|error| match error {
        crate::framing::EncodeError::TooLarge => "Host message exceeds the size limit".to_owned(),
        crate::framing::EncodeError::Invalid => "Could not encode host message".to_owned(),
    })?;
    write_encoded(output, &bytes).await
}

pub(crate) async fn write_encoded(
    output: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err("Host message exceeds the size limit".into());
    }
    output
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|_| "Host connection closed".to_string())?;
    output
        .write_all(bytes)
        .await
        .map_err(|_| "Host connection closed".to_string())?;
    output
        .flush()
        .await
        .map_err(|_| "Host connection closed".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_response_identity_wipes_an_already_parsed_secret_result() {
        reset_test_secret_wipes();
        let invalid = br#"{"Response":{"result":{"Ok":{"passphrase":"maple-wipe-test-secret-reply-parse"}},"id":"wrong type"}}"#;
        assert!(serde_json::from_slice::<IncomingMessage>(invalid).is_err());
        assert_eq!(test_secret_wipes(), 1);
    }

    #[tokio::test]
    async fn cancelled_incoming_queue_wipes_its_owned_secret_reply() {
        reset_test_secret_wipes();
        let message: IncomingMessage = serde_json::from_slice(
            br#"{"Response":{"id":1,"result":{"Ok":{"passphrase":"maple-wipe-test-secret-reply-queue"}}}}"#,
        )
        .unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        sender
            .send(message)
            .await
            .unwrap_or_else(|_| panic!("fixture queue closed"));
        assert_eq!(test_secret_wipes(), 0);
        drop(receiver);
        assert_eq!(test_secret_wipes(), 1);
    }

    #[tokio::test]
    async fn fragmented_frames_preserve_request_boundaries() {
        let (mut sender, mut receiver) = tokio::io::duplex(7);
        let writer = tokio::spawn(async move {
            for id in 0..20 {
                write(
                    &mut sender,
                    &Request {
                        id,
                        send_id: None,
                        method: "test".into(),
                        params: serde_json::json!(["a payload larger than the pipe"]),
                    },
                )
                .await
                .unwrap();
            }
        });
        for id in 0..20 {
            assert_eq!(read::<Request>(&mut receiver).await.unwrap().id, id);
        }
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_header_is_rejected_before_reading_or_allocating_a_body() {
        let (mut sender, mut receiver) = tokio::io::duplex(8);
        sender.write_u32(MAX_FRAME as u32 + 1).await.unwrap();
        assert!(
            read::<Request>(&mut receiver)
                .await
                .unwrap_err()
                .contains("size limit")
        );
    }
    #[test]
    fn semantic_parse_failure_and_rejected_owned_requests_wipe_passwords() {
        WIPED_TEST_SECRET.with(|count| count.set(0));
        let invalid=br#"{"params":{"password":"maple-wipe-test-secret-parse"},"id":"wrong type","method":"unlock"}"#;
        assert!(serde_json::from_slice::<Request>(invalid).is_err());
        assert_eq!(WIPED_TEST_SECRET.with(|count| count.get()), 1);
        let request: Request = serde_json::from_slice(
            br#"{"id":1,"method":"login","params":["fixture","maple-wipe-test-secret-reject"]}"#,
        )
        .unwrap();
        let request = SensitiveRequest(request);
        assert!(
            super::super::gate(&maple_agent::store::StoreState::Locked, &request.method).is_err()
        );
        assert!(!format!("{:?}", request.0).contains("maple-wipe-test-secret"));
        drop(request);
        assert_eq!(WIPED_TEST_SECRET.with(|count| count.get()), 2);
    }

    #[tokio::test]
    async fn cancelled_queued_request_wipes_its_owned_password() {
        WIPED_TEST_SECRET.with(|count| count.set(0));
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let request = SensitiveRequest(Request {
            id: 1,
            method: "unlock".into(),
            send_id: None,
            params: serde_json::json!({"password":"maple-wipe-test-secret-cancel"}),
        });
        sender
            .send(request)
            .await
            .unwrap_or_else(|_| panic!("fixture queue closed"));
        drop(receiver);
        assert_eq!(WIPED_TEST_SECRET.with(|count| count.get()), 1);
    }

    #[tokio::test]
    async fn malformed_escaped_password_is_rejected_without_materializing_parameters() {
        WIPED_TEST_SECRET.with(|count| count.set(0));
        let malformed=br#"{"id":1,"method":"unlock","params":{"password":"maple-wipe-test-secret\uD800oops"}}"#;
        let (mut sender, mut receiver) = tokio::io::duplex(512);
        sender.write_u32(malformed.len() as u32).await.unwrap();
        sender.write_all(malformed).await.unwrap();
        assert!(read::<Request>(&mut receiver).await.is_err());
        assert_eq!(WIPED_TEST_SECRET.with(|count| count.get()), 0);
    }
}
