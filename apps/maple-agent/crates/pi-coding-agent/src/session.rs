//! The session: an append-only tree of entries.
//!
//! Every entry points at its parent and the leaf marks the current position. Appending
//! adds a child of the leaf; moving the leaf to an earlier entry and appending starts a
//! branch, so no history is ever rewritten. The model context is the path from the root
//! to the leaf, with the latest compaction standing in for everything it summarized and
//! context edits replacing or omitting earlier entries' content.

use std::collections::HashMap;
use std::fmt;
use std::io;

use pi_agent_core::AgentMessage;
use pi_ai::transcript::current_system_message;
use pi_ai::{
    AssistantContent, Content, Message, SystemMessage, ThinkingLevel, Timestamp, Usage, now_ms,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{new_entry_id, new_session_id};
use crate::messages::{
    BranchSummaryMessage, CompactionSummaryMessage, CustomMessage, SessionMessage,
};
use crate::store::{MemoryStore, SessionStore};

pub const SESSION_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub version: u32,
    pub id: String,
    pub timestamp: Timestamp,
    pub cwd: String,
    /// The session this one was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: Timestamp,
    #[serde(flatten)]
    pub kind: EntryKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum EntryKind {
    Message {
        message: SessionMessage,
    },
    ThinkingLevelChange {
        thinking_level: ThinkingLevel,
    },
    ModelChange {
        provider: String,
        model_id: String,
    },
    /// Everything before `first_kept_entry_id` is replaced by `summary` in the context.
    Compaction {
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default)]
        from_extension: bool,
        /// The complete prompt and tool state at the boundary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_message: Option<SystemMessage>,
    },
    /// A summary of the branch the conversation came back from.
    BranchSummary {
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default)]
        from_extension: bool,
    },
    /// Extension state. Never part of the model context.
    Custom {
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// An extension message that is part of the model context.
    CustomMessage {
        custom_type: String,
        content: Vec<Content>,
        display: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    /// Replace an earlier entry's content in the context, or omit it with `None`.
    ContextEdit {
        target_id: String,
        replacement: Option<Vec<Content>>,
    },
    Label {
        target_id: String,
        label: Option<String>,
    },
    SessionInfo {
        name: Option<String>,
    },
}

#[derive(Debug)]
pub enum SessionError {
    NotFound(String),
    NotOnBranch(String),
    NotEditable(String),
    Io(io::Error),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(formatter, "Entry {id} not found"),
            Self::NotOnBranch(id) => write!(formatter, "Entry {id} is not on the active branch"),
            Self::NotEditable(id) => {
                write!(
                    formatter,
                    "Entry {id} does not contribute editable model content"
                )
            }
            Self::Io(error) => write!(formatter, "Session storage failed: {error}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<io::Error> for SessionError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// One entry's contribution to the model context.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedEntry {
    pub entry_id: String,
    pub is_compaction: bool,
    /// Empty for state-only entries and omitted ones.
    pub messages: Vec<SessionMessage>,
}

/// The model context of a branch, with where each message came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionProjection {
    pub entries: Vec<ProjectedEntry>,
    pub messages: Vec<SessionMessage>,
    pub thinking_level: ThinkingLevel,
    /// `(provider, model id)` of the last model selection or response.
    pub model: Option<(String, String)>,
}

/// A node of [`SessionManager::tree`].
#[derive(Clone, Debug, PartialEq)]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub label: Option<String>,
    pub children: Vec<SessionTreeNode>,
}

