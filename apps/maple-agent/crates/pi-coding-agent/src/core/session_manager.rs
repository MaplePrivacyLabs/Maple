//! Full append-only JSONL session store from `core/session-manager.ts`.
//! Entries retain unknown fields, insertion order and UTF-16 strings. Cloning an
//! entry is a shallow shared reference, matching the source store's read API.
use crate::{
    config::HostConfig,
    core::messages::*,
    utils::{
        dates::{iso_timestamp, parse_timestamp, timestamp_from_value},
        paths::{PathInputOptions, normalize_path, resolve_path},
    },
};
use indexmap::IndexMap;
use pi_agent_core::types::{AgentMessage, AgentMessageValue, Shared};
use pi_ai::utils::{
    js_json::stringify, js_value::to_js_value, json_parse::parse_json_utf16,
    transcript::get_current_system_message,
};
use pi_ai::{
    env::{CancellationToken, PiEnv},
    types::{JsObject, JsString, JsValue, Message, Usage},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    collections::HashSet,
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::Arc,
};

pub const CURRENT_SESSION_VERSION: u32 = 3;
const MAX_SESSION_HEADER_SCAN_BYTES: usize = 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionError {
    pub message: JsString,
    pub name: &'static str,
}
impl SessionError {
    pub fn new(message: impl Into<JsString>) -> Self {
        Self {
            message: message.into(),
            name: "Error",
        }
    }
    pub fn name(&self) -> &str {
        self.name
    }
}
impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message.to_string_lossy())
    }
}
impl std::error::Error for SessionError {}
impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}
impl From<String> for SessionError {
    fn from(error: String) -> Self {
        Self::new(error)
    }
}
pub type SessionResult<T> = Result<T, SessionError>;

