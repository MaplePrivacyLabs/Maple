use std::{fmt, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    audit::AuditSpec,
    policy::InvocationPolicy,
    semantic::{RevisionDomain, SemanticTarget},
};

const MAX_ACTION_ID_LEN: usize = 128;
const MAX_REASON_CODE_LEN: usize = 64;
const MAX_CONTEXT_PATTERN_LEN: usize = 512;

/// A stable, wire-facing action identifier.
///
/// IDs use an intentionally conservative grammar so they survive Rust and UI
/// refactors: at least two dot-separated ASCII segments, each beginning with a
/// lowercase letter and continuing with lowercase letters, digits, or `_`.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ActionId(
    #[schemars(
        length(min = 3, max = 128),
        pattern(r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$")
    )]
    String,
);

impl ActionId {
    pub fn parse(value: impl Into<String>) -> Result<Self, ActionIdError> {
        let value = value.into();
        validate_action_id(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for ActionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for ActionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for ActionId {
    type Err = ActionIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for ActionId {
    type Error = ActionIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for ActionId {
    type Error = ActionIdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for ActionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ActionIdError {
    #[error("action ID is empty")]
    Empty,
    #[error("action ID exceeds {MAX_ACTION_ID_LEN} bytes")]
    TooLong,
    #[error("action ID must contain at least one namespace separator")]
    MissingNamespace,
    #[error("action ID contains an empty segment at position {segment}")]
    EmptySegment { segment: usize },
    #[error("action ID segment {segment} must start with an ASCII lowercase letter")]
    InvalidSegmentStart { segment: usize },
    #[error("action ID contains invalid character {character:?} at byte {byte_index}")]
    InvalidCharacter { byte_index: usize, character: char },
}

fn validate_action_id(value: &str) -> Result<(), ActionIdError> {
    if value.is_empty() {
        return Err(ActionIdError::Empty);
    }
    if value.len() > MAX_ACTION_ID_LEN {
        return Err(ActionIdError::TooLong);
    }
    if !value.contains('.') {
        return Err(ActionIdError::MissingNamespace);
    }

    let mut byte_offset = 0;
    for (segment_index, segment) in value.split('.').enumerate() {
        if segment.is_empty() {
            return Err(ActionIdError::EmptySegment {
                segment: segment_index,
            });
        }
        if !segment.as_bytes()[0].is_ascii_lowercase() {
            return Err(ActionIdError::InvalidSegmentStart {
                segment: segment_index,
            });
        }
        for (relative_index, character) in segment.char_indices() {
            if !(character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_') {
                return Err(ActionIdError::InvalidCharacter {
                    byte_index: byte_offset + relative_index,
                    character,
                });
            }
        }
        byte_offset += segment.len() + 1;
    }
    Ok(())
}

/// Stable machine-readable reason code used by availability and errors.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct DisabledReasonCode(
    #[schemars(length(min = 1, max = 64), pattern(r"^[a-z][a-z0-9_]*$"))] String,
);

impl DisabledReasonCode {
    pub fn parse(value: impl Into<String>) -> Result<Self, ReasonCodeError> {
        let value = value.into();
        validate_reason_code(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisabledReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DisabledReasonCode {
    type Err = ReasonCodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for DisabledReasonCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ReasonCodeError {
    #[error("reason code is empty")]
    Empty,
    #[error("reason code exceeds {MAX_REASON_CODE_LEN} bytes")]
    TooLong,
    #[error("reason code must start with an ASCII lowercase letter")]
    InvalidStart,
    #[error("reason code contains invalid character {character:?} at byte {byte_index}")]
    InvalidCharacter { byte_index: usize, character: char },
}

fn validate_reason_code(value: &str) -> Result<(), ReasonCodeError> {
    if value.is_empty() {
        return Err(ReasonCodeError::Empty);
    }
    if value.len() > MAX_REASON_CODE_LEN {
        return Err(ReasonCodeError::TooLong);
    }
    if !value.as_bytes()[0].is_ascii_lowercase() {
        return Err(ReasonCodeError::InvalidStart);
    }
    for (byte_index, character) in value.char_indices() {
        if !(character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_') {
            return Err(ReasonCodeError::InvalidCharacter {
                byte_index,
                character,
            });
        }
    }
    Ok(())
}

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Eq,
            Hash,
            JsonSchema,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

uuid_id!(InvocationId);
uuid_id!(ProgramId);
uuid_id!(ExecutionId);
// Stable identity for asynchronous work that outlives the admitting executor.
// InvocationId identifies the attempt/audit record; OperationId identifies
// the retained cancellation and terminal-completion handle.
uuid_id!(OperationId);

/// Exact opaque identity assigned by Maple's model-run registry.
///
/// Model run IDs are deliberately not re-minted as UUIDs at the harness
/// boundary: cancellation, tombstones, audit, and Code Mode must all refer to
/// the same host-owned value losslessly.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct RunId(#[schemars(length(min = 1, max = 4096))] String);

impl RunId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn from_host(value: impl Into<String>) -> Result<Self, RunIdError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(RunIdError::Empty);
        }
        if value.len() > 4096 {
            return Err(RunIdError::TooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(RunIdError::ControlCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_host(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RunIdError {
    #[error("model run ID cannot be empty")]
    Empty,
    #[error("model run ID exceeds 4096 bytes")]
    TooLong,
    #[error("model run ID contains a control character")]
    ControlCharacter,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskIdentity {
    pub account_scope: String,
    pub task_id: String,
}

impl TaskIdentity {
    pub fn new(account_scope: impl Into<String>, task_id: impl Into<String>) -> Self {
        Self {
            account_scope: account_scope.into(),
            task_id: task_id.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct SemanticContextPattern(#[schemars(length(min = 1, max = 512))] String);

impl SemanticContextPattern {
    pub fn parse(value: impl Into<String>) -> Result<Self, ContextPatternError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ContextPatternError::Empty);
        }
        if value.len() > MAX_CONTEXT_PATTERN_LEN {
            return Err(ContextPatternError::TooLong);
        }
        if value.trim() != value || value.chars().any(char::is_control) {
            return Err(ContextPatternError::InvalidWhitespaceOrControl);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SemanticContextPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SemanticContextPattern {
    type Err = ContextPatternError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for SemanticContextPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ContextPatternError {
    #[error("semantic context pattern is empty")]
    Empty,
    #[error("semantic context pattern exceeds {MAX_CONTEXT_PATTERN_LEN} bytes")]
    TooLong,
    #[error("semantic context pattern has surrounding whitespace or control characters")]
    InvalidWhitespaceOrControl,
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionEffect {
    Observe,
    Navigate,
    MutateMaple,
    ExternalEffect,
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recoverability {
    Ephemeral,
    Reversible,
    Irreversible,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutProfile {
    #[default]
    Standard,
    Vim,
}

/// Declares how an action derives its canonical precondition domain from the
/// call's semantic target.
#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreconditionDomainSelector {
    Global,
    /// Use the complete semantic target as the revision domain.
    Target,
    /// Derive a task record domain from any task-bearing target.
    Task,
    Project,
    /// Derive a task timeline domain from any task-bearing target.
    Timeline,
    TimelineItem,
    /// Derive a draft domain from any task-bearing target.
    Draft,
    Setting,
    Permission,
    Question,
    QueueItem,
}

impl PreconditionDomainSelector {
    fn resolve(
        self,
        target: Option<&SemanticTarget>,
    ) -> Result<RevisionDomain, ActionPreconditionInvariantError> {
        if self == Self::Global {
            return Ok(RevisionDomain::Global);
        }

        let target =
            target.ok_or(ActionPreconditionInvariantError::MissingTarget { selector: self })?;
        let incompatible = || ActionPreconditionInvariantError::IncompatibleTarget {
            selector: self,
            target: Box::new(target.clone()),
        };

        match self {
            Self::Global => unreachable!("global selector returned before target resolution"),
            Self::Target => Ok(RevisionDomain::Target {
                target: target.clone(),
            }),
            Self::Task => target
                .task_id()
                .map(|task_id| RevisionDomain::Task {
                    task_id: task_id.to_owned(),
                })
                .ok_or_else(incompatible),
            Self::Project => match target {
                SemanticTarget::Project { canonical_root } => Ok(RevisionDomain::Project {
                    canonical_root: canonical_root.clone(),
                }),
                _ => Err(incompatible()),
            },
            Self::Timeline => target
                .task_id()
                .map(|task_id| RevisionDomain::Timeline {
                    task_id: task_id.to_owned(),
                })
                .ok_or_else(incompatible),
            Self::TimelineItem => match target {
                SemanticTarget::TimelineItem { task_id, item_id }
                | SemanticTarget::Annotation {
                    task_id, item_id, ..
                } => Ok(RevisionDomain::TimelineItem {
                    task_id: task_id.clone(),
                    item_id: item_id.clone(),
                }),
                _ => Err(incompatible()),
            },
            Self::Draft => target
                .task_id()
                .map(|task_id| RevisionDomain::Draft {
                    task_id: task_id.to_owned(),
                })
                .ok_or_else(incompatible),
            Self::Setting => match target {
                SemanticTarget::Setting { key } => Ok(RevisionDomain::Setting { key: key.clone() }),
                _ => Err(incompatible()),
            },
            Self::Permission => match target {
                SemanticTarget::Permission { request_id } => Ok(RevisionDomain::Permission {
                    request_id: request_id.clone(),
                }),
                _ => Err(incompatible()),
            },
            Self::Question => match target {
                SemanticTarget::Question {
                    request_id,
                    question_id,
                } => Ok(RevisionDomain::Question {
                    request_id: request_id.clone(),
                    question_id: question_id.clone(),
                }),
                _ => Err(incompatible()),
            },
            Self::QueueItem => match target {
                SemanticTarget::QueueItem { task_id, queue_id } => Ok(RevisionDomain::QueueItem {
                    task_id: task_id.clone(),
                    queue_id: queue_id.clone(),
                }),
                _ => Err(incompatible()),
            },
        }
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultBinding {
    pub profile: ShortcutProfile,
    pub context: String,
    pub sequence: String,
    #[serde(default = "empty_object")]
    pub arguments: Value,
}

pub type DefaultBindings = Vec<DefaultBinding>;

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDescriptor {
    pub schema_version: u16,
    pub id: ActionId,
    pub label: String,
    pub description: String,
    pub category: String,
    pub argument_schema: Value,
    pub result_schema: Value,
    #[serde(default)]
    pub contexts: Vec<SemanticContextPattern>,
    pub effect: ActionEffect,
    pub invocation_policy: InvocationPolicy,
    pub recoverability: Recoverability,
    #[serde(default)]
    pub audit: AuditSpec,
    #[serde(default)]
    pub default_bindings: DefaultBindings,
    /// The action-declared revision domain accepted in call preconditions.
    ///
    /// `None` means this action does not accept a precondition. A declared
    /// kind permits, but does not require, a call precondition; executors that
    /// require compare-and-set semantics may additionally require its
    /// presence after this structural check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precondition_domain: Option<PreconditionDomainSelector>,
    /// Whether this action participates in the GPUI keymap/action catalog.
    #[serde(default)]
    pub bindable: bool,
    /// True for a host action whose accepted response precedes process shutdown.
    #[serde(default)]
    pub terminal_host_action: bool,
}

impl ActionDescriptor {
    pub fn validate_arguments(&self, arguments: &Value) -> Result<(), SchemaValidationError> {
        validate_instance(&self.argument_schema, arguments)
    }

    pub fn validate_result(&self, result: &Value) -> Result<(), SchemaValidationError> {
        validate_instance(&self.result_schema, result)
    }

    /// Validates that a call uses this descriptor and cannot substitute a
    /// different revision domain kind.
    pub fn validate_precondition(
        &self,
        call: &ActionCall,
    ) -> Result<(), ActionPreconditionInvariantError> {
        if call.action_id != self.id {
            return Err(ActionPreconditionInvariantError::ActionMismatch {
                expected: self.id.clone(),
                actual: call.action_id.clone(),
            });
        }
        match (self.precondition_domain, call.precondition.as_ref()) {
            (None, None) | (Some(_), None) => Ok(()),
            (None, Some(_)) => Err(ActionPreconditionInvariantError::UndeclaredPrecondition),
            (Some(selector), Some(precondition)) => {
                let expected = selector.resolve(call.target.as_ref())?;
                if precondition.domain == expected {
                    Ok(())
                } else {
                    Err(ActionPreconditionInvariantError::DomainMismatch {
                        expected: Box::new(expected),
                        actual: Box::new(precondition.domain.clone()),
                    })
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ActionPreconditionInvariantError {
    #[error("call action {actual} does not match descriptor {expected}")]
    ActionMismatch {
        expected: ActionId,
        actual: ActionId,
    },
    #[error("action descriptor does not declare a precondition domain")]
    UndeclaredPrecondition,
    #[error("precondition selector {selector:?} requires a semantic target")]
    MissingTarget {
        selector: PreconditionDomainSelector,
    },
    #[error("semantic target {target:?} cannot define precondition selector {selector:?}")]
    IncompatibleTarget {
        selector: PreconditionDomainSelector,
        target: Box<SemanticTarget>,
    },
    #[error("precondition domain {actual:?} does not match canonical domain {expected:?}")]
    DomainMismatch {
        expected: Box<RevisionDomain>,
        actual: Box<RevisionDomain>,
    },
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SchemaValidationError {
    #[error("invalid JSON schema: {0}")]
    InvalidSchema(String),
    #[error("value does not match schema: {0}")]
    InvalidValue(String),
}

fn validate_instance(schema: &Value, instance: &Value) -> Result<(), SchemaValidationError> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| SchemaValidationError::InvalidSchema(error.to_string()))?;
    if validator.is_valid(instance) {
        Ok(())
    } else {
        let message = validator
            .iter_errors(instance)
            .next()
            .map(|error| error.masked().to_string())
            .unwrap_or_else(|| "unknown validation error".to_owned());
        Err(SchemaValidationError::InvalidValue(message))
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionCall {
    pub action_id: ActionId,
    #[serde(default = "empty_object")]
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<SemanticTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precondition: Option<ActionPrecondition>,
}

impl ActionCall {
    pub fn new(action_id: ActionId) -> Self {
        Self {
            action_id,
            arguments: empty_object(),
            target: None,
            precondition: None,
        }
    }

    pub fn with_arguments(mut self, arguments: Value) -> Self {
        self.arguments = arguments;
        self
    }

    pub fn with_target(mut self, target: SemanticTarget) -> Self {
        self.target = Some(target);
        self
    }

    pub fn with_precondition(mut self, precondition: ActionPrecondition) -> Self {
        self.precondition = Some(precondition);
        self
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionPrecondition {
    pub domain: RevisionDomain,
    pub target_revision: u64,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Availability {
    Available,
    Disabled {
        code: DisabledReasonCode,
        message: String,
    },
}

impl Availability {
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    pub fn disabled(code: DisabledReasonCode, message: impl Into<String>) -> Self {
        Self::Disabled {
            code,
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Accepted,
    AcceptedTerminal,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionErrorCode {
    UnknownAction,
    InvalidArguments,
    NotApplicable,
    Unavailable,
    PolicyDenied,
    StaleTarget,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionError {
    pub code: ActionErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<DisabledReasonCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ActionError {
    /// Builds an error whose code does not require additional structured
    /// metadata. Use [`Self::unavailable`] for `Unavailable` errors.
    pub fn new(
        code: ActionErrorCode,
        message: impl Into<String>,
    ) -> Result<Self, ActionErrorInvariantError> {
        let error = Self {
            code,
            message: message.into(),
            reason_code: None,
            details: None,
        };
        error.validate()?;
        Ok(error)
    }

    pub fn unavailable(code: DisabledReasonCode, message: impl Into<String>) -> Self {
        Self {
            code: ActionErrorCode::Unavailable,
            message: message.into(),
            reason_code: Some(code),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn validate(&self) -> Result<(), ActionErrorInvariantError> {
        match (self.code, self.reason_code.as_ref()) {
            (ActionErrorCode::Unavailable, None) => {
                Err(ActionErrorInvariantError::UnavailableMissingReasonCode)
            }
            (ActionErrorCode::Unavailable, Some(_)) | (_, None) => Ok(()),
            (code, Some(_)) => Err(ActionErrorInvariantError::UnexpectedReasonCode { code }),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionErrorWire {
    code: ActionErrorCode,
    message: String,
    #[serde(default)]
    reason_code: Option<DisabledReasonCode>,
    #[serde(default)]
    details: Option<Value>,
}

#[derive(Serialize)]
struct ActionErrorWireRef<'a> {
    code: ActionErrorCode,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason_code: Option<&'a DisabledReasonCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a Value>,
}

impl Serialize for ActionError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        ActionErrorWireRef {
            code: self.code,
            message: &self.message,
            reason_code: self.reason_code.as_ref(),
            details: self.details.as_ref(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ActionError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ActionErrorWire::deserialize(deserializer)?;
        let error = Self {
            code: wire.code,
            message: wire.message,
            reason_code: wire.reason_code,
            details: wire.details,
        };
        error.validate().map_err(de::Error::custom)?;
        Ok(error)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ActionErrorInvariantError {
    #[error("unavailable action error is missing a stable reason code")]
    UnavailableMissingReasonCode,
    #[error("action error code {code:?} must not include an unavailable reason code")]
    UnexpectedReasonCode { code: ActionErrorCode },
}

#[derive(Clone, Debug, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionResponse {
    pub invocation_id: InvocationId,
    pub action_id: ActionId,
    pub status: ActionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ActionError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_revision: Option<u64>,
}

impl ActionResponse {
    /// Builds an asynchronous acceptance response with a fresh host operation
    /// identity. The operation ID is part of the result envelope so action
    /// result schemas and generated SDKs expose it without conflating it with
    /// the invocation/audit identity.
    pub fn accepted(invocation_id: InvocationId, action_id: ActionId, result: Value) -> Self {
        Self::accepted_with_operation_id(invocation_id, action_id, OperationId::new(), result)
    }

    /// Deterministic form used by a host that reserves an operation identity
    /// before handing work to a backend. Existing `operation_id` input is
    /// overwritten: an executor cannot select a caller-provided identity.
    pub fn accepted_with_operation_id(
        invocation_id: InvocationId,
        action_id: ActionId,
        operation_id: OperationId,
        mut result: Value,
    ) -> Self {
        if let Some(result) = result.as_object_mut() {
            result.insert(
                "operation_id".to_string(),
                Value::String(operation_id.to_string()),
            );
        }
        Self {
            invocation_id,
            action_id,
            status: ActionStatus::Accepted,
            result: Some(result),
            error: None,
            state_revision: None,
        }
    }

    /// Returns the stable operation identity for an accepted response.
    /// Non-accepted responses deliberately have no operation handle.
    pub fn operation_id(&self) -> Result<Option<OperationId>, ActionResponseInvariantError> {
        if self.status != ActionStatus::Accepted {
            return Ok(None);
        }
        let value = self
            .result
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|result| result.get("operation_id"))
            .ok_or(ActionResponseInvariantError::AcceptedMissingOperationId)?;
        serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| ActionResponseInvariantError::AcceptedInvalidOperationId)
    }

    pub fn accepted_terminal(
        invocation_id: InvocationId,
        action_id: ActionId,
        result: Value,
    ) -> Self {
        Self {
            invocation_id,
            action_id,
            status: ActionStatus::AcceptedTerminal,
            result: Some(result),
            error: None,
            state_revision: None,
        }
    }

    pub fn completed(
        invocation_id: InvocationId,
        action_id: ActionId,
        result: Value,
        state_revision: Option<u64>,
    ) -> Self {
        Self {
            invocation_id,
            action_id,
            status: ActionStatus::Completed,
            result: Some(result),
            error: None,
            state_revision,
        }
    }

    pub fn failed(
        invocation_id: InvocationId,
        action_id: ActionId,
        error: ActionError,
    ) -> Result<Self, ActionResponseInvariantError> {
        let status = if error.code == ActionErrorCode::Cancelled {
            ActionStatus::Cancelled
        } else {
            ActionStatus::Failed
        };
        let response = Self {
            invocation_id,
            action_id,
            status,
            result: None,
            error: Some(error),
            state_revision: None,
        };
        response.validate()?;
        Ok(response)
    }

    pub fn validate(&self) -> Result<(), ActionResponseInvariantError> {
        if let Some(error) = &self.error {
            error.validate()?;
        }

        match self.status {
            ActionStatus::Accepted => {
                if self.error.is_some() {
                    return Err(ActionResponseInvariantError::SuccessHasError);
                }
                self.operation_id()?;
            }
            ActionStatus::AcceptedTerminal => {
                if self.error.is_some() {
                    return Err(ActionResponseInvariantError::SuccessHasError);
                }
            }
            ActionStatus::Completed => {
                if self.error.is_some() {
                    return Err(ActionResponseInvariantError::SuccessHasError);
                }
                if self.result.is_none() {
                    return Err(ActionResponseInvariantError::CompletedMissingResult);
                }
            }
            ActionStatus::Failed => {
                if self.error.is_none() {
                    return Err(ActionResponseInvariantError::FailureMissingError);
                }
                if self.result.is_some() {
                    return Err(ActionResponseInvariantError::FailureHasResult);
                }
                if self.error.as_ref().map(|error| error.code) == Some(ActionErrorCode::Cancelled) {
                    return Err(ActionResponseInvariantError::FailedHasCancelledError);
                }
            }
            ActionStatus::Cancelled => {
                let Some(error) = &self.error else {
                    return Err(ActionResponseInvariantError::FailureMissingError);
                };
                if self.result.is_some() {
                    return Err(ActionResponseInvariantError::FailureHasResult);
                }
                if error.code != ActionErrorCode::Cancelled {
                    return Err(
                        ActionResponseInvariantError::CancelledHasNonCancelledError {
                            actual: error.code,
                        },
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionResponseWire {
    invocation_id: InvocationId,
    action_id: ActionId,
    status: ActionStatus,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ActionError>,
    #[serde(default)]
    state_revision: Option<u64>,
}

#[derive(Serialize)]
struct ActionResponseWireRef<'a> {
    invocation_id: InvocationId,
    action_id: &'a ActionId,
    status: ActionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a ActionError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_revision: Option<u64>,
}

impl Serialize for ActionResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        ActionResponseWireRef {
            invocation_id: self.invocation_id,
            action_id: &self.action_id,
            status: self.status,
            result: self.result.as_ref(),
            error: self.error.as_ref(),
            state_revision: self.state_revision,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ActionResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ActionResponseWire::deserialize(deserializer)?;
        let response = Self {
            invocation_id: wire.invocation_id,
            action_id: wire.action_id,
            status: wire.status,
            result: wire.result,
            error: wire.error,
            state_revision: wire.state_revision,
        };
        response.validate().map_err(de::Error::custom)?;
        Ok(response)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ActionResponseInvariantError {
    #[error(transparent)]
    InvalidError(#[from] ActionErrorInvariantError),
    #[error("successful or accepted action response contains an error")]
    SuccessHasError,
    #[error("accepted action response is missing a stable operation_id result field")]
    AcceptedMissingOperationId,
    #[error("accepted action response contains an invalid operation_id")]
    AcceptedInvalidOperationId,
    #[error("completed action response is missing a result")]
    CompletedMissingResult,
    #[error("failed or cancelled action response is missing an error")]
    FailureMissingError,
    #[error("failed or cancelled action response contains a result")]
    FailureHasResult,
    #[error("failed action response contains a cancelled error")]
    FailedHasCancelledError,
    #[error("cancelled action response contains non-cancelled error {actual:?}")]
    CancelledHasNonCancelledError { actual: ActionErrorCode },
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn descriptor(id: &str) -> ActionDescriptor {
        ActionDescriptor {
            schema_version: 1,
            id: ActionId::parse(id).unwrap(),
            label: "Test action".into(),
            description: "Test action descriptor".into(),
            category: "Tests".into(),
            argument_schema: json!({"type": "object"}),
            result_schema: json!({"type": "object"}),
            contexts: Vec::new(),
            effect: ActionEffect::MutateMaple,
            invocation_policy: InvocationPolicy::ControllerCallable,
            recoverability: Recoverability::Reversible,
            audit: AuditSpec::default(),
            default_bindings: Vec::new(),
            precondition_domain: None,
            bindable: false,
            terminal_host_action: false,
        }
    }

    #[test]
    fn stable_action_ids_accept_the_normative_grammar() {
        for valid in [
            "app.quit",
            "task.set_archived",
            "composer.vim.count_digit",
            "settings.set_model2",
        ] {
            assert_eq!(ActionId::parse(valid).unwrap().as_str(), valid);
        }
    }

    #[test]
    fn stable_action_ids_reject_ambiguous_or_unstable_spellings() {
        for invalid in [
            "",
            "quit",
            ".quit",
            "app.",
            "app..quit",
            "App.quit",
            "app.Quit",
            "app. quit",
            "app.-quit",
            "app.quît",
        ] {
            assert!(ActionId::parse(invalid).is_err(), "accepted {invalid:?}");
            assert!(
                serde_json::from_value::<ActionId>(json!(invalid)).is_err(),
                "deserialized {invalid:?}"
            );
        }
    }

    #[test]
    fn model_run_ids_preserve_host_owned_opaque_identity() {
        let value = "run_1788062400000_17";
        let run = RunId::from_host(value).unwrap();
        assert_eq!(run.as_str(), value);
        assert_eq!(serde_json::to_value(&run).unwrap(), json!(value));
        assert_eq!(serde_json::from_value::<RunId>(json!(value)).unwrap(), run);
        assert_eq!(RunId::from_host("  ").unwrap_err(), RunIdError::Empty);
    }

    #[test]
    fn argument_and_result_schemas_validate_wire_values() {
        let mut action_descriptor = descriptor("task.set_archived");
        action_descriptor.argument_schema = json!({
            "type": "object",
            "required": ["task_id", "archived"],
            "properties": {
                "task_id": {"type": "string"},
                "archived": {"type": "boolean"}
            },
            "additionalProperties": false
        });
        action_descriptor.result_schema = json!({
            "type": "object",
            "required": ["changed"],
            "properties": {"changed": {"type": "boolean"}}
        });
        action_descriptor.bindable = true;

        assert!(
            action_descriptor
                .validate_arguments(&json!({"task_id": "t1", "archived": true}))
                .is_ok()
        );
        assert!(
            action_descriptor
                .validate_arguments(&json!({"task_id": "t1", "archived": "yes"}))
                .is_err()
        );
        assert!(
            action_descriptor
                .validate_result(&json!({"changed": true}))
                .is_ok()
        );
        assert!(action_descriptor.validate_result(&json!({})).is_err());

        let round_trip: ActionDescriptor =
            serde_json::from_value(serde_json::to_value(&action_descriptor).unwrap()).unwrap();
        assert_eq!(round_trip, action_descriptor);
    }

    #[test]
    fn schema_validation_diagnostics_mask_offending_secret_values() {
        let sentinel = "MAPLE_SECRET_SENTINEL_9f31";
        let error = validate_instance(&json!({"type": "string", "maxLength": 2}), &json!(sentinel))
            .unwrap_err();
        assert!(!error.to_string().contains(sentinel));
    }

    #[test]
    fn controller_wire_types_generate_schemars_one_schemas_for_gpui_adapters() {
        let action_call = schemars::schema_for!(ActionCall);
        let target = schemars::schema_for!(SemanticTarget);
        let precondition = schemars::schema_for!(ActionPrecondition);

        for schema in [action_call, target, precondition] {
            let value = serde_json::to_value(schema).unwrap();
            assert!(value.is_object());
            assert!(value.get("$schema").is_some());
        }
    }

    #[test]
    fn unavailable_error_has_stable_reason_and_human_copy() {
        let error = ActionError::unavailable(
            DisabledReasonCode::parse("no_annotations").unwrap(),
            "No annotations exist in this task",
        );
        assert_eq!(error.code, ActionErrorCode::Unavailable);
        assert_eq!(error.reason_code.unwrap().as_str(), "no_annotations");
    }

    #[test]
    fn action_errors_enforce_unavailable_reason_code_in_both_directions() {
        assert_eq!(
            ActionError::new(ActionErrorCode::Unavailable, "not ready").unwrap_err(),
            ActionErrorInvariantError::UnavailableMissingReasonCode
        );
        assert!(ActionError::new(ActionErrorCode::Failed, "failed").is_ok());

        assert!(
            serde_json::from_value::<ActionError>(json!({
                "code": "unavailable",
                "message": "not ready"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ActionError>(json!({
                "code": "failed",
                "message": "failed",
                "reason_code": "not_ready"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ActionError>(json!({
                "code": "unavailable",
                "message": "not ready",
                "reason_code": "not_ready"
            }))
            .is_ok()
        );

        let forged = ActionError {
            code: ActionErrorCode::Failed,
            message: "failed".into(),
            reason_code: Some(DisabledReasonCode::parse("not_ready").unwrap()),
            details: None,
        };
        assert!(serde_json::to_value(forged).is_err());
    }

    #[test]
    fn descriptor_rejects_caller_selected_precondition_domain_kind() {
        let mut action_descriptor = descriptor("task.set_archived");
        action_descriptor.precondition_domain = Some(PreconditionDomainSelector::Task);

        let matching = ActionCall::new(action_descriptor.id.clone())
            .with_target(SemanticTarget::Task {
                task_id: "task-1".into(),
            })
            .with_precondition(ActionPrecondition {
                domain: RevisionDomain::Task {
                    task_id: "task-1".into(),
                },
                target_revision: 7,
            });
        assert!(action_descriptor.validate_precondition(&matching).is_ok());

        let substituted = ActionCall::new(action_descriptor.id.clone())
            .with_target(SemanticTarget::Task {
                task_id: "task-1".into(),
            })
            .with_precondition(ActionPrecondition {
                domain: RevisionDomain::Global,
                target_revision: 7,
            });
        assert_eq!(
            action_descriptor
                .validate_precondition(&substituted)
                .unwrap_err(),
            ActionPreconditionInvariantError::DomainMismatch {
                expected: Box::new(RevisionDomain::Task {
                    task_id: "task-1".into(),
                }),
                actual: Box::new(RevisionDomain::Global),
            }
        );

        let undeclared_call = ActionCall::new(ActionId::parse("task.open").unwrap())
            .with_target(SemanticTarget::Task {
                task_id: "task-1".into(),
            })
            .with_precondition(ActionPrecondition {
                domain: RevisionDomain::Task {
                    task_id: "task-1".into(),
                },
                target_revision: 7,
            });
        let undeclared = descriptor("task.open").validate_precondition(&undeclared_call);
        assert_eq!(
            undeclared.unwrap_err(),
            ActionPreconditionInvariantError::UndeclaredPrecondition
        );
    }

    #[test]
    fn descriptor_rejects_same_kind_precondition_for_a_different_identity() {
        let mut action_descriptor = descriptor("task.set_archived");
        action_descriptor.precondition_domain = Some(PreconditionDomainSelector::Task);
        let call = ActionCall::new(action_descriptor.id.clone())
            .with_target(SemanticTarget::Task {
                task_id: "task-a".into(),
            })
            .with_precondition(ActionPrecondition {
                domain: RevisionDomain::Task {
                    task_id: "task-b".into(),
                },
                target_revision: 7,
            });

        assert_eq!(
            action_descriptor.validate_precondition(&call).unwrap_err(),
            ActionPreconditionInvariantError::DomainMismatch {
                expected: Box::new(RevisionDomain::Task {
                    task_id: "task-a".into(),
                }),
                actual: Box::new(RevisionDomain::Task {
                    task_id: "task-b".into(),
                }),
            }
        );
    }

    #[test]
    fn descriptor_precondition_validation_is_bound_to_the_action_id() {
        let mut action_descriptor = descriptor("task.set_archived");
        action_descriptor.precondition_domain = Some(PreconditionDomainSelector::Task);
        let other = ActionCall::new(ActionId::parse("task.open").unwrap());

        assert!(matches!(
            action_descriptor.validate_precondition(&other),
            Err(ActionPreconditionInvariantError::ActionMismatch { .. })
        ));
    }

    #[test]
    fn response_invariants_distinguish_acceptance_from_completion() {
        let invocation_id = InvocationId::new();
        let action_id = ActionId::parse("task.open").unwrap();
        let accepted = ActionResponse::accepted(
            invocation_id,
            action_id.clone(),
            json!({
                "operation_id": "op-1"
            }),
        );
        assert_eq!(accepted.status, ActionStatus::Accepted);
        assert!(accepted.validate().is_ok());

        let completed =
            ActionResponse::completed(invocation_id, action_id, json!({"task_id": "t1"}), Some(4));
        assert_eq!(completed.status, ActionStatus::Completed);
        assert!(completed.validate().is_ok());
    }

    #[test]
    fn response_deserialization_rejects_inconsistent_status_result_and_error() {
        let invocation_id = InvocationId::new();
        let action_id = ActionId::parse("task.open").unwrap();
        let base = json!({
            "invocation_id": invocation_id,
            "action_id": action_id,
        });

        let cases = [
            json!({
                "invocation_id": invocation_id,
                "action_id": action_id,
                "status": "completed"
            }),
            json!({
                "invocation_id": invocation_id,
                "action_id": action_id,
                "status": "accepted",
                "error": {"code": "failed", "message": "failed"}
            }),
            json!({
                "invocation_id": invocation_id,
                "action_id": action_id,
                "status": "accepted",
                "result": {}
            }),
            json!({
                "invocation_id": invocation_id,
                "action_id": action_id,
                "status": "accepted",
                "result": {"operation_id": "caller-selected"}
            }),
            json!({
                "invocation_id": invocation_id,
                "action_id": action_id,
                "status": "failed"
            }),
            json!({
                "invocation_id": invocation_id,
                "action_id": action_id,
                "status": "failed",
                "result": {},
                "error": {"code": "failed", "message": "failed"}
            }),
            json!({
                "invocation_id": invocation_id,
                "action_id": action_id,
                "status": "failed",
                "error": {"code": "cancelled", "message": "cancelled"}
            }),
            json!({
                "invocation_id": invocation_id,
                "action_id": action_id,
                "status": "cancelled",
                "error": {"code": "failed", "message": "failed"}
            }),
        ];

        assert!(base.is_object());
        for case in cases {
            assert!(
                serde_json::from_value::<ActionResponse>(case.clone()).is_err(),
                "accepted inconsistent response {case}"
            );
        }

        assert!(
            serde_json::from_value::<ActionResponse>(json!({
                "invocation_id": invocation_id,
                "action_id": action_id,
                "status": "accepted",
                "result": {"operation_id": OperationId::new()}
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<ActionResponse>(json!({
                "invocation_id": invocation_id,
                "action_id": action_id,
                "status": "cancelled",
                "error": {"code": "cancelled", "message": "cancelled"}
            }))
            .is_ok()
        );

        let forged = ActionResponse {
            invocation_id,
            action_id,
            status: ActionStatus::Completed,
            result: None,
            error: None,
            state_revision: None,
        };
        assert!(serde_json::to_value(forged).is_err());
    }
}
