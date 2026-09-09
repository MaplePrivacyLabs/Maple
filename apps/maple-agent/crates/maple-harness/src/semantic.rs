use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    ops::Range,
    str::FromStr,
};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
use thiserror::Error;

use crate::{
    ActionError, ActionId, ActionStatus, Availability, InvocationId, OperationId,
    ProgrammabilityAuthority, UiControllerAccess,
};

pub const DEFAULT_SEMANTIC_EVENT_CAPACITY: usize = 2_048;
pub const DEFAULT_SEMANTIC_NODE_PAGE_LIMIT: usize = 200;
pub const DEFAULT_SNAPSHOT_TEXT_LIMIT_BYTES: usize = 128 * 1024;
pub const MAX_EVENT_WAIT_TIMEOUT_MS: u64 = 60_000;

macro_rules! semantic_name {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        #[schemars(transparent)]
        pub struct $name(
            #[schemars(length(min = 1, max = 64), pattern(r"^[a-z][a-z0-9_.]*$"))] String,
        );

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, SemanticNameError> {
                let value = value.into();
                validate_semantic_name(&value, $label)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = SemanticNameError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = SemanticNameError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(de::Error::custom)
            }
        }
    };
}

semantic_name!(ScreenId, "screen ID");
semantic_name!(RegionId, "region ID");
semantic_name!(SemanticKind, "semantic kind");

impl ScreenId {
    pub fn login() -> Self {
        Self::parse("login").expect("built-in screen ID is valid")
    }

    pub fn chat() -> Self {
        Self::parse("chat").expect("built-in screen ID is valid")
    }

    pub fn settings() -> Self {
        Self::parse("settings").expect("built-in screen ID is valid")
    }

    pub fn code_mode() -> Self {
        Self::parse("code_mode").expect("built-in screen ID is valid")
    }
}

impl RegionId {
    pub fn app() -> Self {
        Self::parse("app").expect("built-in region ID is valid")
    }

    pub fn sidebar() -> Self {
        Self::parse("sidebar").expect("built-in region ID is valid")
    }

    pub fn transcript() -> Self {
        Self::parse("transcript").expect("built-in region ID is valid")
    }

    pub fn composer() -> Self {
        Self::parse("composer").expect("built-in region ID is valid")
    }

    pub fn settings() -> Self {
        Self::parse("settings").expect("built-in region ID is valid")
    }