/// The messages one entry contributes to the context, before edits.
pub fn entry_messages(entry: &SessionEntry) -> Vec<SessionMessage> {
    match &entry.kind {
        EntryKind::Message { message } => vec![message.clone()],
        EntryKind::CustomMessage {
            custom_type,
            content,
            display,
            details,
        } => vec![SessionMessage::Custom(CustomMessage {
            custom_type: custom_type.clone(),
            content: content.clone(),
            display: *display,
            details: details.clone(),
            timestamp: entry.timestamp,
        })],
        EntryKind::BranchSummary {
            from_id, summary, ..
        } if !summary.is_empty() => {
            vec![SessionMessage::BranchSummary(BranchSummaryMessage {
                summary: summary.clone(),
                from_id: from_id.clone(),
                timestamp: entry.timestamp,
            })]
        }
        EntryKind::Compaction {
            summary,
            tokens_before,
            system_message,
            ..
        } => {
            let mut messages: Vec<SessionMessage> = system_message
                .iter()
                .map(|system| SessionMessage::Llm(Message::System(system.clone())))
                .collect();
            messages.push(SessionMessage::CompactionSummary(
                CompactionSummaryMessage {
                    summary: summary.clone(),
                    tokens_before: *tokens_before,
                    timestamp: entry.timestamp,
                },
            ));
            messages
        }
        _ => Vec::new(),
    }
}

fn apply_edit(message: SessionMessage, replacement: &[Content]) -> SessionMessage {
    match message {
        SessionMessage::Llm(Message::User(mut user)) => {
            user.content = replacement.to_vec();
            SessionMessage::Llm(Message::User(user))
        }
        SessionMessage::Llm(Message::ToolResult(mut result)) => {
            result.content = replacement.to_vec();
            SessionMessage::Llm(Message::ToolResult(result))
        }
        SessionMessage::Llm(Message::Assistant(mut assistant)) => {
            assistant.content = replacement
                .iter()
                .filter_map(|block| block.as_text().map(AssistantContent::text))
                .collect();
            SessionMessage::Llm(Message::Assistant(assistant))
        }
        SessionMessage::Custom(mut custom) => {
            custom.content = replacement.to_vec();
            SessionMessage::Custom(custom)
        }
        other => other,
    }
}

/// The context edits among some entries, by target; a later edit of a target wins.
pub(crate) struct ContextEdits<'a> {
    edits: HashMap<&'a str, &'a Option<Vec<Content>>>,
}

impl<'a> ContextEdits<'a> {
    pub(crate) fn of(entries: &[&'a SessionEntry]) -> Self {
        let mut edits = HashMap::new();
        for entry in entries {
            if let EntryKind::ContextEdit {
                target_id,
                replacement,
            } = &entry.kind
            {
                edits.insert(target_id.as_str(), replacement);
            }
        }
        Self { edits }
    }

    /// The messages `entry` contributes once its edit is applied.
    pub(crate) fn messages(&self, entry: &SessionEntry) -> Vec<SessionMessage> {
        match self.edits.get(entry.id.as_str()) {
            Some(None) => Vec::new(),
            Some(Some(replacement)) => entry_messages(entry)
                .into_iter()
                .map(|message| apply_edit(message, replacement))
                .collect(),
            None => entry_messages(entry),
        }
    }
}

/// The entries that make up the context: with a compaction on the path, the latest one
/// comes first, then the kept entries before it and everything after it.
pub fn context_entries<'a>(path: &[&'a SessionEntry]) -> Vec<&'a SessionEntry> {
    let Some(compaction_index) = path
        .iter()
        .rposition(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
    else {
        return path.to_vec();
    };
    let compaction = path[compaction_index];
    let EntryKind::Compaction {
        first_kept_entry_id,
        ..
    } = &compaction.kind
    else {
        return path.to_vec();
    };
    let mut entries = vec![compaction];
    let mut keeping = false;
    for entry in &path[..compaction_index] {
        keeping |= entry.id == *first_kept_entry_id;
        // The compaction carries the prompt state; older system messages would repeat it.
        let is_system = matches!(&entry.kind, EntryKind::Message { message } if matches!(message.as_message(), Some(Message::System(_))));
        if keeping && !is_system {
            entries.push(*entry);
        }
    }
    entries.extend_from_slice(&path[compaction_index + 1..]);
    entries
}