#[derive(Clone, Debug)]
struct EntryData {
    raw: JsValue,
    message: Option<AgentMessage>,
}
#[derive(Clone, Debug)]
pub struct SessionEntry(Shared<EntryData>);
pub type FileEntry = SessionEntry;
pub type SessionHeader = SessionEntry;
pub type CompactionEntry = SessionEntry;
pub type BranchSummaryEntry = SessionEntry;
pub type ContextEditEntry = SessionEntry;
pub type SessionMessageEntry = SessionEntry;
pub type ThinkingLevelChangeEntry = SessionEntry;
pub type ModelChangeEntry = SessionEntry;
pub type UsageEntry = SessionEntry;
pub type CustomEntry = SessionEntry;
pub type LabelEntry = SessionEntry;
pub type SessionInfoEntry = SessionEntry;
pub type CustomMessageEntry = SessionEntry;
pub type ContextEditableContent = JsValue;
impl SessionEntry {
    pub fn new(raw: JsObject) -> Self {
        Self::from_value(raw.into())
    }
    pub fn from_value(raw: JsValue) -> Self {
        let message = raw
            .get("message")
            .and_then(JsValue::as_object)
            .cloned()
            .map(raw_message);
        Self(Shared::new(EntryData { raw, message }))
    }
    pub fn with_message(raw: JsObject, message: AgentMessage) -> Self {
        Self(Shared::new(EntryData {
            raw: raw.into(),
            message: Some(message),
        }))
    }
    pub fn value(&self) -> JsValue {
        self.0.read(|data| {
            let mut raw = data.raw.clone();
            if let (Some(raw), Some(message)) = (raw.as_object_mut(), &data.message) {
                raw.insert("message", message_value(message));
            }
            raw
        })
    }
    pub fn snapshot(&self) -> JsObject {
        self.value().as_object().cloned().unwrap_or_default()
    }
    pub fn get(&self, key: &str) -> Option<JsValue> {
        if key == "message"
            && let Some(message) = self.message()
        {
            return Some(message_value(&message));
        }
        self.0.read(|data| data.raw.get(key).cloned())
    }
    pub fn set(&self, key: impl Into<JsString>, value: JsValue) {
        let key = key.into();
        self.update(|raw| {
            raw.insert(key, value);
        });
    }
    pub fn update<R>(&self, update: impl FnOnce(&mut JsObject) -> R) -> R {
        self.0.update(|data| {
            if let Some(message) = &data.message {
                data.raw
                    .as_object_mut()
                    .expect("session entry is an object")
                    .insert("message", message_value(message));
            }
            let before = data.raw.get("message").cloned();
            let result = update(
                data.raw
                    .as_object_mut()
                    .expect("session entry is an object"),
            );
            if data.raw.get("message") != before.as_ref() {
                data.message = data
                    .raw
                    .get("message")
                    .and_then(JsValue::as_object)
                    .cloned()
                    .map(raw_message);
            }
            result
        })
    }
    pub fn kind(&self) -> JsString {
        string(self.get("type").as_ref())
    }
    pub fn id(&self) -> JsString {
        string(self.get("id").as_ref())
    }
    pub fn parent_id(&self) -> Option<JsString> {
        self.get("parentId").and_then(|v| v.as_js_str().cloned())
    }
    pub fn timestamp(&self) -> JsString {
        string(self.get("timestamp").as_ref())
    }
    pub fn message(&self) -> Option<AgentMessage> {
        self.0.read(|data| data.message.clone())
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }
    pub fn copy(&self) -> Self {
        if let Some(message) = self.message() {
            Self::with_message(self.snapshot(), message)
        } else {
            Self::from_value(self.value())
        }
    }
}
impl PartialEq for SessionEntry {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other) || self.value() == other.value()
    }
}
impl Serialize for SessionEntry {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.value().serialize(s)
    }
}
impl<'de> Deserialize<'de> for SessionEntry {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        JsValue::deserialize(d).map(Self::from_value)
    }
}
impl From<JsObject> for SessionEntry {
    fn from(raw: JsObject) -> Self {
        Self::new(raw)
    }
}
#[derive(Clone, Debug, Default)]
pub struct NewSessionOptions {
    pub id: Option<JsString>,
    pub parent_session: Option<JsString>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_timestamp: Option<JsString>,
}
impl Drop for SessionTreeNode {
    fn drop(&mut self) {
        let mut pending = std::mem::take(&mut self.children);
        while let Some(mut node) = pending.pop() {
            pending.append(&mut node.children);
        }
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedSessionEntry {
    pub source_entry: SessionEntry,
    pub messages: Vec<AgentMessage>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModel {
    pub provider: JsString,
    pub model_id: JsString,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionProjection {
    pub entries: Vec<ProjectedSessionEntry>,
    pub messages: Vec<AgentMessage>,
    pub thinking_level: JsString,
    pub model: Option<SessionModel>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: JsString,
    pub model: Option<SessionModel>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub path: String,
    pub id: JsString,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_path: Option<JsString>,
    #[serde(serialize_with = "serialize_date")]
    pub created: f64,
    #[serde(serialize_with = "serialize_date")]
    pub modified: f64,
    pub message_count: usize,
    pub first_message: JsString,
    pub all_messages_text: JsString,
}
fn serialize_date<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.is_finite() && value.abs() <= 8.64e15 {
        iso_timestamp(value.trunc() as i64).serialize(serializer)
    } else {
        serializer.serialize_none()
    }
}
pub type SessionListProgress<'a> = dyn FnMut(usize, usize, Option<&[SessionInfo]>) + Send + 'a;
pub trait ReadonlySessionManager {
    fn get_cwd(&self) -> &str;
    fn get_session_dir(&self) -> &str;
    fn get_session_id(&self) -> JsString;
    fn get_session_file(&self) -> Option<String>;
    fn get_branch(&self, from_id: Option<&JsString>) -> Vec<SessionEntry>;
    fn get_entry(&self, id: &JsString) -> Option<SessionEntry>;
    fn get_leaf_id(&self) -> Option<JsString>;
    fn get_leaf_entry(&self) -> Option<SessionEntry>;
    fn get_label(&self, id: &JsString) -> Option<JsString>;
    fn build_context_entries(&self) -> Vec<SessionEntry>;
    fn build_session_projection(&self) -> SessionProjection;
    fn get_header(&self) -> Option<SessionHeader>;
    fn get_entries(&self) -> Vec<SessionEntry>;
    fn get_tree(&self) -> Vec<SessionTreeNode>;
    fn get_session_name(&self) -> Option<JsString>;
}
pub fn assert_valid_session_id(id: &JsString) -> SessionResult<()> {
    let units = id.as_utf16();
    let alnum = |u: u16| matches!(u,48..=57|65..=90|97..=122);
    if units.first().is_some_and(|u| alnum(*u))
        && units.last().is_some_and(|u| alnum(*u))
        && units
            .iter()
            .all(|u| alnum(*u) || matches!(*u, 45 | 46 | 95))
    {
        return Ok(());
    }
    Err(SessionError::new(
        "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character",
    ))
}
fn generate_id(existing: impl Fn(&JsString) -> bool, env: &dyn PiEnv) -> SessionResult<JsString> {
    for _ in 0..100 {
        let id: JsString = env
            .random_uuid()
            .map_err(|e| SessionError::new(e.to_string()))?
            .chars()
            .take(8)
            .collect::<String>()
            .into();
        if !existing(&id) {
            return Ok(id);
        }
    }
    env.random_uuid()
        .map(Into::into)
        .map_err(|e| SessionError::new(e.to_string()))
}
fn migrate_to_current_version(entries: &mut [FileEntry], env: &dyn PiEnv) -> SessionResult<bool> {
    let version = entries
        .iter()
        .find(|e| e.kind() == "session")
        .and_then(|e| e.get("version"))
        .filter(|v| !v.is_null())
        .map(|v| pi_ai::utils::raw_message::number(Some(&v)))
        .unwrap_or(1.0);
    if version >= f64::from(CURRENT_SESSION_VERSION) {
        return Ok(false);
    }
    if version < 2.0 {
        // Upstream's ids Set is intentionally never populated during migration.
        let mut previous = None;
        for index in 0..entries.len() {
            let entry = &entries[index];
            if entry.kind() == "session" {
                entry.set("version", 2.0.into());
                continue;
            }
            let id = generate_id(|_| false, env)?;
            entry.set("id", id.clone().into());
            entry.set(
                "parentId",
                previous.map(JsValue::String).unwrap_or(JsValue::Null),
            );
            previous = Some(id);
            if entry.kind() == "compaction"
                && let Some(kept) = entry.get("firstKeptEntryIndex").and_then(|v| v.as_f64())
            {
                if kept >= 0.0
                    && kept.fract() == 0.0
                    && let Some(target) =
                        entries.get(kept as usize).filter(|e| e.kind() != "session")
                {
                    if let Some(id) = target.get("id") {
                        entry.set("firstKeptEntryId", id);
                    } else {
                        entry.update(|raw| {
                            raw.remove("firstKeptEntryId");
                        });
                    }
                }
                entry.update(|raw| {
                    raw.remove("firstKeptEntryIndex");
                });
            }
        }
    }
    if version < 3.0 {
        for entry in entries {
            if entry.kind() == "session" {
                entry.set("version", 3.0.into());
            } else if entry.kind() == "message"
                && let Some(message) = entry.message()
                && message.role() == "hookMessage"
            {
                message.update(|value| {
                    if let AgentMessageValue::Custom(raw) = value {
                        raw.update(|object| {
                            object.insert("role", "custom".into());
                        });
                    }
                });
            }
        }
    }
    Ok(true)
}
pub fn migrate_session_entries(entries: &mut [FileEntry], env: &dyn PiEnv) -> SessionResult<()> {
    migrate_to_current_version(entries, env).map(|_| ())
}
pub fn parse_session_entries(content: &JsString) -> Vec<FileEntry> {
    trim_js(content)
        .as_utf16()
        .split(|u| *u == 10)
        .filter_map(|line| {
            parse_json_utf16(&JsString::from_utf16(line.to_vec()))
                .ok()
                .map(SessionEntry::from_value)
        })
        .collect()
}
pub fn get_latest_compaction_entry(entries: &[SessionEntry]) -> Option<CompactionEntry> {
    entries
        .iter()
        .rev()
        .find(|e| e.kind() == "compaction")
        .cloned()
}
fn entry_index(entries: &[SessionEntry]) -> IndexMap<JsString, SessionEntry> {
    entries.iter().map(|e| (e.id(), e.clone())).collect()
}
fn build_session_path(
    entries: &[SessionEntry],
    leaf_id: Option<Option<&JsString>>,
    by_id: Option<&IndexMap<JsString, SessionEntry>>,
) -> Vec<SessionEntry> {
    if leaf_id == Some(None) {
        return vec![];
    }
    let own;
    let index = if let Some(index) = by_id {
        index
    } else {
        own = entry_index(entries);
        &own
    };
    let mut current = leaf_id
        .flatten()
        .filter(|id| !id.is_empty())
        .and_then(|id| index.get(id))
        .cloned()
        .or_else(|| entries.last().cloned());
    let mut path = vec![];
    while let Some(entry) = current {
        current = entry
            .parent_id()
            .filter(|id| !id.is_empty())
            .and_then(|id| index.get(&id).cloned());
        path.push(entry);
    }
    path.reverse();
    path
}
pub fn session_entry_to_context_messages(entry: &SessionEntry) -> Vec<AgentMessage> {
    match entry.kind().as_str() {
        Some("message") => {
            let Some(message) = entry.message() else {
                return vec![];
            };
            let raw = message_value(&message);
            let role = message.role();
            if raw.get("content").is_none_or(JsValue::is_null)
                && matches!(
                    role.as_str(),
                    Some("system" | "user" | "assistant" | "toolResult")
                )
            {
                let mut copy = raw.as_object().expect("message is an object").clone();
                copy.insert(
                    "content",
                    if role == "system" {
                        "".into()
                    } else {
                        vec![].into()
                    },
                );
                vec![raw_message(copy)]
            } else {
                vec![message]
            }
        }
        Some("custom_message") => vec![create_custom_message(
            string(entry.get("customType").as_ref()),
            entry
                .get("content")
                .filter(|v| !v.is_null())
                .unwrap_or_else(|| vec![].into()),
            entry.get("display").is_some_and(|v| truthy(&v)),
            entry.get("details"),
            &entry.timestamp(),
        )],
        Some("branch_summary") if entry.get("summary").is_some_and(|v| truthy(&v)) => {
            vec![create_branch_summary_message(
                string(entry.get("summary").as_ref()),
                string(entry.get("fromId").as_ref()),
                &entry.timestamp(),
            )]
        }
        Some("compaction") => {
            let summary = create_compaction_summary_message_raw_tokens(
                string(entry.get("summary").as_ref()),
                entry.get("tokensBefore"),
                &entry.timestamp(),
            );
            let mut messages = vec![];
            if let Some(system) = entry
                .get("systemMessage")
                .filter(truthy)
                .and_then(|v| v.as_object().cloned())
            {
                messages.push(raw_message(system));
            }
            messages.push(summary);
            messages
        }
        _ => vec![],
    }
}
pub fn build_context_entries(
    entries: &[SessionEntry],
    leaf_id: Option<Option<&JsString>>,
    by_id: Option<&IndexMap<JsString, SessionEntry>>,
) -> Vec<SessionEntry> {
    let path = build_session_path(entries, leaf_id, by_id);
    let Some(compaction) = get_latest_compaction_entry(&path) else {
        return path;
    };
    let Some(index) = path.iter().position(|e| e.id() == compaction.id()) else {
        return path;
    };
    let mut result = vec![compaction.clone()];
    let mut kept = false;
    for entry in &path[..index] {
        if Some(JsValue::String(entry.id())) == compaction.get("firstKeptEntryId") {
            kept = true;
        }
        if kept
            && !(entry.kind() == "message" && entry.message().is_some_and(|m| m.role() == "system"))
        {
            result.push(entry.clone());
        }
    }
    result.extend_from_slice(&path[index + 1..]);
    result
}
pub fn build_session_projection(
    entries: &[SessionEntry],
    leaf_id: Option<Option<&JsString>>,
    by_id: Option<&IndexMap<JsString, SessionEntry>>,
) -> SessionProjection {
    let path = build_session_path(entries, leaf_id, by_id);
    let mut thinking_level = "off".into();
    let mut model = None;
    for entry in &path {
        match entry.kind().as_str() {
            Some("thinking_level_change") => {
                thinking_level = string(entry.get("thinkingLevel").as_ref())
            }
            Some("model_change") => {
                model = Some(SessionModel {
                    provider: string(entry.get("provider").as_ref()),
                    model_id: string(entry.get("modelId").as_ref()),
                })
            }
            Some("message") => {
                if let Some(message) = entry.message().filter(|m| m.role() == "assistant") {
                    let raw = message_value(&message);
                    model = Some(SessionModel {
                        provider: string(raw.get("provider")),
                        model_id: string(raw.get("model")),
                    });
                }
            }
            _ => {}
        }
    }
    let context = build_context_entries(entries, leaf_id, by_id);
    let mut edits = IndexMap::new();
    for entry in &context {
        if entry.kind() == "context_edit" {
            edits.insert(string(entry.get("targetId").as_ref()), entry.clone());
        }
    }
    let projected = context
        .into_iter()
        .enumerate()
        .map(|(index, source_entry)| {
            let mut messages = if index > 0 && source_entry.kind() == "compaction" {
                vec![]
            } else {
                session_entry_to_context_messages(&source_entry)
            };
            if let Some(edit) = edits.get(&source_entry.id()) {
                match edit.get("replacement") {
                    Some(JsValue::Null) => messages.clear(),
                    Some(replacement) => {
                        for message in &mut messages {
                            if matches!(
                                message.role().as_str(),
                                Some("user" | "assistant" | "toolResult" | "custom")
                            ) {
                                let role = message.role();
                                let mut raw = message_value(message)
                                    .as_object()
                                    .expect("message object")
                                    .clone();
                                if let Some(content) = replacement.get("content") {
                                    let content = if matches!(
                                        role.as_str(),
                                        Some("assistant" | "toolResult")
                                    ) && content.is_string()
                                    {
                                        vec![object([
                                            ("type", "text".into()),
                                            ("text", content.clone()),
                                        ])]
                                        .into()
                                    } else {
                                        content.clone()
                                    };
                                    raw.insert("content", content);
                                } else {
                                    raw.remove("content");
                                }
                                *message = raw_message(raw);
                            }
                        }
                    }
                    _ => {}
                }
            }
            ProjectedSessionEntry {
                source_entry,
                messages,
            }
        })
        .collect::<Vec<_>>();
    let messages = projected.iter().flat_map(|e| e.messages.clone()).collect();
    SessionProjection {
        entries: projected,
        messages,
        thinking_level,
        model,
    }
}
pub fn build_session_context(
    entries: &[SessionEntry],
    leaf_id: Option<Option<&JsString>>,
    by_id: Option<&IndexMap<JsString, SessionEntry>>,
) -> SessionContext {
    let projection = build_session_projection(entries, leaf_id, by_id);
    SessionContext {
        messages: projection.messages,
        thinking_level: projection.thinking_level,
        model: projection.model,
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into()
}
fn resolve_host_path(path: &str, config: &HostConfig) -> SessionResult<String> {
    resolve_path(
        path,
        &path_string(&config.process_cwd),
        &PathInputOptions {
            home_dir: Some(path_string(&config.home_dir)),
            ..Default::default()
        },
    )
    .map_err(Into::into)
}
pub fn get_default_session_dir_path(
    cwd: &str,
    agent_dir: Option<&str>,
    config: &HostConfig,
) -> SessionResult<String> {
    let cwd = resolve_host_path(cwd, config)?;
    let default_agent = config.get_agent_dir()?;
    let agent = resolve_host_path(agent_dir.unwrap_or(&path_string(&default_agent)), config)?;
    let cwd = cwd.strip_prefix(['/', '\\']).unwrap_or(&cwd);
    let safe = format!("--{}--", cwd.replace(['/', '\\', ':'], "-"));
    Ok(path_string(&Path::new(&agent).join("sessions").join(safe)))
}
pub fn get_default_session_dir(
    cwd: &str,
    agent_dir: Option<&str>,
    config: &HostConfig,
) -> SessionResult<String> {
    let path = get_default_session_dir_path(cwd, agent_dir, config)?;
    if !Path::new(&path).exists() {
        fs::create_dir_all(&path)?;
    }
    Ok(path)
}
fn parse_entry_line(line: &[u8]) -> Option<FileEntry> {
    let text = String::from_utf8_lossy(line);
    parse_json_utf16(&JsString::from(text.as_ref()))
        .ok()
        .filter(truthy)
        .map(SessionEntry::from_value)
}
pub fn load_entries_from_file(file_path: &str) -> SessionResult<Vec<FileEntry>> {
    use std::io::BufRead;
    let file_path = normalize_path(file_path, &PathInputOptions::default())?;
    if !Path::new(&file_path).exists() {
        return Ok(vec![]);
    }
    let file = File::open(&file_path)?;
    let mut reader = std::io::BufReader::with_capacity(1024 * 1024, file);
    let mut entries = vec![];
    let mut line = vec![];
    let mut pending = false;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        pending = line.last() != Some(&b'\n');
        if let Some(entry) = parse_entry_line(&line) {
            entries.push(entry);
        }
    }
    if entries.is_empty() {
        return Ok(entries);
    }
    if entries[0].kind() != "session" || !entries[0].get("id").is_some_and(|v| v.is_string()) {
        return Ok(vec![]);
    }
    if pending {
        OpenOptions::new()
            .append(true)
            .open(&file_path)?
            .write_all(b"\n")?;
    }
    Ok(entries)
}
enum HeaderReadError {
    Io(std::io::Error),
    Limit,
}
fn read_session_header(file_path: &str) -> Result<Option<SessionHeader>, HeaderReadError> {
    let mut file = File::open(file_path).map_err(HeaderReadError::Io)?;
    let mut pending = vec![];
    let mut buffer = [0u8; 4096];
    let mut scanned = 0;
    let candidate = |line: &[u8]| -> Option<Option<SessionHeader>> {
        let entry = parse_entry_line(line)?;
        Some(
            (entry.kind() == "session" && entry.get("id").is_some_and(|v| v.is_string()))
                .then_some(entry),
        )
    };
    while scanned < MAX_SESSION_HEADER_SCAN_BYTES {
        let length = buffer.len().min(MAX_SESSION_HEADER_SCAN_BYTES - scanned);
        let n = file
            .read(&mut buffer[..length])
            .map_err(HeaderReadError::Io)?;
        if n == 0 {
            return Ok(candidate(&pending).flatten());
        }
        scanned += n;
        let mut start = 0;
        for (i, byte) in buffer[..n].iter().enumerate() {
            if *byte == b'\n' {
                pending.extend_from_slice(&buffer[start..i]);
                if let Some(header) = candidate(&pending) {
                    return Ok(header);
                }
                pending.clear();
                start = i + 1;
            }
        }
        pending.extend_from_slice(&buffer[start..n]);
    }
    if file.read(&mut buffer[..1]).map_err(HeaderReadError::Io)? == 0 {
        Ok(candidate(&pending).flatten())
    } else {
        Err(HeaderReadError::Limit)
    }
}
fn discovery_header(path: &str) -> Option<SessionHeader> {
    read_session_header(path).ok().flatten()
}
fn session_cwd_matches(cwd: &str, resolved_cwd: &str, config: &HostConfig) -> bool {
    !cwd.is_empty() && resolve_host_path(cwd, config).is_ok_and(|cwd| cwd == resolved_cwd)
}
fn mtime(meta: &fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .map(|time| match time.duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => duration.as_secs_f64() * 1000.0,
            Err(error) => -error.duration().as_secs_f64() * 1000.0,
        })
        .unwrap_or(f64::NAN)
}
pub fn find_most_recent_session(
    session_dir: &str,
    cwd: Option<&str>,
    config: &HostConfig,
) -> Option<String> {
    let dir = normalize_path(
        session_dir,
        &PathInputOptions {
            home_dir: Some(path_string(&config.home_dir)),
            ..Default::default()
        },
    )
    .ok()?;
    let cwd = cwd
        .filter(|s| !s.is_empty())
        .map(|cwd| resolve_host_path(cwd, config))
        .transpose()
        .ok()?;
    let mut paths = fs::read_dir(&dir)
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?
        .into_iter()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
        .map(|entry| {
            let path = entry.path();
            fs::metadata(&path).map(|meta| (path_string(&path), mtime(&meta)))
        })
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    paths.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (path, _) in paths {
        if let Some(header) = discovery_header(&path)
            && cwd.as_ref().is_none_or(|cwd| {
                header
                    .get("cwd")
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .is_some_and(|stored| session_cwd_matches(&stored, cwd, config))
            })
        {
            return Some(path);
        }
    }
    None
}
fn ensure_not_aborted(signal: Option<&CancellationToken>) -> SessionResult<()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(SessionError {
            message: "This operation was aborted".into(),
            name: "AbortError",
        })
    } else {
        Ok(())
    }
}
fn extract_text_content(content: &JsValue) -> Option<JsString> {
    if let Some(text) = content.as_js_str() {
        return Some(text.clone());
    }
    let blocks = content.as_array()?;
    if blocks.iter().any(JsValue::is_null) {
        return None;
    }
    Some(pi_ai::utils::raw_message::join_values(
        blocks
            .iter()
            .filter(|block| block.get("type").and_then(JsValue::as_str) == Some("text"))
            .map(|block| block.get("text")),
        " ",
    ))
}
fn build_session_info(
    file_path: &str,
    signal: Option<&CancellationToken>,
    stats: Option<fs::Metadata>,
) -> SessionResult<Option<SessionInfo>> {
    use std::io::BufRead;
    let run = || -> SessionResult<Option<SessionInfo>> {
        ensure_not_aborted(signal)?;
        let stats = stats.map(Ok).unwrap_or_else(|| fs::metadata(file_path))?;
        let mut header = None;
        let mut count = 0;
        let mut first = JsString::default();
        let mut all = vec![];
        let mut name = None;
        let mut last: Option<f64> = None;
        let reader = std::io::BufReader::new(File::open(file_path)?);
        for line in reader.split(b'\n') {
            ensure_not_aborted(signal)?;
            let Some(entry) = parse_entry_line(&line?) else {
                continue;
            };
            if header.is_none() {
                if entry.kind() != "session" {
                    return Ok(None);
                }
                header = Some(entry);
                continue;
            }
            if entry.kind() == "session_info" {
                name = entry
                    .get("name")
                    .and_then(|v| v.as_js_str().cloned())
                    .map(|s| trim_js(&s))
                    .filter(|s| !s.is_empty());
            }
            if entry.kind() != "message" {
                continue;
            }
            count += 1;
            let Some(message) = entry.get("message") else {
                return Ok(None);
            };
            let Some(role) = message.get("role").and_then(JsValue::as_str) else {
                continue;
            };
            let Some(content) = message.get("content") else {
                continue;
            };
            if !matches!(role, "user" | "assistant") {
                continue;
            }
            let activity = message
                .get("timestamp")
                .and_then(JsValue::as_f64)
                .unwrap_or_else(|| parse_timestamp(&entry.timestamp()));
            if !activity.is_nan() || message.get("timestamp").is_some_and(JsValue::is_number) {
                let previous = last.unwrap_or(0.0);
                last = Some(if previous.is_nan() || activity.is_nan() {
                    f64::NAN
                } else {
                    previous.max(activity)
                });
            }
            let Some(text) = extract_text_content(content) else {
                return Ok(None);
            };
            if text.is_empty() {
                continue;
            }
            all.push(text.clone());
            if first.is_empty() && role == "user" {
                first = text;
            }
        }
        let Some(header) = header else {
            return Ok(None);
        };
        let header_time = parse_timestamp(&header.timestamp());
        let created = timestamp_from_value(header.get("timestamp").as_ref());
        let modified = last.filter(|v| *v > 0.0).unwrap_or_else(|| {
            if header_time.is_nan() {
                mtime(&stats)
            } else {
                header_time
            }
        });
        Ok(Some(SessionInfo {
            path: file_path.into(),
            id: header.id(),
            cwd: header
                .get("cwd")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            name,
            parent_session_path: header
                .get("parentSession")
                .and_then(|v| v.as_js_str().cloned()),
            created,
            modified,
            message_count: count,
            first_message: if first.is_empty() {
                "(no messages)".into()
            } else {
                first
            },
            all_messages_text: JsString::join(all.iter(), " "),
        }))
    };
    match run() {
        Ok(value) => Ok(value),
        Err(_) => {
            ensure_not_aborted(signal)?;
            Ok(None)
        }
    }
}
fn trim_js(text: &JsString) -> JsString {
    let units = text.as_utf16();
    let whitespace =
        |u: &u16| char::from_u32(u32::from(*u)).is_some_and(crate::utils::paths::js_whitespace);
    let first = units
        .iter()
        .position(|u| !whitespace(u))
        .unwrap_or(units.len());
    let last = units
        .iter()
        .rposition(|u| !whitespace(u))
        .map_or(first, |i| i + 1);
    JsString::from_utf16(units[first..last].to_vec())
}
fn sort_session_infos(sessions: &mut [SessionInfo]) {
    sessions.sort_by(|a, b| {
        b.modified
            .partial_cmp(&a.modified)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

pub struct SessionManager {
    session_id: JsString,
    session_file: Option<String>,
    session_dir: String,
    cwd: String,
    persist: bool,
    flushed: bool,
    file_entries: Vec<FileEntry>,
    by_id: IndexMap<JsString, SessionEntry>,
    labels_by_id: IndexMap<JsString, JsString>,
    label_timestamps_by_id: IndexMap<JsString, JsString>,
    leaf_id: Option<JsString>,
    pub env: Arc<dyn PiEnv>,
    pub config: Arc<HostConfig>,
}
impl fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionManager")
            .field("session_id", &self.session_id)
            .field("session_file", &self.session_file)
            .field("entries", &self.file_entries.len())
            .finish()
    }
}
impl SessionManager {
    // The six upstream constructor inputs plus the two explicit host services.
    #[allow(clippy::too_many_arguments)]
    fn new(
        cwd: &str,
        session_dir: &str,
        session_file: Option<&str>,
        persist: bool,
        options: Option<NewSessionOptions>,
        preloaded: Option<Vec<FileEntry>>,
        env: Arc<dyn PiEnv>,
        config: Arc<HostConfig>,
    ) -> SessionResult<Self> {
        let cwd = resolve_host_path(cwd, &config)?;
        let session_dir = normalize_path(
            session_dir,
            &PathInputOptions {
                home_dir: Some(path_string(&config.home_dir)),
                ..Default::default()
            },
        )?;
        if persist && !session_dir.is_empty() && !Path::new(&session_dir).exists() {
            fs::create_dir_all(&session_dir)?;
        }
        let mut store = Self {
            session_id: JsString::default(),
            session_file: None,
            session_dir,
            cwd,
            persist,
            flushed: false,
            file_entries: vec![],
            by_id: IndexMap::new(),
            labels_by_id: IndexMap::new(),
            label_timestamps_by_id: IndexMap::new(),
            leaf_id: None,
            env,
            config,
        };
        if let Some(path) = session_file {
            store.set_session_file_inner(path, preloaded)?;
        } else if let Some(entries) = preloaded.filter(|e| !e.is_empty()) {
            store.load_entries(entries, options)?;
        } else {
            store.new_session(options)?;
        }
        Ok(store)
    }
    pub fn set_session_file(&mut self, path: &str) -> SessionResult<()> {
        self.set_session_file_inner(path, None)
    }
    fn set_session_file_inner(
        &mut self,
        path: &str,
        preloaded: Option<Vec<FileEntry>>,
    ) -> SessionResult<()> {
        let path = resolve_host_path(path, &self.config)?;
        self.session_file = Some(path.clone());
        if Path::new(&path).exists() {
            let entries = if let Some(entries) = preloaded {
                entries
            } else {
                load_entries_from_file(&path)?
            };
            if entries.is_empty() {
                if fs::metadata(&path)?.len() > 0 {
                    return Err(SessionError::new(format!(
                        "Session file is not a valid {} session: {path}",
                        self.config.app_name
                    )));
                }
                self.new_session(None)?;
                self.session_file = Some(path);
                self.rewrite_file()?;
                self.flushed = true;
                return Ok(());
            }
            self.load_entries(entries, None)?;
            self.flushed = true;
        } else {
            self.new_session(None)?;
            self.session_file = Some(path);
        }
        Ok(())
    }
    fn create_session_id(&self) -> SessionResult<JsString> {
        self.config
            .uuid_generator
            .uuidv7(self.env.as_ref(), None)
            .map(Into::into)
            .map_err(|e| SessionError::new(e.to_string()))
    }
    pub fn new_session(
        &mut self,
        options: Option<NewSessionOptions>,
    ) -> SessionResult<Option<String>> {
        let options = options.unwrap_or_default();
        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        self.session_id = match options.id {
            Some(id) => id,
            None => self.create_session_id()?,
        };
        let timestamp = iso_timestamp(self.env.now_ms());
        let mut header = JsObject::from([
            ("type", "session".into()),
            ("version", f64::from(CURRENT_SESSION_VERSION).into()),
            ("id", self.session_id.clone().into()),
            ("timestamp", timestamp.clone().into()),
            ("cwd", self.cwd.clone().into()),
        ]);
        if let Some(parent) = options.parent_session {
            header.insert("parentSession", parent.into());
        }
        self.file_entries = vec![header.into()];
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        self.flushed = false;
        if self.persist {
            self.session_file = Some(path_string(&Path::new(&self.session_dir).join(format!(
                "{}_{}.jsonl",
                timestamp.to_string_lossy().replace([':', '.'], "-"),
                self.session_id.to_string_lossy()
            ))));
        }
        Ok(self.session_file.clone())
    }
    fn load_entries(
        &mut self,
        entries: Vec<FileEntry>,
        options: Option<NewSessionOptions>,
    ) -> SessionResult<()> {
        if let Some(header) = entries.iter().find(|e| e.kind() == "session") {
            self.session_id = header.id();
            self.file_entries = entries;
            if migrate_to_current_version(&mut self.file_entries, self.env.as_ref())? {
                self.rewrite_file()?;
            }
        } else {
            self.new_session(options)?;
            self.file_entries.extend(entries);
        }
        self.build_index();
        Ok(())
    }
    fn build_index(&mut self) {
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        for entry in &self.file_entries {
            if entry.kind() == "session" {
                continue;
            }
            self.by_id.insert(entry.id(), entry.clone());
            self.leaf_id = Some(entry.id());
            if entry.kind() == "label" {
                let target = string(entry.get("targetId").as_ref());
                if let Some(label) = entry
                    .get("label")
                    .filter(truthy)
                    .and_then(|v| v.as_js_str().cloned())
                {
                    self.labels_by_id.insert(target.clone(), label);
                    self.label_timestamps_by_id
                        .insert(target, entry.timestamp());
                } else {
                    self.labels_by_id.shift_remove(&target);
                    self.label_timestamps_by_id.shift_remove(&target);
                }
            }
        }
    }
    fn write_entries(&self, file: &mut File) -> SessionResult<()> {
        for entry in &self.file_entries {
            file.write_all(stringify(&entry.value()).as_bytes())?;
            file.write_all(b"\n")?;
        }
        Ok(())
    }
    fn rewrite_file(&self) -> SessionResult<()> {
        if self.persist
            && let Some(path) = &self.session_file
        {
            self.write_entries(&mut File::create(path)?)?;
        }
        Ok(())
    }
    fn has_conversation(&self) -> bool {
        self.file_entries.iter().any(|e| {
            e.kind() == "message"
                && e.message()
                    .is_some_and(|m| matches!(m.role().as_str(), Some("user" | "assistant")))
        })
    }
    pub fn persist_entry(&mut self, entry: &SessionEntry) -> SessionResult<()> {
        if !self.persist {
            return Ok(());
        }
        let Some(path) = &self.session_file else {
            return Ok(());
        };
        if !self.flushed {
            if !self.has_conversation() {
                return Ok(());
            }
            let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
            self.write_entries(&mut file)?;
            self.flushed = true;
        } else {
            let mut file = OpenOptions::new().append(true).create(true).open(path)?;
            file.write_all(stringify(&entry.value()).as_bytes())?;
            file.write_all(b"\n")?;
        }
        Ok(())
    }
    fn append_entry(&mut self, entry: SessionEntry) -> SessionResult<()> {
        self.file_entries.push(entry.clone());
        self.by_id.insert(entry.id(), entry.clone());
        self.leaf_id = Some(entry.id());
        self.persist_entry(&entry)
    }
    fn base_entry(&self, kind: &str) -> SessionResult<JsObject> {
        Ok(JsObject::from([
            ("type", kind.into()),
            (
                "id",
                generate_id(|id| self.by_id.contains_key(id), self.env.as_ref())?.into(),
            ),
            (
                "parentId",
                self.leaf_id
                    .clone()
                    .map(JsValue::String)
                    .unwrap_or(JsValue::Null),
            ),
            ("timestamp", iso_timestamp(self.env.now_ms()).into()),
        ]))
    }
    fn append_raw(&mut self, raw: JsObject) -> SessionResult<JsString> {
        let entry = SessionEntry::new(raw);
        let id = entry.id();
        self.append_entry(entry)?;
        Ok(id)
    }
    pub fn append_message(&mut self, message: impl Into<AgentMessage>) -> SessionResult<JsString> {
        let message = message.into();
        let mut raw = self.base_entry("message")?;
        raw.insert("message", message_value(&message));
        let entry = SessionEntry::with_message(raw, message);
        let id = entry.id();
        self.append_entry(entry)?;
        Ok(id)
    }
    pub fn append_thinking_level_change(
        &mut self,
        level: impl Into<JsString>,
    ) -> SessionResult<JsString> {
        let mut raw = self.base_entry("thinking_level_change")?;
        raw.insert("thinkingLevel", level.into().into());
        self.append_raw(raw)
    }
    pub fn append_model_change(
        &mut self,
        provider: impl Into<JsString>,
        model_id: impl Into<JsString>,
    ) -> SessionResult<JsString> {
        let mut raw = self.base_entry("model_change")?;
        raw.insert("provider", provider.into().into());
        raw.insert("modelId", model_id.into().into());
        self.append_raw(raw)
    }
    pub fn append_usage(
        &mut self,
        kind: impl Into<JsString>,
        provider: impl Into<JsString>,
        model: impl Into<JsString>,
        usage: Usage,
        note: Option<JsString>,
    ) -> SessionResult<SessionEntry> {
        let mut raw = self.base_entry("usage")?;
        raw.insert("kind", kind.into().into());
        raw.insert("provider", provider.into().into());
        raw.insert("model", model.into().into());
        raw.insert("usage", to_js_value(&usage).expect("usage serializes"));
        if let Some(note) = note.filter(|s| !s.is_empty()) {
            raw.insert("note", note.into());
        }
        let entry = SessionEntry::new(raw);
        self.append_entry(entry.clone())?;
        Ok(entry)
    }
    pub fn append_compaction(
        &mut self,
        summary: impl Into<JsString>,
        first_kept_entry_id: Option<JsString>,
        tokens_before: f64,
        details: Option<JsValue>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> SessionResult<JsString> {
        self.append_compaction_raw_tokens(
            summary,
            first_kept_entry_id,
            tokens_before,
            details,
            from_hook,
            usage,
            None,
        )
    }
    /// Preserve a source compaction estimate's nonnumeric JavaScript value.
    #[allow(clippy::too_many_arguments)] // Upstream append arguments plus the retained raw-value view.
    pub fn append_compaction_raw_tokens(
        &mut self,
        summary: impl Into<JsString>,
        first_kept_entry_id: Option<JsString>,
        tokens_before: f64,
        details: Option<JsValue>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
        raw_tokens_before: Option<JsValue>,
    ) -> SessionResult<JsString> {
        let timestamp = iso_timestamp(self.env.now_ms());
        let projection = self.build_session_projection();
        let system_messages = projection
            .messages
            .iter()
            .filter(|m| m.role() == "system")
            .map(|m| {
                Message::Raw(
                    message_value(m)
                        .as_object()
                        .expect("system message is an object")
                        .clone()
                        .into(),
                )
            })
            .collect::<Vec<_>>();
        let system = get_current_system_message(&system_messages).map_err(SessionError::new)?;
        let id = generate_id(|id| self.by_id.contains_key(id), self.env.as_ref())?;
        let mut raw = JsObject::from([
            ("type", "compaction".into()),
            ("id", id.clone().into()),
            (
                "parentId",
                self.leaf_id
                    .clone()
                    .map(JsValue::String)
                    .unwrap_or(JsValue::Null),
            ),
            ("timestamp", timestamp.clone().into()),
            ("summary", summary.into().into()),
            (
                "firstKeptEntryId",
                first_kept_entry_id.unwrap_or_else(|| id.clone()).into(),
            ),
            (
                "tokensBefore",
                raw_tokens_before.unwrap_or_else(|| tokens_before.into()),
            ),
        ]);
        if let Some(details) = details {
            raw.insert("details", details);
        }
        if let Some(usage) = usage {
            raw.insert("usage", to_js_value(&usage).expect("usage serializes"));
        }
        if let Some(hook) = from_hook {
            raw.insert("fromHook", hook.into());
        }
        if let Some(system) = system {
            let mut system = to_js_value(&system)
                .expect("system serializes")
                .as_object()
                .expect("system object")
                .clone();
            system.insert("timestamp", parse_timestamp(&timestamp).into());
            raw.insert("systemMessage", system.into());
        }
        self.append_raw(raw)
    }
    pub fn append_custom_entry(
        &mut self,
        custom_type: impl Into<JsString>,
        data: Option<JsValue>,
    ) -> SessionResult<JsString> {
        let mut raw = JsObject::from([
            ("type", "custom".into()),
            ("customType", custom_type.into().into()),
        ]);
        if let Some(data) = data {
            raw.insert("data", data);
        }
        raw.extend(self.base_entry("custom")?);
        self.append_raw(raw)
    }
    pub fn append_session_info(&mut self, name: impl Into<JsString>) -> SessionResult<JsString> {
        let name = name.into();
        let mut sanitized = vec![];
        let mut newline = false;
        for unit in name.units() {
            if matches!(unit, 10 | 13) {
                if !newline {
                    sanitized.push(32);
                }
                newline = true;
            } else {
                sanitized.push(unit);
                newline = false;
            }
        }
        let mut raw = self.base_entry("session_info")?;
        raw.insert("name", trim_js(&JsString::from_utf16(sanitized)).into());
        self.append_raw(raw)
    }
    pub fn get_session_name(&self) -> Option<JsString> {
        self.file_entries
            .iter()
            .rev()
            .find(|e| e.kind() == "session_info")
            .and_then(|e| e.get("name"))
            .and_then(|v| v.as_js_str().cloned())
            .map(|s| trim_js(&s))
            .filter(|s| !s.is_empty())
    }
    pub fn append_custom_message_entry(
        &mut self,
        custom_type: impl Into<JsString>,
        content: JsValue,
        display: bool,
        details: Option<JsValue>,
    ) -> SessionResult<JsString> {
        let mut raw = JsObject::from([
            ("type", "custom_message".into()),
            ("customType", custom_type.into().into()),
            ("content", content),
            ("display", display.into()),
        ]);
        if let Some(details) = details {
            raw.insert("details", details);
        }
        raw.extend(self.base_entry("custom_message")?);
        self.append_raw(raw)
    }
    pub fn append_context_edit(
        &mut self,
        target_id: &JsString,
        replacement: JsValue,
    ) -> SessionResult<JsString> {
        if !replacement.is_null()
            && (!replacement.is_object()
                || replacement
                    .get("content")
                    .is_none_or(|v| !v.is_string() && !v.is_array()))
        {
            return Err(SessionError::new(
                "Context edit replacement must be null or contain string/array content",
            ));
        }
        let target = self
            .by_id
            .get(target_id)
            .ok_or_else(|| entry_error(target_id, " not found"))?;
        if !self.get_branch(None).iter().any(|e| e.id() == *target_id) {
            return Err(entry_error(target_id, " is not on the active branch"));
        }
        let target_role = if target.kind() == "message" {
            target.message().map(|m| m.role()).unwrap_or_default()
        } else {
            "custom".into()
        };
        if target.kind() != "custom_message"
            && !(target.kind() == "message"
                && matches!(
                    target_role.as_str(),
                    Some("user" | "assistant" | "toolResult")
                ))
        {
            return Err(entry_error(
                target_id,
                " does not contribute editable model content",
            ));
        }
        let replacement = if !replacement.is_null()
            && matches!(target_role.as_str(), Some("assistant" | "toolResult"))
            && replacement.get("content").is_some_and(JsValue::is_string)
        {
            object([(
                "content",
                vec![object([
                    ("type", "text".into()),
                    (
                        "text",
                        replacement.get("content").expect("validated").clone(),
                    ),
                ])]
                .into(),
            )])
        } else {
            replacement
        };
        let mut raw = self.base_entry("context_edit")?;
        raw.insert("targetId", target_id.clone().into());
        raw.insert("replacement", replacement);
        self.append_raw(raw)
    }
    pub fn is_persisted(&self) -> bool {
        self.persist
    }
    pub fn get_cwd(&self) -> &str {
        &self.cwd
    }
    pub fn get_session_dir(&self) -> &str {
        &self.session_dir
    }
    pub fn uses_default_session_dir(&self) -> bool {
        get_default_session_dir_path(&self.cwd, None, &self.config)
            .is_ok_and(|path| path == self.session_dir)
    }
    pub fn get_session_id(&self) -> JsString {
        self.session_id.clone()
    }
    pub fn get_session_file(&self) -> Option<String> {
        self.session_file.clone()
    }
    pub fn get_leaf_id(&self) -> Option<JsString> {
        self.leaf_id.clone()
    }
    pub fn get_leaf_entry(&self) -> Option<SessionEntry> {
        self.leaf_id
            .as_ref()
            .filter(|id| !id.is_empty())
            .and_then(|id| self.by_id.get(id))
            .cloned()
    }
    pub fn get_entry(&self, id: &JsString) -> Option<SessionEntry> {
        self.by_id.get(id).cloned()
    }
    pub fn get_children(&self, parent_id: &JsString) -> Vec<SessionEntry> {
        self.by_id
            .values()
            .filter(|e| e.parent_id().as_ref() == Some(parent_id))
            .cloned()
            .collect()
    }
    pub fn get_label(&self, id: &JsString) -> Option<JsString> {
        self.labels_by_id.get(id).cloned()
    }
    pub fn append_label_change(
        &mut self,
        target_id: &JsString,
        label: Option<JsString>,
    ) -> SessionResult<JsString> {
        if !self.by_id.contains_key(target_id) {
            return Err(entry_error(target_id, " not found"));
        }
        let mut raw = self.base_entry("label")?;
        raw.insert("targetId", target_id.clone().into());
        if let Some(label) = &label {
            raw.insert("label", label.clone().into());
        }
        let entry = SessionEntry::new(raw);
        let id = entry.id();
        self.append_entry(entry.clone())?;
        if let Some(label) = label.filter(|s| !s.is_empty()) {
            self.labels_by_id.insert(target_id.clone(), label);
            self.label_timestamps_by_id
                .insert(target_id.clone(), entry.timestamp());
        } else {
            self.labels_by_id.shift_remove(target_id);
            self.label_timestamps_by_id.shift_remove(target_id);
        }
        Ok(id)
    }
    pub fn get_branch(&self, from_id: Option<&JsString>) -> Vec<SessionEntry> {
        let mut current = from_id
            .or(self.leaf_id.as_ref())
            .filter(|id| !id.is_empty())
            .and_then(|id| self.by_id.get(id))
            .cloned();
        let mut path = vec![];
        while let Some(entry) = current {
            current = entry
                .parent_id()
                .filter(|id| !id.is_empty())
                .and_then(|id| self.by_id.get(&id).cloned());
            path.push(entry);
        }
        path.reverse();
        path
    }
    pub fn build_context_entries(&self) -> Vec<SessionEntry> {
        build_context_entries(
            &self.get_entries(),
            Some(self.leaf_id.as_ref()),
            Some(&self.by_id),
        )
    }
    pub fn build_session_projection(&self) -> SessionProjection {
        build_session_projection(
            &self.get_entries(),
            Some(self.leaf_id.as_ref()),
            Some(&self.by_id),
        )
    }
    pub fn build_session_context(&self) -> SessionContext {
        build_session_context(
            &self.get_entries(),
            Some(self.leaf_id.as_ref()),
            Some(&self.by_id),
        )
    }
    pub fn get_header(&self) -> Option<SessionHeader> {
        self.file_entries
            .iter()
            .find(|e| e.kind() == "session")
            .cloned()
    }
    pub fn get_entry_count(&self) -> usize {
        self.by_id.len()
    }
    pub fn get_entries(&self) -> Vec<SessionEntry> {
        self.file_entries
            .iter()
            .filter(|e| e.kind() != "session")
            .cloned()
            .collect()
    }
    pub fn get_tree(&self) -> Vec<SessionTreeNode> {
        // Build arena links first, then materialize bottom-up without recursive construction.
        let entries = self.get_entries();
        let mut by_id = IndexMap::new();
        for (index, entry) in entries.iter().enumerate() {
            by_id.insert(entry.id(), index);
        }
        let mut children = vec![vec![]; entries.len()];
        let mut roots = vec![];
        for (index, entry) in entries.iter().enumerate() {
            let parent = entry.parent_id();
            if parent.is_none() || parent.as_ref() == Some(&entry.id()) {
                roots.push(index);
            } else if let Some(parent) = parent.and_then(|id| by_id.get(&id).copied()) {
                children[parent].push(index);
            } else {
                roots.push(index);
            }
        }
        for list in &mut children {
            list.sort_by(|a, b| {
                parse_timestamp(&entries[*a].timestamp())
                    .partial_cmp(&parse_timestamp(&entries[*b].timestamp()))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        let mut built: Vec<Option<SessionTreeNode>> = (0..entries.len()).map(|_| None).collect();
        let mut stack = roots.iter().rev().map(|i| (*i, false)).collect::<Vec<_>>();
        while let Some((index, visited)) = stack.pop() {
            if !visited {
                stack.push((index, true));
                stack.extend(children[index].iter().rev().map(|i| (*i, false)));
            } else {
                let entry = entries[index].clone();
                let id = entry.id();
                built[index] = Some(SessionTreeNode {
                    entry,
                    children: children[index]
                        .iter()
                        .filter_map(|i| built[*i].take())
                        .collect(),
                    label: self.labels_by_id.get(&id).cloned(),
                    label_timestamp: self.label_timestamps_by_id.get(&id).cloned(),
                });
            }
        }
        roots.into_iter().filter_map(|i| built[i].take()).collect()
    }
    pub fn branch(&mut self, from_id: &JsString) -> SessionResult<()> {
        if !self.by_id.contains_key(from_id) {
            return Err(entry_error(from_id, " not found"));
        }
        self.leaf_id = Some(from_id.clone());
        Ok(())
    }
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }
    pub fn branch_with_summary(
        &mut self,
        branch_from_id: Option<JsString>,
        summary: impl Into<JsString>,
        details: Option<JsValue>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> SessionResult<JsString> {
        if let Some(id) = &branch_from_id
            && !self.by_id.contains_key(id)
        {
            return Err(entry_error(id, " not found"));
        }
        let from_id = self.leaf_id.clone().unwrap_or_else(|| "root".into());
        self.leaf_id = branch_from_id;
        let mut raw = self.base_entry("branch_summary")?;
        raw.insert("fromId", from_id.into());
        raw.insert("summary", summary.into().into());
        if let Some(details) = details {
            raw.insert("details", details);
        }
        if let Some(usage) = usage {
            raw.insert("usage", to_js_value(&usage).expect("usage serializes"));
        }
        if let Some(hook) = from_hook {
            raw.insert("fromHook", hook.into());
        }
        self.append_raw(raw)
    }
    pub fn create_branched_session(&mut self, leaf_id: &JsString) -> SessionResult<Option<String>> {
        let previous_file = self.session_file.clone();
        let path = self.get_branch(Some(leaf_id));
        if path.is_empty() {
            return Err(entry_error(leaf_id, " not found"));
        }
        let mut retained = vec![];
        let mut replacement_by_label = IndexMap::new();
        let mut pending = vec![];
        let mut parent = None;
        for entry in path {
            if entry.kind() == "label" {
                pending.push(entry.id());
                continue;
            }
            for label in pending.drain(..) {
                replacement_by_label.insert(label, entry.id());
            }
            let copy = entry.copy();
            copy.set(
                "parentId",
                parent.clone().map(JsValue::String).unwrap_or(JsValue::Null),
            );
            if entry.kind() == "compaction" {
                let first = string(entry.get("firstKeptEntryId").as_ref());
                if first != entry.id()
                    && let Some(replacement) = replacement_by_label.get(&first)
                {
                    copy.set("firstKeptEntryId", replacement.clone().into());
                }
            }
            parent = Some(entry.id());
            retained.push(copy);
        }
        let id = self.create_session_id()?;
        let timestamp = iso_timestamp(self.env.now_ms());
        let new_file = path_string(&Path::new(&self.session_dir).join(format!(
            "{}_{}.jsonl",
            timestamp.to_string_lossy().replace([':', '.'], "-"),
            id.to_string_lossy()
        )));
        let mut header = JsObject::from([
            ("type", "session".into()),
            ("version", 3.0.into()),
            ("id", id.clone().into()),
            ("timestamp", timestamp.into()),
            ("cwd", self.cwd.clone().into()),
        ]);
        if self.persist
            && let Some(previous) = previous_file
        {
            header.insert("parentSession", previous.into());
        }
        let mut ids = retained
            .iter()
            .map(SessionEntry::id)
            .collect::<HashSet<_>>();
        let mut labels = vec![];
        for (target, label) in &self.labels_by_id {
            if !ids.contains(target) {
                continue;
            }
            let label_id = generate_id(|id| ids.contains(id), self.env.as_ref())?;
            let raw = JsObject::from([
                ("type", "label".into()),
                ("id", label_id.clone().into()),
                (
                    "parentId",
                    parent.clone().map(JsValue::String).unwrap_or(JsValue::Null),
                ),
                (
                    "timestamp",
                    self.label_timestamps_by_id[target].clone().into(),
                ),
                ("targetId", target.clone().into()),
                ("label", label.clone().into()),
            ]);
            ids.insert(label_id.clone());
            parent = Some(label_id);
            labels.push(SessionEntry::new(raw));
        }
        self.file_entries = vec![header.into()];
        self.file_entries.extend(retained);
        self.file_entries.extend(labels);
        self.session_id = id;
        if self.persist {
            self.session_file = Some(new_file.clone());
        }
        self.build_index();
        if self.persist {
            if self.has_conversation() {
                self.rewrite_file()?;
                self.flushed = true;
            } else {
                self.flushed = false;
            }
            Ok(Some(new_file))
        } else {
            Ok(None)
        }
    }
    pub fn create(
        cwd: &str,
        session_dir: Option<&str>,
        options: Option<NewSessionOptions>,
        env: Arc<dyn PiEnv>,
        config: Arc<HostConfig>,
    ) -> SessionResult<Self> {
        let dir = match session_dir.filter(|s| !s.is_empty()) {
            Some(dir) => normalize_path(
                dir,
                &PathInputOptions {
                    home_dir: Some(path_string(&config.home_dir)),
                    ..Default::default()
                },
            )?,
            None => get_default_session_dir(cwd, None, &config)?,
        };
        Self::new(cwd, &dir, None, true, options, None, env, config)
    }
    pub fn open(
        path: &str,
        session_dir: Option<&str>,
        cwd_override: Option<&str>,
        env: Arc<dyn PiEnv>,
        config: Arc<HostConfig>,
    ) -> SessionResult<Self> {
        let path = resolve_host_path(path, &config)?;
        let mut header = None;
        let mut preloaded = None;
        if cwd_override.is_none() && Path::new(&path).exists() {
            match read_session_header(&path) {
                Ok(found) => header = found,
                Err(HeaderReadError::Io(error)) => return Err(error.into()),
                Err(HeaderReadError::Limit) => {
                    let entries = load_entries_from_file(&path)?;
                    header = entries.first().filter(|e| e.kind() == "session").cloned();
                    preloaded = Some(entries);
                }
            }
        }
        let stored = header
            .and_then(|h| h.get("cwd"))
            .and_then(|v| v.as_str().map(str::to_owned));
        let default_cwd = path_string(&config.process_cwd);
        let cwd = cwd_override.or(stored.as_deref()).unwrap_or(&default_cwd);
        let dir = match session_dir.filter(|s| !s.is_empty()) {
            Some(dir) => normalize_path(
                dir,
                &PathInputOptions {
                    home_dir: Some(path_string(&config.home_dir)),
                    ..Default::default()
                },
            )?,
            None => path_string(
                Path::new(&path)
                    .parent()
                    .expect("resolved file path has parent"),
            ),
        };
        Self::new(cwd, &dir, Some(&path), true, None, preloaded, env, config)
    }
    pub fn continue_recent(
        cwd: &str,
        session_dir: Option<&str>,
        env: Arc<dyn PiEnv>,
        config: Arc<HostConfig>,
    ) -> SessionResult<Self> {
        let dir = match session_dir.filter(|s| !s.is_empty()) {
            Some(dir) => normalize_path(
                dir,
                &PathInputOptions {
                    home_dir: Some(path_string(&config.home_dir)),
                    ..Default::default()
                },
            )?,
            None => get_default_session_dir(cwd, None, &config)?,
        };
        let filter =
            session_dir.is_some() && dir != get_default_session_dir_path(cwd, None, &config)?;
        let recent = find_most_recent_session(&dir, filter.then_some(cwd), &config);
        Self::new(cwd, &dir, recent.as_deref(), true, None, None, env, config)
    }
    pub fn in_memory(
        cwd: Option<&str>,
        options: Option<NewSessionOptions>,
        entries: Option<Vec<FileEntry>>,
        env: Arc<dyn PiEnv>,
        config: Arc<HostConfig>,
    ) -> SessionResult<Self> {
        let default_cwd = path_string(&config.process_cwd);
        Self::new(
            cwd.unwrap_or(&default_cwd),
            "",
            None,
            false,
            options,
            entries,
            env,
            config,
        )
    }
    pub fn fork_from(
        source_path: &str,
        target_cwd: &str,
        session_dir: Option<&str>,
        options: Option<NewSessionOptions>,
        env: Arc<dyn PiEnv>,
        config: Arc<HostConfig>,
    ) -> SessionResult<Self> {
        let source = resolve_host_path(source_path, &config)?;
        let target = resolve_host_path(target_cwd, &config)?;
        let entries = load_entries_from_file(&source)?;
        if entries.is_empty() {
            return Err(SessionError::new(format!(
                "Cannot fork: source session file is empty or invalid: {source}"
            )));
        }
        if !entries.iter().any(|e| e.kind() == "session") {
            return Err(SessionError::new(format!(
                "Cannot fork: source session has no header: {source}"
            )));
        }
        let dir = match session_dir.filter(|s| !s.is_empty()) {
            Some(dir) => normalize_path(
                dir,
                &PathInputOptions {
                    home_dir: Some(path_string(&config.home_dir)),
                    ..Default::default()
                },
            )?,
            None => get_default_session_dir(&target, None, &config)?,
        };
        if !Path::new(&dir).exists() {
            fs::create_dir_all(&dir)?;
        }
        let options = options.unwrap_or_default();
        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        let id = match options.id {
            Some(id) => id,
            None => config
                .uuid_generator
                .uuidv7(env.as_ref(), None)
                .map_err(|e| SessionError::new(e.to_string()))?
                .into(),
        };
        let timestamp = iso_timestamp(env.now_ms());
        let path = path_string(&Path::new(&dir).join(format!(
            "{}_{}.jsonl",
            timestamp.to_string_lossy().replace([':', '.'], "-"),
            id.to_string_lossy()
        )));
        let header = object([
            ("type", "session".into()),
            ("version", 3.0.into()),
            ("id", id.into()),
            ("timestamp", timestamp.into()),
            ("cwd", target.clone().into()),
            ("parentSession", source.into()),
        ]);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        file.write_all(stringify(&header).as_bytes())?;
        file.write_all(b"\n")?;
        for entry in entries {
            if entry.kind() != "session" {
                file.write_all(stringify(&entry.value()).as_bytes())?;
                file.write_all(b"\n")?;
            }
        }
        drop(file);
        Self::new(&target, &dir, Some(&path), true, None, None, env, config)
    }
    pub fn find_by_id(
        cwd: &str,
        id: &JsString,
        session_dir: Option<&str>,
        config: &HostConfig,
    ) -> SessionResult<Option<String>> {
        let dir = match session_dir.filter(|s| !s.is_empty()) {
            Some(dir) => normalize_path(
                dir,
                &PathInputOptions {
                    home_dir: Some(path_string(&config.home_dir)),
                    ..Default::default()
                },
            )?,
            None => get_default_session_dir(cwd, None, config)?,
        };
        let filter =
            session_dir.is_some() && dir != get_default_session_dir_path(cwd, None, config)?;
        let cwd = resolve_host_path(cwd, config)?;
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(None);
        };
        let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if !entry.file_name().to_string_lossy().ends_with(".jsonl") {
                continue;
            }
            let path = path_string(&entry.path());
            let Some(header) = discovery_header(&path) else {
                continue;
            };
            if header.id() != *id {
                continue;
            }
            if filter
                && !header
                    .get("cwd")
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .is_some_and(|stored| session_cwd_matches(&stored, &cwd, config))
            {
                continue;
            }
            return Ok(Some(path));
        }
        Ok(None)
    }
    pub async fn list(
        cwd: &str,
        session_dir: Option<&str>,
        mut on_progress: Option<&mut SessionListProgress<'_>>,
        signal: Option<&CancellationToken>,
        config: &HostConfig,
    ) -> SessionResult<Vec<SessionInfo>> {
        let dir = match session_dir.filter(|s| !s.is_empty()) {
            Some(dir) => normalize_path(
                dir,
                &PathInputOptions {
                    home_dir: Some(path_string(&config.home_dir)),
                    ..Default::default()
                },
            )?,
            None => get_default_session_dir(cwd, None, config)?,
        };
        let filter =
            session_dir.is_some() && dir != get_default_session_dir_path(cwd, None, config)?;
        let cwd = resolve_host_path(cwd, config)?;
        let include = |info: &SessionInfo| !filter || session_cwd_matches(&info.cwd, &cwd, config);
        let mut relay = |loaded, total, partial: Option<&[SessionInfo]>| {
            if let Some(progress) = on_progress.as_mut() {
                let partial = partial.map(|entries| {
                    entries
                        .iter()
                        .filter(|entry| include(entry))
                        .cloned()
                        .collect::<Vec<_>>()
                });
                progress(loaded, total, partial.as_deref());
            }
        };
        let mut sessions = list_sessions_from_dir(&dir, Some(&mut relay), signal)
            .await?
            .into_iter()
            .filter(include)
            .collect::<Vec<_>>();
        sort_session_infos(&mut sessions);
        Ok(sessions)
    }
    pub async fn list_all(
        session_dir: Option<&str>,
        on_progress: Option<&mut SessionListProgress<'_>>,
        signal: Option<&CancellationToken>,
        config: &HostConfig,
    ) -> SessionResult<Vec<SessionInfo>> {
        ensure_not_aborted(signal)?;
        if let Some(dir) = session_dir.filter(|s| !s.is_empty()) {
            let dir = normalize_path(
                dir,
                &PathInputOptions {
                    home_dir: Some(path_string(&config.home_dir)),
                    ..Default::default()
                },
            )?;
            let mut sessions = list_sessions_from_dir(&dir, on_progress, signal).await?;
            sort_session_infos(&mut sessions);
            return Ok(sessions);
        }
        let sessions_dir = config.get_sessions_dir()?;
        if !sessions_dir.exists() {
            return Ok(vec![]);
        }
        let run = async {
            use futures_util::{StreamExt, stream};
            let mut entries = fs::read_dir(&sessions_dir)?.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            let dirs = entries
                .into_iter()
                .filter(|entry| {
                    entry
                        .file_type()
                        .is_ok_and(|kind| kind.is_dir() || kind.is_symlink())
                })
                .map(|e| e.path())
                .collect::<Vec<_>>();
            let batches = stream::iter(dirs)
                .map(|dir| async move {
                    tokio::task::spawn_blocking(move || {
                        let Ok(entries) = fs::read_dir(dir) else {
                            return vec![];
                        };
                        let mut files = entries
                            .filter_map(Result::ok)
                            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
                            .map(|entry| path_string(&entry.path()))
                            .collect::<Vec<_>>();
                        files.sort();
                        files
                    })
                    .await
                    .unwrap_or_default()
                })
                .buffered(64)
                .collect::<Vec<_>>()
                .await;
            ensure_not_aborted(signal)?;
            let files = batches.into_iter().flatten().collect::<Vec<_>>();
            let mut candidates = stream::iter(files)
                .map(|path| async move {
                    let stats_path = path.clone();
                    let stats = tokio::task::spawn_blocking(move || fs::metadata(stats_path).ok())
                        .await
                        .ok()
                        .flatten();
                    (path, stats)
                })
                .buffered(64)
                .collect::<Vec<_>>()
                .await;
            candidates.sort_by(|a, b| {
                b.1.as_ref()
                    .map(mtime)
                    .unwrap_or(f64::NEG_INFINITY)
                    .partial_cmp(&a.1.as_ref().map(mtime).unwrap_or(f64::NEG_INFINITY))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        Path::new(&b.0)
                            .file_name()
                            .cmp(&Path::new(&a.0).file_name())
                    })
            });
            load_session_infos(candidates, 100, true, on_progress, signal).await
        }
        .await;
        match run {
            Ok(mut sessions) => {
                sort_session_infos(&mut sessions);
                Ok(sessions)
            }
            Err(_) => {
                ensure_not_aborted(signal)?;
                Ok(vec![])
            }
        }
    }
}
impl ReadonlySessionManager for SessionManager {
    fn get_cwd(&self) -> &str {
        SessionManager::get_cwd(self)
    }
    fn get_session_dir(&self) -> &str {
        SessionManager::get_session_dir(self)
    }
    fn get_session_id(&self) -> JsString {
        SessionManager::get_session_id(self)
    }
    fn get_session_file(&self) -> Option<String> {
        SessionManager::get_session_file(self)
    }
    fn get_branch(&self, from_id: Option<&JsString>) -> Vec<SessionEntry> {
        SessionManager::get_branch(self, from_id)
    }
    fn get_entry(&self, id: &JsString) -> Option<SessionEntry> {
        SessionManager::get_entry(self, id)
    }
    fn get_leaf_id(&self) -> Option<JsString> {
        SessionManager::get_leaf_id(self)
    }
    fn get_leaf_entry(&self) -> Option<SessionEntry> {
        SessionManager::get_leaf_entry(self)
    }
    fn get_label(&self, id: &JsString) -> Option<JsString> {
        SessionManager::get_label(self, id)
    }
    fn build_context_entries(&self) -> Vec<SessionEntry> {
        SessionManager::build_context_entries(self)
    }
    fn build_session_projection(&self) -> SessionProjection {
        SessionManager::build_session_projection(self)
    }
    fn get_header(&self) -> Option<SessionHeader> {
        SessionManager::get_header(self)
    }
    fn get_entries(&self) -> Vec<SessionEntry> {
        SessionManager::get_entries(self)
    }
    fn get_tree(&self) -> Vec<SessionTreeNode> {
        SessionManager::get_tree(self)
    }
    fn get_session_name(&self) -> Option<JsString> {
        SessionManager::get_session_name(self)
    }
}
fn entry_error(id: &JsString, suffix: &str) -> SessionError {
    let mut message = JsString::from("Entry ");
    message.push(id);
    message.push_str(suffix);
    SessionError::new(message)
}
async fn list_sessions_from_dir(
    dir: &str,
    on_progress: Option<&mut SessionListProgress<'_>>,
    signal: Option<&CancellationToken>,
) -> SessionResult<Vec<SessionInfo>> {
    ensure_not_aborted(signal)?;
    if !Path::new(dir).exists() {
        return Ok(vec![]);
    }
    let run = async {
        let mut files = fs::read_dir(dir)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
            .map(|entry| (path_string(&entry.path()), None))
            .collect::<Vec<_>>();
        files.sort_by(|a, b| b.0.cmp(&a.0));
        load_session_infos(files, 10, false, on_progress, signal).await
    }
    .await;
    match run {
        Ok(value) => Ok(value),
        Err(_) => {
            ensure_not_aborted(signal)?;
            Ok(vec![])
        }
    }
}
async fn load_session_infos(
    files: Vec<(String, Option<fs::Metadata>)>,
    interval: usize,
    wait_first: bool,
    mut on_progress: Option<&mut SessionListProgress<'_>>,
    signal: Option<&CancellationToken>,
) -> SessionResult<Vec<SessionInfo>> {
    use futures_util::{StreamExt, stream};
    let total = files.len();
    let mut partial = vec![];
    let mut loaded = 0;
    let mut first_loaded = false;
    let mut indexed = vec![];
    let mut tasks = stream::iter(files.into_iter().enumerate())
        .map(|(index, (path, stats))| {
            let signal = signal.cloned();
            async move {
                ensure_not_aborted(signal.as_ref())?;
                let result = tokio::task::spawn_blocking(move || {
                    build_session_info(&path, signal.as_ref(), stats)
                })
                .await
                .map_err(|error| SessionError::new(error.to_string()))??;
                Ok::<_, SessionError>((index, result))
            }
        })
        .buffer_unordered(10);
    while let Some(result) = tasks.next().await {
        ensure_not_aborted(signal)?;
        let (index, info) = result?;
        loaded += 1;
        if index == 0 {
            first_loaded = true;
        }
        if let Some(info) = &info {
            partial.push(info.clone());
        }
        let publish = if wait_first {
            first_loaded && (index == 0 || loaded % interval == 0 || loaded == total)
        } else {
            loaded == 1 || loaded % interval == 0 || loaded == total
        };
        if let Some(progress) = on_progress.as_mut() {
            if publish {
                let mut sorted = partial.clone();
                sort_session_infos(&mut sorted);
                progress(loaded, total, Some(&sorted));
            } else {
                progress(loaded, total, None);
            }
        }
        indexed.push((index, info));
    }
    indexed.sort_by_key(|(index, _)| *index);
    Ok(indexed.into_iter().filter_map(|(_, info)| info).collect())
}