    pub fn overlay() -> Self {
        Self::parse("overlay").expect("built-in region ID is valid")
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SemanticNameError {
    #[error("{kind} is empty")]
    Empty { kind: &'static str },
    #[error("{kind} exceeds 64 bytes")]
    TooLong { kind: &'static str },
    #[error("{kind} must start with an ASCII lowercase letter")]
    InvalidStart { kind: &'static str },
    #[error("{kind} contains invalid character {character:?} at byte {byte_index}")]
    InvalidCharacter {
        kind: &'static str,
        byte_index: usize,
        character: char,
    },
}

fn validate_semantic_name(value: &str, kind: &'static str) -> Result<(), SemanticNameError> {
    if value.is_empty() {
        return Err(SemanticNameError::Empty { kind });
    }
    if value.len() > 64 {
        return Err(SemanticNameError::TooLong { kind });
    }
    if !value.as_bytes()[0].is_ascii_lowercase() {
        return Err(SemanticNameError::InvalidStart { kind });
    }
    for (byte_index, character) in value.char_indices() {
        if !(character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '_'
            || character == '.')
        {
            return Err(SemanticNameError::InvalidCharacter {
                kind,
                byte_index,
                character,
            });
        }
    }
    Ok(())
}

/// A stable target. It intentionally contains no render indices, GPUI entity
/// IDs, focus handles, or other ephemeral UI identities.
#[derive(
    Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SemanticTarget {
    App,
    Screen {
        screen: ScreenId,
    },
    Region {
        region: RegionId,
    },
    Project {
        canonical_root: String,
    },
    Task {
        task_id: String,
    },
    TimelineItem {
        task_id: String,
        item_id: String,
    },
    Annotation {
        task_id: String,
        item_id: String,
        annotation_id: String,
    },
    Permission {
        request_id: String,
    },
    Question {
        request_id: String,
        question_id: String,
    },
    QueueItem {
        task_id: String,
        queue_id: String,
    },
    DraftAttachment {
        draft_id: u64,
    },
    Setting {
        key: String,
    },
    MenuItem {
        menu_id: String,
        item_id: String,
    },
}

impl SemanticTarget {
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::Task { task_id }
            | Self::TimelineItem { task_id, .. }
            | Self::Annotation { task_id, .. }
            | Self::QueueItem { task_id, .. } => Some(task_id),
            _ => None,
        }
    }

    /// Human-readable display path for logs/UI only. Executors must continue to
    /// use the structural enum rather than parsing this string.
    pub fn display_path(&self) -> String {
        match self {
            Self::App => "app".into(),
            Self::Screen { screen } => format!("screen:{screen}"),
            Self::Region { region } => format!("region:{region}"),
            Self::Project { canonical_root } => format!("project:{canonical_root}"),
            Self::Task { task_id } => format!("task:{task_id}"),
            Self::TimelineItem { task_id, item_id } => {
                format!("task:{task_id}/timeline:{item_id}")
            }
            Self::Annotation {
                task_id,
                item_id,
                annotation_id,
            } => format!("task:{task_id}/timeline:{item_id}/annotation:{annotation_id}"),
            Self::Permission { request_id } => format!("permission:{request_id}"),
            Self::Question {
                request_id,
                question_id,
            } => format!("request:{request_id}/question:{question_id}"),
            Self::QueueItem { task_id, queue_id } => {
                format!("task:{task_id}/queue:{queue_id}")
            }
            Self::DraftAttachment { draft_id } => format!("draft_attachment:{draft_id}"),
            Self::Setting { key } => format!("setting:{key}"),
            Self::MenuItem { menu_id, item_id } => format!("menu:{menu_id}/item:{item_id}"),
        }
    }
}

#[derive(
    Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RevisionDomain {
    Global,
    Task {
        task_id: String,
    },
    Project {
        canonical_root: String,
    },
    Timeline {
        task_id: String,
    },
    TimelineItem {
        task_id: String,
        item_id: String,
    },
    Draft {
        task_id: String,
    },
    Setting {
        key: String,
    },
    Permission {
        request_id: String,
    },
    Question {
        request_id: String,
        question_id: String,
    },
    QueueItem {
        task_id: String,
        queue_id: String,
    },
    Target {
        target: SemanticTarget,
    },
}

impl RevisionDomain {
    /// Collapses the structural aliases that can otherwise give one semantic
    /// object two independent revision counters.
    pub fn canonicalized(&self) -> Self {
        match self {
            Self::Target { target } => match target {
                SemanticTarget::App => Self::Global,
                SemanticTarget::Project { canonical_root } => Self::Project {
                    canonical_root: canonical_root.clone(),
                },
                SemanticTarget::Task { task_id } => Self::Task {
                    task_id: task_id.clone(),
                },
                SemanticTarget::TimelineItem { task_id, item_id } => Self::TimelineItem {
                    task_id: task_id.clone(),
                    item_id: item_id.clone(),
                },
                SemanticTarget::Permission { request_id } => Self::Permission {
                    request_id: request_id.clone(),
                },
                SemanticTarget::Question {
                    request_id,
                    question_id,
                } => Self::Question {
                    request_id: request_id.clone(),
                    question_id: question_id.clone(),
                },
                SemanticTarget::QueueItem { task_id, queue_id } => Self::QueueItem {
                    task_id: task_id.clone(),
                    queue_id: queue_id.clone(),
                },
                SemanticTarget::Setting { key } => Self::Setting { key: key.clone() },
                _ => self.clone(),
            },
            _ => self.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RevisionChange {
    pub state_revision: u64,
    pub target_revision: u64,
}

/// Tracks the coherent global state revision separately from action-specific
/// target revisions.
#[derive(Clone, Debug, Default)]
pub struct RevisionTracker {
    state_revision: u64,
    domains: BTreeMap<RevisionDomain, u64>,
}

impl RevisionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state_revision(&self) -> u64 {
        self.state_revision
    }

    pub fn revision(&self, domain: &RevisionDomain) -> u64 {
        let domain = domain.canonicalized();
        if domain == RevisionDomain::Global {
            self.state_revision
        } else {
            self.domains.get(&domain).copied().unwrap_or(0)
        }
    }

    pub fn bump(&mut self, domain: RevisionDomain) -> RevisionChange {
        let domain = domain.canonicalized();
        self.state_revision = self.state_revision.saturating_add(1);
        let target_revision = if domain == RevisionDomain::Global {
            self.state_revision
        } else {
            let revision = self.domains.entry(domain).or_default();
            *revision = revision.saturating_add(1);
            *revision
        };
        RevisionChange {
            state_revision: self.state_revision,
            target_revision,
        }
    }

    pub fn check(&self, domain: &RevisionDomain, expected: u64) -> Result<(), StaleRevision> {
        let domain = domain.canonicalized();
        let actual = self.revision(&domain);
        if actual == expected {
            Ok(())
        } else {
            Err(StaleRevision {
                domain,
                expected,
                actual,
            })
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("revision for {domain:?} changed from {expected} to {actual}")]
pub struct StaleRevision {
    pub domain: RevisionDomain,
    pub expected: u64,
    pub actual: u64,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsertionSummary {
    pub target: SemanticTarget,
    pub byte_offset: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_byte_range: Option<Range<usize>>,
    pub draft_revision: u64,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewportSummary {
    pub region: RegionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_visible: Option<SemanticTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_visible: Option<SemanticTarget>,
    pub visible_count: usize,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionAvailabilitySummary {
    pub action_id: ActionId,
    pub availability: Availability,
}

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticNodeState {
    pub selected: bool,
    pub expanded: bool,
    pub disabled: bool,
    pub busy: bool,
    /// Small, already-redacted semantic attributes. Never place arbitrary
    /// application state or credentials here.
    pub attributes: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticNode {
    pub target: SemanticTarget,
    pub kind: SemanticKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub state: SemanticNodeState,
    #[serde(default)]
    pub available_actions: Vec<ActionAvailabilitySummary>,
    #[serde(default)]
    pub children: Vec<SemanticNode>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq)]
pub struct SemanticSnapshot {
    pub schema_version: u16,
    pub state_revision: u64,
    pub event_cursor: u64,
    pub screen: ScreenId,
    pub active_region: RegionId,
    pub selection: Option<SemanticTarget>,
    pub insertion: Option<InsertionSummary>,
    pub viewport: ViewportSummary,
    pub stream_follow: bool,
    pub roots: Vec<SemanticNode>,
}

impl SemanticSnapshot {
    /// Rejects secret-shaped attribute keys before a snapshot crosses the
    /// controller boundary. Projection labels still remain the host's
    /// responsibility and should contain only visible product text.
    pub fn validate_redaction(&self) -> Result<(), SnapshotRedactionError> {
        for root in &self.roots {
            validate_node_attributes(root)?;
        }
        Ok(())
    }
}

impl Serialize for SemanticSnapshot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::Error as _;

        self.validate_redaction().map_err(S::Error::custom)?;
        #[derive(Serialize)]
        #[serde(deny_unknown_fields)]
        struct Wire<'a> {
            schema_version: u16,
            state_revision: u64,
            event_cursor: u64,
            screen: &'a ScreenId,
            active_region: &'a RegionId,
            #[serde(skip_serializing_if = "Option::is_none")]
            selection: &'a Option<SemanticTarget>,
            #[serde(skip_serializing_if = "Option::is_none")]
            insertion: &'a Option<InsertionSummary>,
            viewport: &'a ViewportSummary,
            stream_follow: bool,
            roots: &'a [SemanticNode],
        }

        Wire {
            schema_version: self.schema_version,
            state_revision: self.state_revision,
            event_cursor: self.event_cursor,
            screen: &self.screen,
            active_region: &self.active_region,
            selection: &self.selection,
            insertion: &self.insertion,
            viewport: &self.viewport,
            stream_follow: self.stream_follow,
            roots: &self.roots,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SemanticSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u16,
            state_revision: u64,
            event_cursor: u64,
            screen: ScreenId,
            active_region: RegionId,
            #[serde(default)]
            selection: Option<SemanticTarget>,
            #[serde(default)]
            insertion: Option<InsertionSummary>,
            viewport: ViewportSummary,
            stream_follow: bool,
            #[serde(default)]
            roots: Vec<SemanticNode>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let snapshot = Self {
            schema_version: wire.schema_version,
            state_revision: wire.state_revision,
            event_cursor: wire.event_cursor,
            screen: wire.screen,
            active_region: wire.active_region,
            selection: wire.selection,
            insertion: wire.insertion,
            viewport: wire.viewport,
            stream_follow: wire.stream_follow,
            roots: wire.roots,
        };
        snapshot.validate_redaction().map_err(de::Error::custom)?;
        Ok(snapshot)
    }
}

fn validate_node_attributes(node: &SemanticNode) -> Result<(), SnapshotRedactionError> {
    for (key, value) in &node.state.attributes {
        validate_attribute_value(key, value)?;
    }
    for child in &node.children {
        validate_node_attributes(child)?;
    }
    Ok(())
}

fn validate_attribute_value(key: &str, value: &Value) -> Result<(), SnapshotRedactionError> {
    if semantic_field_is_sensitive(key) {
        return Err(SnapshotRedactionError::SensitiveAttribute(key.to_owned()));
    }
    match value {
        Value::Object(object) => {
            for (nested_key, nested_value) in object {
                validate_attribute_value(nested_key, nested_value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_attribute_value(key, value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn semantic_field_is_sensitive(field: &str) -> bool {
    let normalized = field.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "credential",
        "authorization",
        "cookie",
        "oauth",
        "callback",
        "private_key",
        "api_key",
        "header",
        "environment",
        "permission_payload",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SnapshotRedactionError {
    #[error("semantic snapshot attribute {0:?} appears sensitive")]
    SensitiveAttribute(String),
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticSelection {
    pub region: RegionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<SemanticTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<SemanticTarget>,
    pub explicit: bool,
    pub observed_revision: u64,
}

impl SemanticSelection {
    pub fn new(region: RegionId, target: Option<SemanticTarget>, observed_revision: u64) -> Self {
        Self {
            region,
            target,
            anchor: None,
            explicit: false,
            observed_revision,
        }
    }

    pub fn explicit(region: RegionId, target: SemanticTarget, observed_revision: u64) -> Self {
        Self {
            region,
            target: Some(target),
            anchor: None,
            explicit: true,
            observed_revision,
        }
    }

    pub fn select(&mut self, target: SemanticTarget, observed_revision: u64, explicit: bool) {
        self.target = Some(target);
        self.explicit = explicit;
        self.observed_revision = observed_revision;
    }

    pub fn clear(&mut self, observed_revision: u64) {
        self.target = None;
        self.anchor = None;
        self.explicit = false;
        self.observed_revision = observed_revision;
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegionReturnReason {
    Overlay,
    Dialog,
    EnterComposer,
    RegionNavigation,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegionReturnFrame {
    pub token: RegionReturnToken,
    pub screen: ScreenId,
    pub region: RegionId,
    pub target: Option<SemanticTarget>,
    pub reason: RegionReturnReason,
    pub relevant_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RegionReturnToken {
    stack_id: uuid::Uuid,
    sequence: u64,
}

impl RegionReturnToken {
    pub fn get(self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionReturnPush {
    pub token: RegionReturnToken,
    pub truncated_oldest: bool,
}

#[derive(Debug)]
pub struct RegionReturnStack {
    capacity: usize,
    stack_id: uuid::Uuid,
    next_token: u64,
    frames: VecDeque<RegionReturnFrame>,
}

impl RegionReturnStack {
    pub fn new(capacity: usize) -> Result<Self, RegionReturnStackError> {
        if capacity == 0 {
            return Err(RegionReturnStackError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            stack_id: uuid::Uuid::new_v4(),
            next_token: 1,
            frames: VecDeque::with_capacity(capacity),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        screen: ScreenId,
        region: RegionId,
        target: Option<SemanticTarget>,
        reason: RegionReturnReason,
        relevant_revision: u64,
    ) -> RegionReturnPush {
        let token = RegionReturnToken {
            stack_id: self.stack_id,
            sequence: self.next_token,
        };
        self.next_token = self.next_token.saturating_add(1);
        let truncated_oldest = self.frames.len() == self.capacity;
        if truncated_oldest {
            self.frames.pop_front();
        }
        self.frames.push_back(RegionReturnFrame {
            token,
            screen,
            region,
            target,
            reason,
            relevant_revision,
        });
        RegionReturnPush {
            token,
            truncated_oldest,
        }
    }

    /// Pops only the exact top frame. It never searches through and removes an
    /// unrelated lower overlay/composer return point.
    pub fn pop_exact(
        &mut self,
        token: RegionReturnToken,
    ) -> Result<RegionReturnFrame, RegionReturnStackError> {
        let top = self.frames.back().ok_or(RegionReturnStackError::Empty)?;
        if top.token != token {
            return Err(RegionReturnStackError::TokenMismatch {
                expected_top: top.token,
                requested: token,
            });
        }
        self.frames.pop_back().ok_or(RegionReturnStackError::Empty)
    }

    pub fn top(&self) -> Option<&RegionReturnFrame> {
        self.frames.back()
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RegionReturnStackError {
    #[error("region return stack capacity must be greater than zero")]
    ZeroCapacity,
    #[error("region return stack is empty")]
    Empty,
    #[error("region return token {requested:?} does not match top frame {expected_top:?}")]
    TokenMismatch {
        expected_top: RegionReturnToken,
        requested: RegionReturnToken,
    },
}

/// Eligibility flags used to derive a navigable projection from a raw backing
/// collection. This keeps hidden/internal/zero-presence rows out of semantic
/// navigation while allowing off-screen but revealable objects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticPresence {
    pub internal_only: bool,
    pub filtered_out: bool,
    pub revealable: bool,
    pub nonzero_presence: bool,
}

impl SemanticPresence {
    pub const fn navigable(self) -> bool {
        !self.internal_only && !self.filtered_out && self.revealable && self.nonzero_presence
    }
}

pub fn navigable_targets<'a>(
    candidates: impl IntoIterator<Item = (&'a SemanticTarget, SemanticPresence)>,
) -> Vec<SemanticTarget> {
    candidates
        .into_iter()
        .filter(|(_, presence)| presence.navigable())
        .map(|(target, _)| target.clone())
        .collect()
}

/// Reconciles a removed selection through stable IDs. It prefers the next item
/// at the removed item's former position, then the previous surviving item.
pub fn reconcile_removed_selection(
    previous_order: &[SemanticTarget],
    current_order: &[SemanticTarget],
    selected: &SemanticTarget,
) -> Option<SemanticTarget> {
    if current_order.contains(selected) {
        return Some(selected.clone());
    }
    let old_index = previous_order
        .iter()
        .position(|target| target == selected)?;

    for distance in 1..=previous_order.len() {
        if let Some(candidate) = previous_order.get(old_index + distance)
            && current_order.contains(candidate)
        {
            return Some(candidate.clone());
        }
        if let Some(candidate_index) = old_index.checked_sub(distance)
            && let Some(candidate) = previous_order.get(candidate_index)
            && current_order.contains(candidate)
        {
            return Some(candidate.clone());
        }
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticEventKind {
    ScreenChanged,
    RegionChanged,
    SemanticSelectionChanged,
    TargetUpdated,
    TargetRemoved,
    TaskOpened,
    ActionStarted,
    ActionCompleted,
    ControllerPolicyChanged,
    CodeModeStateChanged,
}

/// Terminal subset accepted by an `action_completed` event. `Accepted` is
/// deliberately excluded because asynchronous acceptance is not completion.
#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalActionStatus {
    AcceptedTerminal,
    Completed,
    Failed,
    Cancelled,
}

impl TryFrom<ActionStatus> for TerminalActionStatus {
    type Error = NonTerminalActionStatus;

    fn try_from(status: ActionStatus) -> Result<Self, Self::Error> {
        match status {
            ActionStatus::AcceptedTerminal => Ok(Self::AcceptedTerminal),
            ActionStatus::Completed => Ok(Self::Completed),
            ActionStatus::Failed => Ok(Self::Failed),
            ActionStatus::Cancelled => Ok(Self::Cancelled),
            ActionStatus::Accepted => Err(NonTerminalActionStatus),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("accepted is not a terminal action status")]
pub struct NonTerminalActionStatus;

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SemanticEventPayload {
    ScreenChanged {
        screen: ScreenId,
    },
    RegionChanged {
        region: RegionId,
    },
    SemanticSelectionChanged {
        selection: SemanticSelection,
    },
    TargetUpdated {
        target: SemanticTarget,
        target_revision: u64,
    },
    TargetRemoved {
        target: SemanticTarget,
    },
    TaskOpened {
        task_id: String,
    },
    ActionStarted {
        invocation_id: InvocationId,
        action_id: ActionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<SemanticTarget>,
    },
    ActionCompleted {
        invocation_id: InvocationId,
        action_id: ActionId,
        status: TerminalActionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<OperationId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ActionError>,
    },
    ControllerPolicyChanged {
        controller_access: UiControllerAccess,
        policy_epoch: u64,
    },
    CodeModeStateChanged {
        authority: ProgrammabilityAuthority,
    },
}

impl SemanticEventPayload {
    pub const fn kind(&self) -> SemanticEventKind {
        match self {
            Self::ScreenChanged { .. } => SemanticEventKind::ScreenChanged,
            Self::RegionChanged { .. } => SemanticEventKind::RegionChanged,
            Self::SemanticSelectionChanged { .. } => SemanticEventKind::SemanticSelectionChanged,
            Self::TargetUpdated { .. } => SemanticEventKind::TargetUpdated,
            Self::TargetRemoved { .. } => SemanticEventKind::TargetRemoved,
            Self::TaskOpened { .. } => SemanticEventKind::TaskOpened,
            Self::ActionStarted { .. } => SemanticEventKind::ActionStarted,
            Self::ActionCompleted { .. } => SemanticEventKind::ActionCompleted,
            Self::ControllerPolicyChanged { .. } => SemanticEventKind::ControllerPolicyChanged,
            Self::CodeModeStateChanged { .. } => SemanticEventKind::CodeModeStateChanged,
        }
    }

    pub fn target(&self) -> Option<&SemanticTarget> {
        match self {
            Self::SemanticSelectionChanged { selection } => selection.target.as_ref(),
            Self::TargetUpdated { target, .. } | Self::TargetRemoved { target } => Some(target),
            Self::ActionStarted { target, .. } => target.as_ref(),
            _ => None,
        }
    }

    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::TaskOpened { task_id } => Some(task_id),
            _ => self.target().and_then(SemanticTarget::task_id),
        }
    }

    pub fn action_id(&self) -> Option<&ActionId> {
        match self {
            Self::ActionStarted { action_id, .. } | Self::ActionCompleted { action_id, .. } => {
                Some(action_id)
            }
            _ => None,
        }
    }

    pub fn invocation_id(&self) -> Option<InvocationId> {
        match self {
            Self::ActionStarted { invocation_id, .. }
            | Self::ActionCompleted { invocation_id, .. } => Some(*invocation_id),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticEvent {
    pub schema_version: u16,
    pub cursor: u64,
    pub state_revision: u64,
    pub payload: SemanticEventPayload,
}

#[derive(Clone, Debug, Default, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticEventPredicate {
    pub kind: Option<SemanticEventKind>,
    pub target: Option<SemanticTarget>,
    pub task_id: Option<String>,
    pub action_id: Option<ActionId>,
    pub invocation_id: Option<InvocationId>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventWaitRequest {
    pub predicate: SemanticEventPredicate,
    pub after_cursor: u64,
    pub timeout_ms: u64,
}

impl EventWaitRequest {
    pub fn new(
        predicate: SemanticEventPredicate,
        after_cursor: u64,
        timeout_ms: u64,
    ) -> Result<Self, EventWaitRequestError> {
        if timeout_ms == 0 || timeout_ms > MAX_EVENT_WAIT_TIMEOUT_MS {
            return Err(EventWaitRequestError::InvalidTimeout {
                maximum_ms: MAX_EVENT_WAIT_TIMEOUT_MS,
                requested_ms: timeout_ms,
            });
        }
        Ok(Self {
            predicate,
            after_cursor,
            timeout_ms,
        })
    }
}

impl<'de> Deserialize<'de> for EventWaitRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            predicate: SemanticEventPredicate,
            after_cursor: u64,
            timeout_ms: u64,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.predicate, wire.after_cursor, wire.timeout_ms).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum EventWaitRequestError {
    #[error(
        "events.wait timeout {requested_ms} ms must be between 1 and {maximum_ms} ms inclusive"
    )]
    InvalidTimeout { maximum_ms: u64, requested_ms: u64 },
}

impl SemanticEventPredicate {
    pub fn matches(&self, event: &SemanticEvent) -> bool {
        self.kind.is_none_or(|kind| event.payload.kind() == kind)
            && self
                .target
                .as_ref()
                .is_none_or(|target| event.payload.target() == Some(target))
            && self
                .task_id
                .as_deref()
                .is_none_or(|task_id| event.payload.task_id() == Some(task_id))
            && self
                .action_id
                .as_ref()
                .is_none_or(|action_id| event.payload.action_id() == Some(action_id))
            && self
                .invocation_id
                .is_none_or(|invocation_id| event.payload.invocation_id() == Some(invocation_id))
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticEventPage {
    pub events: Vec<SemanticEvent>,
    /// Last cursor inspected, including nonmatching events.
    pub scanned_through: u64,
    pub has_more: bool,
}

#[derive(Clone, Debug)]
pub struct SemanticEventRing {
    capacity: usize,
    next_cursor: u64,
    events: VecDeque<SemanticEvent>,
}

impl SemanticEventRing {
    pub fn new(capacity: usize) -> Result<Self, SemanticEventRingError> {
        if capacity == 0 {
            return Err(SemanticEventRingError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            next_cursor: 1,
            events: VecDeque::with_capacity(capacity),
        })
    }

    pub fn push(&mut self, state_revision: u64, payload: SemanticEventPayload) -> SemanticEvent {
        let event = SemanticEvent {
            schema_version: crate::SCHEMA_VERSION,
            cursor: self.next_cursor,
            state_revision,
            payload,
        };
        self.next_cursor = self.next_cursor.saturating_add(1);
        if self.events.len() == self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(event.clone());
        event
    }

    pub fn latest_cursor(&self) -> u64 {
        self.events.back().map_or(0, |event| event.cursor)
    }

    pub fn oldest_cursor(&self) -> Option<u64> {
        self.events.front().map(|event| event.cursor)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn events_after(
        &self,
        after: u64,
        predicate: &SemanticEventPredicate,
        limit: usize,
    ) -> Result<SemanticEventPage, SemanticEventRingError> {
        if limit == 0 {
            return Err(SemanticEventRingError::ZeroLimit);
        }
        let latest = self.latest_cursor();
        if after > latest {
            return Err(SemanticEventRingError::FutureCursor {
                latest,
                requested_after: after,
            });
        }
        if let Some(oldest) = self.oldest_cursor()
            && after.saturating_add(1) < oldest
        {
            return Err(SemanticEventRingError::ResyncRequired {
                oldest_available: oldest,
                requested_after: after,
            });
        }

        let mut events = Vec::new();
        let mut scanned_through = after;
        let mut has_more = false;
        for event in self.events.iter().filter(|event| event.cursor > after) {
            if events.len() == limit {
                has_more = true;
                break;
            }
            scanned_through = event.cursor;
            if predicate.matches(event) {
                events.push(event.clone());
            }
        }
        if !has_more {
            scanned_through = latest.max(scanned_through);
        }
        Ok(SemanticEventPage {
            events,
            scanned_through,
            has_more,
        })
    }
}

impl Default for SemanticEventRing {
    fn default() -> Self {
        Self::new(DEFAULT_SEMANTIC_EVENT_CAPACITY)
            .expect("default semantic event ring capacity is nonzero")
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum SemanticEventRingError {
    #[error("semantic event ring capacity must be greater than zero")]
    ZeroCapacity,
    #[error("semantic event page limit must be greater than zero")]
    ZeroLimit,
    #[error(
        "event cursor {requested_after} is older than retained history; oldest available is {oldest_available}"
    )]
    ResyncRequired {
        oldest_available: u64,
        requested_after: u64,
    },
    #[error("event cursor {requested_after} is ahead of latest cursor {latest}")]
    FutureCursor { latest: u64, requested_after: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str) -> SemanticTarget {
        SemanticTarget::Task { task_id: id.into() }
    }

    #[test]
    fn semantic_targets_are_structural_hashable_wire_values() {
        let target = SemanticTarget::Annotation {
            task_id: "task-1".into(),
            item_id: "item-2".into(),
            annotation_id: "annotation-3".into(),
        };
        let value = serde_json::to_value(&target).unwrap();
        assert_eq!(value["kind"], "annotation");
        assert_eq!(value["task_id"], "task-1");
        assert_eq!(
            serde_json::from_value::<SemanticTarget>(value).unwrap(),
            target
        );

        let mut targets = std::collections::HashSet::new();
        targets.insert(target.clone());
        assert!(targets.contains(&target));
    }

    #[test]
    fn relevant_revision_changes_stale_only_the_relevant_precondition() {
        let task_domain = RevisionDomain::Task {
            task_id: "task-1".into(),
        };
        let streaming_domain = RevisionDomain::TimelineItem {
            task_id: "task-2".into(),
            item_id: "assistant".into(),
        };
        let mut revisions = RevisionTracker::new();
        let first = revisions.bump(task_domain.clone());
        assert_eq!(first.target_revision, 1);

        revisions.bump(streaming_domain);
        assert!(revisions.check(&task_domain, 1).is_ok());

        revisions.bump(task_domain.clone());
        assert_eq!(revisions.check(&task_domain, 1).unwrap_err().actual, 2);
    }

    #[test]
    fn revision_domains_canonicalize_structural_target_aliases() {
        let task_domain = RevisionDomain::Task {
            task_id: "task-1".into(),
        };
        let target_alias = RevisionDomain::Target {
            target: SemanticTarget::Task {
                task_id: "task-1".into(),
            },
        };
        let mut revisions = RevisionTracker::new();
        revisions.bump(task_domain);
        assert_eq!(revisions.revision(&target_alias), 1);
        assert!(revisions.check(&target_alias, 0).is_err());
    }

    #[test]
    fn navigation_projection_excludes_hidden_internal_and_zero_presence_rows() {
        let visible = task("visible");
        let hidden = task("hidden");
        let internal = task("internal");
        let zero_height = task("zero");
        let projected = navigable_targets([
            (
                &visible,
                SemanticPresence {
                    internal_only: false,
                    filtered_out: false,
                    revealable: true,
                    nonzero_presence: true,
                },
            ),
            (
                &hidden,
                SemanticPresence {
                    internal_only: false,
                    filtered_out: true,
                    revealable: true,
                    nonzero_presence: true,
                },
            ),
            (
                &internal,
                SemanticPresence {
                    internal_only: true,
                    filtered_out: false,
                    revealable: true,
                    nonzero_presence: true,
                },
            ),
            (
                &zero_height,
                SemanticPresence {
                    internal_only: false,
                    filtered_out: false,
                    revealable: true,
                    nonzero_presence: false,
                },
            ),
        ]);
        assert_eq!(projected, vec![visible]);
    }

    #[test]
    fn selection_identity_survives_insertion_and_has_deterministic_removal_neighbor() {
        let a = task("a");
        let b = task("b");
        let c = task("c");
        let inserted = task("inserted");
        assert_eq!(
            reconcile_removed_selection(
                &[a.clone(), b.clone(), c.clone()],
                &[inserted, a.clone(), b.clone(), c.clone()],
                &b,
            ),
            Some(b.clone())
        );
        assert_eq!(
            reconcile_removed_selection(
                &[a.clone(), b.clone(), c.clone()],
                &[a.clone(), c.clone()],
                &b,
            ),
            Some(c)
        );
        assert_eq!(
            reconcile_removed_selection(std::slice::from_ref(&a), &[], &a),
            None
        );
    }

    #[test]
    fn bounded_event_ring_is_sequence_safe_and_requires_resync_after_overflow() {
        let mut ring = SemanticEventRing::new(2).unwrap();
        ring.push(
            1,
            SemanticEventPayload::TaskOpened {
                task_id: "one".into(),
            },
        );
        let cursor = ring.latest_cursor();
        ring.push(
            2,
            SemanticEventPayload::TaskOpened {
                task_id: "two".into(),
            },
        );
        ring.push(
            3,
            SemanticEventPayload::TaskOpened {
                task_id: "three".into(),
            },
        );

        assert!(matches!(
            ring.events_after(0, &SemanticEventPredicate::default(), 10),
            Err(SemanticEventRingError::ResyncRequired { .. })
        ));
        let page = ring
            .events_after(
                cursor,
                &SemanticEventPredicate {
                    kind: Some(SemanticEventKind::TaskOpened),
                    task_id: Some("three".into()),
                    ..SemanticEventPredicate::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].cursor, 3);
        assert_eq!(page.scanned_through, 3);
    }

    #[test]
    fn event_cursor_from_describe_closes_query_then_wait_race() {
        let mut ring = SemanticEventRing::new(8).unwrap();
        let snapshot_cursor = ring.latest_cursor();
        ring.push(
            1,
            SemanticEventPayload::TaskOpened {
                task_id: "task-1".into(),
            },
        );
        let page = ring
            .events_after(snapshot_cursor, &SemanticEventPredicate::default(), 8)
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].payload.task_id(), Some("task-1"));
    }

    #[test]
    fn region_return_stack_is_bounded_and_lifo_for_surviving_frames() {
        let mut stack = RegionReturnStack::new(2).unwrap();
        let mut pushes = Vec::new();
        for region in ["sidebar", "transcript", "settings"] {
            pushes.push(stack.push(
                ScreenId::chat(),
                RegionId::parse(region).unwrap(),
                None,
                RegionReturnReason::RegionNavigation,
                1,
            ));
        }
        assert_eq!(stack.len(), 2);
        assert!(pushes[2].truncated_oldest);
        assert!(matches!(
            stack.pop_exact(pushes[1].token),
            Err(RegionReturnStackError::TokenMismatch { .. })
        ));
        assert_eq!(
            stack.pop_exact(pushes[2].token).unwrap().region.as_str(),
            "settings"
        );
        assert_eq!(
            stack.pop_exact(pushes[1].token).unwrap().region.as_str(),
            "transcript"
        );
    }

    #[test]
    fn region_return_tokens_cannot_cross_stacks() {
        let mut first = RegionReturnStack::new(2).unwrap();
        let mut second = RegionReturnStack::new(2).unwrap();
        let first_token = first
            .push(
                ScreenId::chat(),
                RegionId::sidebar(),
                None,
                RegionReturnReason::Overlay,
                1,
            )
            .token;
        second.push(
            ScreenId::chat(),
            RegionId::sidebar(),
            None,
            RegionReturnReason::Overlay,
            1,
        );
        assert!(matches!(
            second.pop_exact(first_token),
            Err(RegionReturnStackError::TokenMismatch { .. })
        ));
    }

    #[test]
    fn action_completed_and_wait_request_are_validated_on_wire() {
        assert!(TerminalActionStatus::try_from(ActionStatus::Accepted).is_err());
        assert!(
            serde_json::from_value::<TerminalActionStatus>(serde_json::json!("accepted")).is_err()
        );
        let operation_id = OperationId::new();
        let completion = SemanticEvent {
            schema_version: crate::SCHEMA_VERSION,
            cursor: 8,
            state_revision: 13,
            payload: SemanticEventPayload::ActionCompleted {
                invocation_id: InvocationId::new(),
                action_id: ActionId::parse("settings.set_theme").unwrap(),
                status: TerminalActionStatus::Completed,
                operation_id: Some(operation_id),
                result: Some(serde_json::json!({
                    "local_applied": true,
                    "durable": true
                })),
                error: None,
            },
        };
        let wire = serde_json::to_value(&completion).unwrap();
        assert_eq!(wire["payload"]["kind"], "action_completed");
        assert_eq!(wire["payload"]["operation_id"], operation_id.to_string());
        assert_eq!(wire["payload"]["result"]["durable"], true);
        assert!(wire["payload"].get("error").is_none());
        assert_eq!(
            serde_json::from_value::<SemanticEvent>(wire).unwrap(),
            completion
        );
        assert!(EventWaitRequest::new(SemanticEventPredicate::default(), 0, 60_000).is_ok());
        assert!(EventWaitRequest::new(SemanticEventPredicate::default(), 0, 60_001).is_err());
        assert!(
            serde_json::from_value::<EventWaitRequest>(serde_json::json!({
                "predicate": {},
                "after_cursor": 0,
                "timeout_ms": 0
            }))
            .is_err()
        );
    }

    #[test]
    fn snapshot_wire_boundary_rejects_secret_shaped_attributes() {
        let snapshot = SemanticSnapshot {
            schema_version: crate::SCHEMA_VERSION,
            state_revision: 1,
            event_cursor: 0,
            screen: ScreenId::chat(),
            active_region: RegionId::transcript(),
            selection: None,
            insertion: None,
            viewport: ViewportSummary {
                region: RegionId::transcript(),
                first_visible: None,
                last_visible: None,
                visible_count: 1,
            },
            stream_follow: true,
            roots: vec![SemanticNode {
                target: SemanticTarget::App,
                kind: SemanticKind::parse("application").unwrap(),
                label: None,
                state: SemanticNodeState {
                    attributes: BTreeMap::from([(
                        "oauth_token".into(),
                        Value::String("sentinel-secret".into()),
                    )]),
                    ..SemanticNodeState::default()
                },
                available_actions: Vec::new(),
                children: Vec::new(),
            }],
        };
        let error = serde_json::to_string(&snapshot).unwrap_err().to_string();
        assert!(!error.contains("sentinel-secret"));
        assert!(error.contains("oauth_token"));
    }
}