/// Project a branch path into its model context.
pub fn project(path: &[&SessionEntry]) -> SessionProjection {
    let mut thinking_level = ThinkingLevel::Off;
    let mut model = None;
    for entry in path {
        match &entry.kind {
            EntryKind::ThinkingLevelChange {
                thinking_level: level,
            } => thinking_level = *level,
            EntryKind::ModelChange { provider, model_id } => {
                model = Some((provider.clone(), model_id.clone()));
            }
            EntryKind::Message {
                message: SessionMessage::Llm(Message::Assistant(assistant)),
            } => model = Some((assistant.provider.clone(), assistant.model.clone())),
            _ => {}
        }
    }
    let entries = context_entries(path);
    let edits = ContextEdits::of(&entries);
    let projected: Vec<ProjectedEntry> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let is_compaction = matches!(entry.kind, EntryKind::Compaction { .. });
            // Only the latest compaction, which leads the list, contributes its summary.
            let messages = if is_compaction && index > 0 {
                Vec::new()
            } else {
                edits.messages(entry)
            };
            ProjectedEntry {
                entry_id: entry.id.clone(),
                is_compaction,
                messages,
            }
        })
        .collect();
    SessionProjection {
        messages: projected
            .iter()
            .flat_map(|entry| entry.messages.clone())
            .collect(),
        entries: projected,
        thinking_level,
        model,
    }
}

fn is_conversation(entry: &SessionEntry) -> bool {
    matches!(
        &entry.kind,
        EntryKind::Message { message } if matches!(message.as_message(), Some(Message::User(_) | Message::Assistant(_)))
    )
}

/// The session tree of one conversation and where it is stored.
pub struct SessionManager {
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    index: HashMap<String, usize>,
    labels: HashMap<String, String>,
    leaf: Option<String>,
    store: Box<dyn SessionStore>,
    flushed: bool,
    persist_error: Option<io::Error>,
}

impl SessionManager {
    /// A new, empty session.
    pub fn create(cwd: impl Into<String>, store: Box<dyn SessionStore>) -> Self {
        let header = SessionHeader {
            version: SESSION_VERSION,
            id: new_session_id(),
            timestamp: now_ms(),
            cwd: cwd.into(),
            parent_session: None,
        };
        Self::from_parts(header, Vec::new(), store, false)
    }

    /// A session that is never stored.
    pub fn in_memory(cwd: impl Into<String>) -> Self {
        Self::create(cwd, Box::new(MemoryStore))
    }

    /// A stored session, read back. The leaf is its last entry.
    pub fn open(
        header: SessionHeader,
        entries: Vec<SessionEntry>,
        store: Box<dyn SessionStore>,
    ) -> Self {
        Self::from_parts(header, entries, store, true)
    }

    fn from_parts(
        header: SessionHeader,
        entries: Vec<SessionEntry>,
        store: Box<dyn SessionStore>,
        flushed: bool,
    ) -> Self {
        let mut manager = Self {
            header,
            entries,
            index: HashMap::new(),
            labels: HashMap::new(),
            leaf: None,
            store,
            flushed,
            persist_error: None,
        };
        manager.rebuild_index();
        manager
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        self.labels.clear();
        self.leaf = None;
        for (position, entry) in self.entries.iter().enumerate() {
            self.index.insert(entry.id.clone(), position);
            self.leaf = Some(entry.id.clone());
            if let EntryKind::Label { target_id, label } = &entry.kind {
                match label {
                    Some(label) => self.labels.insert(target_id.clone(), label.clone()),
                    None => self.labels.remove(target_id),
                };
            }
        }
    }

    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    pub fn id(&self) -> &str {
        &self.header.id
    }

