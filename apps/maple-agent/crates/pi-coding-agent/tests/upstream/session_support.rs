#![allow(dead_code)]
use pi_agent_core::types::AgentMessage;
use pi_ai::types::{JsString, JsValue};
use pi_coding_agent::{
    config::HostConfig,
    core::session_manager::{NewSessionOptions, SessionEntry, SessionManager},
};
use pi_testkit::VirtualEnv;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
pub const NOW: i64 = 1_767_225_600_000;
pub fn json<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("upstream fixture")
}
pub fn observed(value: &impl Serialize) -> Value {
    serde_json::from_str(&pi_ai::utils::js_json::stringify_serializable(value).unwrap()).unwrap()
}
pub fn env() -> Arc<VirtualEnv> {
    Arc::new(VirtualEnv::new(NOW))
}
pub fn config(root: &Path) -> Arc<HostConfig> {
    Arc::new(HostConfig::new("pi", ".pi", root, root))
}
pub fn memory() -> SessionManager {
    memory_with(None, None)
}
pub fn memory_with(
    options: Option<NewSessionOptions>,
    entries: Option<Vec<SessionEntry>>,
) -> SessionManager {
    SessionManager::in_memory(
        Some("/project"),
        options,
        entries,
        env(),
        config(Path::new("/unused-pi-test-home")),
    )
    .unwrap()
}
pub fn user(text: &str) -> AgentMessage {
    json(json!({"role":"user","content":text,"timestamp":NOW}))
}
pub fn user_parts(text: &str) -> AgentMessage {
    json(json!({"role":"user","content":[{"type":"text","text":text}],"timestamp":NOW}))
}
pub fn assistant(text: &str) -> AgentMessage {
    json(
        json!({"role":"assistant","content":[{"type":"text","text":text}],"api":"anthropic-messages","provider":"anthropic","model":"test","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":NOW}),
    )
}
pub fn usage() -> pi_ai::types::Usage {
    json(
        json!({"input":10,"output":20,"cacheRead":30,"cacheWrite":40,"totalTokens":100,"cost":{"input":0.1,"output":0.2,"cacheRead":0.3,"cacheWrite":0.4,"total":1}}),
    )
}
pub fn ids(entries: &[SessionEntry]) -> Vec<JsString> {
    entries.iter().map(SessionEntry::id).collect()
}
pub fn roles(messages: &[AgentMessage]) -> Vec<JsString> {
    messages.iter().map(AgentMessage::role).collect()
}
pub fn strings(values: &[&str]) -> Vec<JsString> {
    values.iter().map(|s| (*s).into()).collect()
}
pub fn text(message: &AgentMessage) -> String {
    let raw = observed(message);
    if let Some(s) = raw["content"].as_str() {
        s.into()
    } else {
        raw["content"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter(|v| v["type"] == "text")
                    .filter_map(|v| v["text"].as_str())
                    .collect()
            })
            .unwrap_or_default()
    }
}
pub fn value(value: Value) -> JsValue {
    json(value)
}
pub fn field(entry: &SessionEntry, key: &str) -> Value {
    observed(&entry.get(key))
}
pub fn file_roles(path: impl AsRef<Path>) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|s| !s.is_empty())
        .map(|line| {
            let raw: Value = serde_json::from_str(line).unwrap();
            raw["message"]["role"]
                .as_str()
                .or_else(|| raw["type"].as_str())
                .unwrap()
                .into()
        })
        .collect()
}
pub fn assert_uuid_v7(id: &JsString) {
    let id = id.as_str().unwrap();
    assert_eq!(id.len(), 36);
    for (i, c) in id.chars().enumerate() {
        if [8, 13, 18, 23].contains(&i) {
            assert_eq!(c, '-')
        } else {
            assert!(c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        }
    }
    assert_eq!(&id[14..15], "7");
    assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
}