    pub fn cwd(&self) -> &str {
        &self.header.cwd
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn entry(&self, id: &str) -> Option<&SessionEntry> {
        self.index.get(id).map(|position| &self.entries[*position])
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    pub fn label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(String::as_str)
    }

    pub fn children(&self, id: &str) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.parent_id.as_deref() == Some(id))
            .collect()
    }

    /// The latest session name, if one was set.
    pub fn session_name(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::SessionInfo { name } => {
                    Some(name.as_deref().filter(|name| !name.trim().is_empty()))
                }
                _ => None,
            })?
    }

    /// A storage failure since the last call. The tree in memory stays correct either way.
    pub fn take_persist_error(&mut self) -> Option<io::Error> {
        self.persist_error.take()
    }

    fn persist(&mut self) {
        let result = if self.flushed {
            match self.entries.last() {
                Some(entry) => self.store.append(entry),
                None => Ok(()),
            }
        } else if self.entries.iter().any(is_conversation) {
            self.store.write_all(&self.header, &self.entries)
        } else {
            return;
        };
        match result {
            Ok(()) => self.flushed = true,
            Err(error) => {
                // Write everything again next time, so a failed write leaves no gap.
                self.flushed = false;
                self.persist_error = Some(error);
            }
        }
    }

    fn fresh_id(&self) -> String {
        new_entry_id(|candidate| self.index.contains_key(candidate))
    }

    fn append(&mut self, kind: EntryKind) -> String {
        let id = self.fresh_id();
        self.append_with_id(id, kind)
    }

    fn append_with_id(&mut self, id: String, kind: EntryKind) -> String {
        if let EntryKind::Label { target_id, label } = &kind {
            match label {
                Some(label) => self.labels.insert(target_id.clone(), label.clone()),
                None => self.labels.remove(target_id),
            };
        }
        self.entries.push(SessionEntry {
            id: id.clone(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            kind,
        });
        self.index.insert(id.clone(), self.entries.len() - 1);
        self.leaf = Some(id.clone());
        self.persist();
        id
    }

    pub fn append_message(&mut self, message: SessionMessage) -> String {
        self.append(EntryKind::Message { message })
    }

    pub fn append_thinking_level_change(&mut self, thinking_level: ThinkingLevel) -> String {
        self.append(EntryKind::ThinkingLevelChange { thinking_level })
    }

    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> String {
        self.append(EntryKind::ModelChange {
            provider: provider.into(),
            model_id: model_id.into(),
        })
    }

    /// Record a compaction. `first_kept_entry_id: None` keeps nothing before it. The
    /// entry records the current prompt and tool state, which the summary replaces.
    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_entry_id: Option<String>,
        tokens_before: u64,
        details: Option<Value>,
        usage: Option<Usage>,
        from_extension: bool,
    ) -> String {
        let projection = self.projection();
        let messages: Vec<&Message> = projection
            .messages
            .iter()
            .filter_map(AgentMessage::as_message)
            .collect();
        let system_message = current_system_message(messages.iter().copied()).map(|mut system| {
            system.timestamp = now_ms();
            system
        });
        let id = self.fresh_id();
        let kind = EntryKind::Compaction {
            summary,
            first_kept_entry_id: first_kept_entry_id.unwrap_or_else(|| id.clone()),
            tokens_before,
            details,
            usage,
            from_extension,
            system_message,
        };
        self.append_with_id(id, kind)
    }

    pub fn append_custom_entry(&mut self, custom_type: &str, data: Option<Value>) -> String {
        self.append(EntryKind::Custom {
            custom_type: custom_type.into(),
            data,
        })
    }

    pub fn append_custom_message(
        &mut self,
        custom_type: &str,
        content: Vec<Content>,
        display: bool,
        details: Option<Value>,
    ) -> String {
        self.append(EntryKind::CustomMessage {
            custom_type: custom_type.into(),
            content,
            display,
            details,
        })
    }

    /// Replace an entry's content in the context from here on, or omit it with `None`.
    pub fn append_context_edit(
        &mut self,
        target_id: &str,
        replacement: Option<Vec<Content>>,
    ) -> Result<String, SessionError> {
        let target = self
            .entry(target_id)
            .ok_or_else(|| SessionError::NotFound(target_id.into()))?;
        let editable = match &target.kind {
            EntryKind::CustomMessage { .. } => true,
            EntryKind::Message { message } => matches!(
                message.as_message(),
                Some(Message::User(_) | Message::Assistant(_) | Message::ToolResult(_))
            ),
            _ => false,
        };
        if !editable {
            return Err(SessionError::NotEditable(target_id.into()));
        }
        if !self.branch().iter().any(|entry| entry.id == target_id) {
            return Err(SessionError::NotOnBranch(target_id.into()));
        }
        Ok(self.append(EntryKind::ContextEdit {
            target_id: target_id.into(),
            replacement,
        }))
    }

    pub fn append_label(
        &mut self,
        target_id: &str,
        label: Option<String>,
    ) -> Result<String, SessionError> {
        if !self.index.contains_key(target_id) {
            return Err(SessionError::NotFound(target_id.into()));
        }
        Ok(self.append(EntryKind::Label {
            target_id: target_id.into(),
            label,
        }))
    }

    pub fn append_session_info(&mut self, name: Option<String>) -> String {
        self.append(EntryKind::SessionInfo { name })
    }

    /// The path from the root to the leaf.
    pub fn branch(&self) -> Vec<&SessionEntry> {
        match &self.leaf {
            Some(leaf) => self.branch_to(leaf),
            None => Vec::new(),
        }
    }

    /// The path from the root to `id`.
    pub fn branch_to(&self, id: &str) -> Vec<&SessionEntry> {
        let mut path = Vec::new();
        let mut current = self.entry(id);
        while let Some(entry) = current {
            path.push(entry);
            current = entry
                .parent_id
                .as_deref()
                .and_then(|parent| self.entry(parent));
        }
        path.reverse();
        path
    }

    /// Move the leaf to `id`; the next entry starts a branch there.
    pub fn set_leaf(&mut self, id: &str) -> Result<(), SessionError> {
        if !self.index.contains_key(id) {
            return Err(SessionError::NotFound(id.into()));
        }
        self.leaf = Some(id.into());
        Ok(())
    }

    /// Move the leaf before the first entry; the next entry is a new root.
    pub fn reset_leaf(&mut self) {
        self.leaf = None;
    }

    /// Move the leaf to `from` and record a summary of the branch being left.
    pub fn branch_with_summary(
        &mut self,
        from: Option<&str>,
        summary: String,
        details: Option<Value>,
        usage: Option<Usage>,
        from_extension: bool,
    ) -> Result<String, SessionError> {
        if let Some(from) = from
            && !self.index.contains_key(from)
        {
            return Err(SessionError::NotFound(from.into()));
        }
        let from_id = self.leaf.clone().unwrap_or_else(|| "root".into());
        self.leaf = from.map(str::to_string);
        Ok(self.append(EntryKind::BranchSummary {
            from_id,
            summary,
            details,
            usage,
            from_extension,
        }))
    }

    pub fn projection(&self) -> SessionProjection {
        project(&self.branch())
    }

    /// The whole tree, children ordered oldest first.
    pub fn tree(&self) -> Vec<SessionTreeNode> {
        let mut children: HashMap<Option<&str>, Vec<&SessionEntry>> = HashMap::new();
        for entry in &self.entries {
            let parent = entry
                .parent_id
                .as_deref()
                .filter(|parent| self.index.contains_key(*parent) && *parent != entry.id);
            children.entry(parent).or_default().push(entry);
        }
        fn build(
            manager: &SessionManager,
            children: &HashMap<Option<&str>, Vec<&SessionEntry>>,
            parent: Option<&str>,
        ) -> Vec<SessionTreeNode> {
            let mut nodes: Vec<&SessionEntry> = children.get(&parent).cloned().unwrap_or_default();
            nodes.sort_by_key(|entry| entry.timestamp);
            nodes
                .into_iter()
                .map(|entry| SessionTreeNode {
                    entry: entry.clone(),
                    label: manager.labels.get(&entry.id).cloned(),
                    children: build(manager, children, Some(&entry.id)),
                })
                .collect()
        }
        build(self, &children, None)
    }

    /// Turn this into a new session holding only the path to `leaf_id`, stored in
    /// `store`. Labels on the path are carried over. A failed write is reported by
    /// [`Self::take_persist_error`], like any other.
    pub fn fork(
        &mut self,
        leaf_id: &str,
        store: Box<dyn SessionStore>,
    ) -> Result<(), SessionError> {
        if !self.index.contains_key(leaf_id) {
            return Err(SessionError::NotFound(leaf_id.into()));
        }
        let mut entries: Vec<SessionEntry> = Vec::new();
        // Label entries are rewritten at the end. A compaction that keeps entries from a
        // label on keeps them from the next entry instead.
        let mut dropped_labels: Vec<String> = Vec::new();
        let mut kept_from: HashMap<String, String> = HashMap::new();
        for entry in self.branch_to(leaf_id) {
            if matches!(entry.kind, EntryKind::Label { .. }) {
                dropped_labels.push(entry.id.clone());
                continue;
            }
            for label in dropped_labels.drain(..) {
                kept_from.insert(label, entry.id.clone());
            }
            let mut entry = entry.clone();
            if let EntryKind::Compaction {
                first_kept_entry_id,
                ..
            } = &mut entry.kind
                && let Some(next) = kept_from.get(first_kept_entry_id)
            {
                *first_kept_entry_id = next.clone();
            }
            entry.parent_id = entries.last().map(|previous| previous.id.clone());
            entries.push(entry);
        }
        let mut parent = entries.last().map(|entry| entry.id.clone());
        let mut labels: Vec<(&String, &String)> = self
            .labels
            .iter()
            .filter(|(target, _)| entries.iter().any(|entry| entry.id == **target))
            .collect();
        labels.sort();
        let label_entries: Vec<SessionEntry> = labels
            .into_iter()
            .map(|(target, label)| {
                let id =
                    new_entry_id(|candidate| entries.iter().any(|entry| entry.id == candidate));
                SessionEntry {
                    id: id.clone(),
                    parent_id: parent.replace(id),
                    timestamp: now_ms(),
                    kind: EntryKind::Label {
                        target_id: target.clone(),
                        label: Some(label.clone()),
                    },
                }
            })
            .collect();
        entries.extend(label_entries);
        self.header = SessionHeader {
            version: SESSION_VERSION,
            id: new_session_id(),
            timestamp: now_ms(),
            cwd: self.header.cwd.clone(),
            parent_session: Some(self.header.id.clone()),
        };
        self.entries = entries;
        self.store = store;
        self.flushed = false;
        self.rebuild_index();
        self.persist();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::JsonlStore;
    use pi_ai::{AssistantMessage, StopReason, UserMessage};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    fn user(text: &str) -> SessionMessage {
        SessionMessage::Llm(Message::User(UserMessage::text(text)))
    }

    fn assistant(text: &str) -> SessionMessage {
        let model = pi_ai::faux::FauxProvider::default_model();
        SessionMessage::Llm(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::text(text)],
            stop_reason: StopReason::Stop,
            ..AssistantMessage::empty(&model)
        }))
    }

    fn texts(manager: &SessionManager) -> Vec<String> {
        manager
            .projection()
            .messages
            .iter()
            .map(SessionMessage::text)
            .collect()
    }

    #[test]
    fn entries_chain_from_the_leaf_and_build_the_context() {
        let mut session = SessionManager::in_memory("/work");
        let first = session.append_message(user("hello"));
        session.append_thinking_level_change(ThinkingLevel::High);
        let reply = session.append_message(assistant("hi"));
        let path: Vec<String> = session
            .branch()
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        assert_eq!(path.len(), 3);
        assert_eq!(
            (path[0].as_str(), path[2].as_str()),
            (first.as_str(), reply.as_str())
        );

        let projection = session.projection();
        assert_eq!(texts(&session), ["hello", "hi"]);
        assert_eq!(projection.thinking_level, ThinkingLevel::High);
        assert_eq!(projection.model, Some(("faux".into(), "faux-1".into())));
    }

    #[test]
    fn branching_keeps_the_old_path_and_starts_a_new_one() {
        let mut session = SessionManager::in_memory("/work");
        let question = session.append_message(user("question"));
        let first_answer = session.append_message(assistant("first answer"));
        session.set_leaf(&question).unwrap();
        session.append_message(assistant("second answer"));

        assert_eq!(texts(&session), ["question", "second answer"]);
        assert_eq!(session.children(&question).len(), 2);
        assert!(session.entry(&first_answer).is_some());
        let tree = session.tree();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].children.len(), 2);

        let summary = session
            .branch_with_summary(
                Some(&question),
                "tried the second answer".into(),
                None,
                None,
                false,
            )
            .unwrap();
        assert_eq!(texts(&session)[1], "tried the second answer");
        assert_eq!(session.leaf_id(), Some(summary.as_str()));
    }

    #[test]
    fn compaction_replaces_what_it_summarized() {
        let mut session = SessionManager::in_memory("/work");
        session.append_message(user("old question"));
        session.append_message(assistant("old answer"));
        let kept = session.append_message(user("recent question"));
        session.append_message(assistant("recent answer"));
        session.append_compaction("summary of old".into(), Some(kept), 1234, None, None, false);
        session.append_message(user("next"));

        let messages = session.projection().messages;
        assert_eq!(messages[0].role(), "compactionSummary");
        let texts: Vec<String> = messages.iter().map(SessionMessage::text).collect();
        assert_eq!(
            texts,
            ["summary of old", "recent question", "recent answer", "next"]
        );
    }

    #[test]
    fn context_edits_replace_or_omit_content_on_the_branch() {
        let mut session = SessionManager::in_memory("/work");
        let question = session.append_message(user("my password is hunter2"));
        let answer = session.append_message(assistant("noted"));
        session
            .append_context_edit(
                &question,
                Some(vec![Content::text("my password is [redacted]")]),
            )
            .unwrap();
        session.append_context_edit(&answer, None).unwrap();
        assert_eq!(texts(&session), ["my password is [redacted]"]);

        let info = session.append_session_info(Some("named".into()));
        assert!(matches!(
            session.append_context_edit(&info, None),
            Err(SessionError::NotEditable(_))
        ));
        assert!(matches!(
            session.append_context_edit("nope", None),
            Err(SessionError::NotFound(_))
        ));
    }

    #[test]
    fn labels_and_names_resolve_to_the_latest_value() {
        let mut session = SessionManager::in_memory("/work");
        let question = session.append_message(user("q"));
        session
            .append_label(&question, Some("start".into()))
            .unwrap();
        assert_eq!(session.label(&question), Some("start"));
        session.append_label(&question, None).unwrap();
        assert_eq!(session.label(&question), None);
        session.append_session_info(Some("first".into()));
        session.append_session_info(Some("second".into()));
        assert_eq!(session.session_name(), Some("second"));
    }

    #[test]
    fn a_file_is_written_only_once_there_is_a_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut session = SessionManager::create("/work", Box::new(JsonlStore::new(&path)));
        session.append_model_change("faux", "faux-1");
        assert!(!path.exists());
        session.append_message(user("hello"));
        session.append_custom_entry("ext", Some(json!({ "state": 1 })));
        assert!(path.exists());

        let (header, entries) = JsonlStore::load(&path).unwrap().unwrap();
        assert_eq!(header.id, session.id());
        assert_eq!(entries, session.entries());
        let reopened = SessionManager::open(header, entries, Box::new(JsonlStore::new(&path)));
        assert_eq!(reopened.leaf_id(), session.leaf_id());
        assert_eq!(texts(&reopened), ["hello"]);
    }

    #[test]
    fn loading_rejects_files_that_are_not_sessions_and_skips_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = dir.path().join("notes.jsonl");
        std::fs::write(&foreign, "{\"hello\":1}\n").unwrap();
        assert!(JsonlStore::load(&foreign).is_err());
        assert!(
            JsonlStore::load(&dir.path().join("missing.jsonl"))
                .unwrap()
                .is_none()
        );

        let path = dir.path().join("s.jsonl");
        let mut session = SessionManager::create("/work", Box::new(JsonlStore::new(&path)));
        session.append_message(user("hello"));
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{not json\n");
        std::fs::write(&path, text).unwrap();
        let (_, entries) = JsonlStore::load(&path).unwrap().unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn forking_keeps_only_the_path_and_its_labels() {
        let mut session = SessionManager::in_memory("/work");
        let question = session.append_message(user("question"));
        session
            .append_label(&question, Some("here".into()))
            .unwrap();
        let answer = session.append_message(assistant("answer"));
        session.set_leaf(&question).unwrap();
        session.append_message(assistant("other answer"));
        let original = session.id().to_string();

        session.fork(&answer, Box::new(MemoryStore)).unwrap();
        assert_eq!(
            session.header().parent_session.as_deref(),
            Some(original.as_str())
        );
        assert_eq!(texts(&session), ["question", "answer"]);
        assert_eq!(session.label(&question), Some("here"));
        assert!(session.entries().iter().all(|entry| !matches!(&entry.kind, EntryKind::Message { message } if message.text() == "other answer")));
    }

    #[test]
    fn a_fork_keeps_what_a_compaction_kept_from_a_label() {
        let mut session = SessionManager::in_memory("/work");
        session.append_message(user("old question"));
        let old_answer = session.append_message(assistant("old answer"));
        let label = session
            .append_label(&old_answer, Some("checkpoint".into()))
            .unwrap();
        session.append_message(user("recent question"));
        session.append_message(assistant("recent answer"));
        session.append_compaction("summary".into(), Some(label), 1, None, None, false);
        let leaf = session.append_message(user("next"));
        let before = texts(&session);
        assert_eq!(
            before,
            ["summary", "recent question", "recent answer", "next"]
        );

        session.fork(&leaf, Box::new(MemoryStore)).unwrap();
        assert_eq!(texts(&session), before);
    }

    /// A store that fails while `failing` is set, and otherwise records entry ids.
    struct FlakyStore {
        failing: Arc<AtomicBool>,
        stored: Arc<Mutex<Vec<String>>>,
    }

    impl SessionStore for FlakyStore {
        fn write_all(
            &mut self,
            _header: &SessionHeader,
            entries: &[SessionEntry],
        ) -> io::Result<()> {
            if self.failing.load(Ordering::SeqCst) {
                return Err(io::Error::other("disk full"));
            }
            *self.stored.lock().unwrap() = entries.iter().map(|entry| entry.id.clone()).collect();
            Ok(())
        }

        fn append(&mut self, entry: &SessionEntry) -> io::Result<()> {
            if self.failing.load(Ordering::SeqCst) {
                return Err(io::Error::other("disk full"));
            }
            self.stored.lock().unwrap().push(entry.id.clone());
            Ok(())
        }
    }

    #[test]
    fn after_a_failed_write_the_next_one_stores_everything() {
        let failing = Arc::new(AtomicBool::new(true));
        let stored = Arc::new(Mutex::new(Vec::new()));
        let mut session = SessionManager::create(
            "/work",
            Box::new(FlakyStore {
                failing: failing.clone(),
                stored: stored.clone(),
            }),
        );
        let ids = |session: &SessionManager| -> Vec<String> {
            session
                .entries()
                .iter()
                .map(|entry| entry.id.clone())
                .collect()
        };

        session.append_message(user("hello"));
        assert!(session.take_persist_error().is_some());
        failing.store(false, Ordering::SeqCst);
        session.append_message(assistant("hi"));
        assert_eq!(*stored.lock().unwrap(), ids(&session));

        failing.store(true, Ordering::SeqCst);
        session.append_message(user("again"));
        assert!(session.take_persist_error().is_some());
        failing.store(false, Ordering::SeqCst);
        session.append_message(assistant("ok"));
        assert_eq!(*stored.lock().unwrap(), ids(&session));
        assert!(session.take_persist_error().is_none());
    }

    #[test]
    fn a_line_cut_off_by_a_crash_costs_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut session = SessionManager::create("/work", Box::new(JsonlStore::new(&path)));
        session.append_message(user("hello"));
        // Cut inside a two-byte character, so the line is not even valid UTF-8.
        let partial = "{\"id\":\"x\",\"text\":\"é";
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&partial.as_bytes()[..partial.len() - 1]);
        std::fs::write(&path, bytes).unwrap();

        session.append_message(assistant("hi"));
        let (_, entries) = JsonlStore::load(&path).unwrap().unwrap();
        assert_eq!(entries, session.entries());
    }
}
