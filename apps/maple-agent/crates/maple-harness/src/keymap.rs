use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    str::FromStr,
};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer, de,
    de::{MapAccess, Visitor},
};
use serde_json::Value;
use thiserror::Error;

use crate::{ActionId, ActionRegistry, SchemaValidationError, ShortcutProfile};

const MAX_KEY_SEQUENCE_BYTES: usize = 128;
const MAX_KEY_STROKES: usize = 8;
const MAX_CONTEXT_BYTES: usize = 512;

/// A syntactically validated space-separated GPUI keystroke sequence.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct KeySequence(String);

impl KeySequence {
    pub fn parse(value: impl Into<String>) -> Result<Self, KeySequenceError> {
        let value = value.into();
        validate_key_sequence(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn strokes(&self) -> impl Iterator<Item = &str> {
        self.0.split(' ')
    }

    pub fn stroke_count(&self) -> usize {
        self.strokes().count()
    }

    pub fn is_prefix_of(&self, other: &Self) -> bool {
        let this = self.strokes().collect::<Vec<_>>();
        let other = other.strokes().collect::<Vec<_>>();
        this.len() < other.len() && other.starts_with(&this)
    }
}

impl fmt::Display for KeySequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for KeySequence {
    type Err = KeySequenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for KeySequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum KeySequenceError {
    #[error("key sequence is empty")]
    Empty,
    #[error("key sequence exceeds {MAX_KEY_SEQUENCE_BYTES} bytes")]
    TooLong,
    #[error("key sequence must use single ASCII spaces with no surrounding whitespace")]
    NonCanonicalWhitespace,
    #[error("key sequence exceeds the {MAX_KEY_STROKES}-stroke limit")]
    TooManyStrokes,
    #[error("keystroke {stroke:?} contains a control or non-ASCII character")]
    InvalidStroke { stroke: String },
}

fn validate_key_sequence(value: &str) -> Result<(), KeySequenceError> {
    if value.is_empty() {
        return Err(KeySequenceError::Empty);
    }
    if value.len() > MAX_KEY_SEQUENCE_BYTES {
        return Err(KeySequenceError::TooLong);
    }
    let strokes = value.split_whitespace().collect::<Vec<_>>();
    if strokes.join(" ") != value {
        return Err(KeySequenceError::NonCanonicalWhitespace);
    }
    if strokes.len() > MAX_KEY_STROKES {
        return Err(KeySequenceError::TooManyStrokes);
    }
    for stroke in strokes {
        if stroke.is_empty()
            || !stroke.is_ascii()
            || stroke.chars().any(|character| character.is_ascii_control())
        {
            return Err(KeySequenceError::InvalidStroke {
                stroke: stroke.to_owned(),
            });
        }
    }
    Ok(())
}

/// Validated GPUI-compatible boolean context expression. This core validator
/// checks the portable lexical/balance contract; the app still passes it to
/// GPUI's parser before atomically installing a complete map.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ContextExpression(String);

impl ContextExpression {
    pub fn parse(value: impl Into<String>) -> Result<Self, ContextExpressionError> {
        let value = value.into();
        validate_context_expression(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn specificity(&self) -> usize {
        self.0.split("&&").count()
    }
}

impl fmt::Display for ContextExpression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ContextExpression {
    type Err = ContextExpressionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for ContextExpression {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ContextExpressionError {
    #[error("key context expression is empty")]
    Empty,
    #[error("key context expression exceeds {MAX_CONTEXT_BYTES} bytes")]
    TooLong,
    #[error("key context expression has surrounding whitespace or control characters")]
    NonCanonicalWhitespace,
    #[error(
        "key context expression contains unsupported character {character:?} at byte {byte_index}"
    )]
    InvalidCharacter { byte_index: usize, character: char },
    #[error("key context expression has unbalanced parentheses")]
    UnbalancedParentheses,
    #[error("key context expression contains an incomplete boolean/equality operator")]
    IncompleteOperator,
}

fn validate_context_expression(value: &str) -> Result<(), ContextExpressionError> {
    if value.is_empty() {
        return Err(ContextExpressionError::Empty);
    }
    if value.len() > MAX_CONTEXT_BYTES {
        return Err(ContextExpressionError::TooLong);
    }
    if value.trim() != value || value.chars().any(char::is_control) {
        return Err(ContextExpressionError::NonCanonicalWhitespace);
    }
    let mut depth = 0usize;
    for (byte_index, character) in value.char_indices() {
        if !(character.is_ascii_alphanumeric()
            || matches!(
                character,
                '_' | '.'
                    | '-'
                    | ' '
                    | '('
                    | ')'
                    | '&'
                    | '|'
                    | '!'
                    | '='
                    | '>'
                    | '<'
                    | '~'
                    | '"'
                    | '?'
            ))
        {
            return Err(ContextExpressionError::InvalidCharacter {
                byte_index,
                character,
            });
        }
        match character {
            '(' => depth += 1,
            ')' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or(ContextExpressionError::UnbalancedParentheses)?;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(ContextExpressionError::UnbalancedParentheses);
    }

    validate_context_operators(value.as_bytes())?;
    Ok(())
}

fn validate_context_operators(bytes: &[u8]) -> Result<(), ContextExpressionError> {
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'&' => {
                if bytes.get(index + 1) != Some(&b'&') || bytes.get(index + 2) == Some(&b'&') {
                    return Err(ContextExpressionError::IncompleteOperator);
                }
                index += 2;
            }
            b'|' => {
                if bytes.get(index + 1) != Some(&b'|') || bytes.get(index + 2) == Some(&b'|') {
                    return Err(ContextExpressionError::IncompleteOperator);
                }
                index += 2;
            }
            b'=' => {
                if bytes.get(index + 1) != Some(&b'=') || bytes.get(index + 2) == Some(&b'=') {
                    return Err(ContextExpressionError::IncompleteOperator);
                }
                index += 2;
            }
            b'!' if bytes.get(index + 1) == Some(&b'=') => {
                if bytes.get(index + 2) == Some(&b'=') {
                    return Err(ContextExpressionError::IncompleteOperator);
                }
                index += 2;
            }
            _ => index += 1,
        }
    }
    Ok(())
}

/// Zed-style binding value: action ID, `[action_id, arguments]`, or `null`.
#[derive(Clone, Debug, PartialEq)]
pub enum KeymapBinding {
    Disabled,
    Action {
        action_id: ActionId,
        arguments: Value,
    },
}

impl KeymapBinding {
    pub fn action(action_id: ActionId) -> Self {
        Self::Action {
            action_id,
            arguments: Value::Object(Default::default()),
        }
    }

    pub fn parameterized(action_id: ActionId, arguments: Value) -> Self {
        Self::Action {
            action_id,
            arguments,
        }
    }

    pub fn action_id(&self) -> Option<&ActionId> {
        match self {
            Self::Disabled => None,
            Self::Action { action_id, .. } => Some(action_id),
        }
    }

    pub fn arguments(&self) -> Option<&Value> {
        match self {
            Self::Disabled => None,
            Self::Action { arguments, .. } => Some(arguments),
        }
    }
}

impl Serialize for KeymapBinding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Disabled => serializer.serialize_none(),
            Self::Action {
                action_id,
                arguments,
            } if arguments
                .as_object()
                .is_some_and(|object| object.is_empty()) =>
            {
                action_id.serialize(serializer)
            }
            Self::Action {
                action_id,
                arguments,
            } => (action_id, arguments).serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for KeymapBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<Value>::deserialize(deserializer)?;
        let Some(value) = value else {
            return Ok(Self::Disabled);
        };
        match value {
            Value::String(action_id) => Ok(Self::action(
                ActionId::parse(action_id).map_err(de::Error::custom)?,
            )),
            Value::Array(values) if values.len() == 2 => {
                let mut values = values.into_iter();
                let action_id = values
                    .next()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .ok_or_else(|| de::Error::custom("binding action ID must be a string"))?;
                let arguments = values.next().expect("array length was checked");
                if !arguments.is_object() {
                    return Err(de::Error::custom(
                        "parameterized binding arguments must be an object",
                    ));
                }
                Ok(Self::parameterized(
                    ActionId::parse(action_id).map_err(de::Error::custom)?,
                    arguments,
                ))
            }
            _ => Err(de::Error::custom(
                "binding must be an action ID, [action ID, argument object], or null",
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeymapEntry {
    pub context: ContextExpression,
    #[serde(deserialize_with = "deserialize_bindings")]
    pub bindings: BTreeMap<KeySequence, KeymapBinding>,
}

fn deserialize_bindings<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<KeySequence, KeymapBinding>, D::Error>
where
    D: Deserializer<'de>,
{
    struct BindingsVisitor;

    impl<'de> Visitor<'de> for BindingsVisitor {
        type Value = BTreeMap<KeySequence, KeymapBinding>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a keymap bindings object with unique key sequences")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut bindings = BTreeMap::new();
            while let Some((sequence, binding)) = map.next_entry::<KeySequence, KeymapBinding>()? {
                if bindings.insert(sequence.clone(), binding).is_some() {
                    return Err(de::Error::custom(format_args!(
                        "duplicate binding sequence {:?}",
                        sequence.as_str()
                    )));
                }
            }
            Ok(bindings)
        }
    }

    deserializer.deserialize_map(BindingsVisitor)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeymapDocument(pub Vec<KeymapEntry>);

impl KeymapDocument {
    pub fn parse(input: &str) -> Result<Self, KeymapParseError> {
        serde_json::from_str(input).map_err(KeymapParseError::Json)
    }

    pub fn validate(&self, registry: &ActionRegistry) -> Result<(), Vec<KeymapValidationError>> {
        let mut errors = Vec::new();
        for (entry_index, entry) in self.0.iter().enumerate() {
            for (sequence, binding) in &entry.bindings {
                let KeymapBinding::Action {
                    action_id,
                    arguments,
                } = binding
                else {
                    continue;
                };
                let Some(descriptor) = registry.descriptor(action_id) else {
                    errors.push(KeymapValidationError::UnknownAction {
                        entry_index,
                        sequence: sequence.clone(),
                        action_id: action_id.clone(),
                    });
                    continue;
                };
                if !descriptor.bindable {
                    errors.push(KeymapValidationError::ActionNotBindable {
                        entry_index,
                        sequence: sequence.clone(),
                        action_id: action_id.clone(),
                    });
                }
                if let Err(source) = descriptor.validate_arguments(arguments) {
                    errors.push(KeymapValidationError::InvalidArguments {
                        entry_index,
                        sequence: sequence.clone(),
                        action_id: action_id.clone(),
                        source,
                    });
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[derive(Debug, Error)]
pub enum KeymapParseError {
    #[error("invalid keymap JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum KeymapValidationError {
    #[error("entry {entry_index} sequence {sequence} references unknown action {action_id}")]
    UnknownAction {
        entry_index: usize,
        sequence: KeySequence,
        action_id: ActionId,
    },
    #[error("entry {entry_index} sequence {sequence} references non-bindable action {action_id}")]
    ActionNotBindable {
        entry_index: usize,
        sequence: KeySequence,
        action_id: ActionId,
    },
    #[error(
        "entry {entry_index} sequence {sequence} has invalid arguments for {action_id}: {source}"
    )]
    InvalidArguments {
        entry_index: usize,
        sequence: KeySequence,
        action_id: ActionId,
        source: SchemaValidationError,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BindingSource {
    Template(ShortcutProfile),
    User,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResolvedBindingState {
    Effective,
    Shadowed,
    Disabled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedBinding {
    pub context: ContextExpression,
    pub sequence: KeySequence,
    pub binding: KeymapBinding,
    pub source: BindingSource,
    pub source_entry: usize,
    pub state: ResolvedBindingState,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KeymapConflictKind {
    Exact,
    Prefix,
    Shadowed,
    Possible,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeymapConflict {
    pub kind: KeymapConflictKind,
    pub left: usize,
    pub right: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedKeymap {
    pub profile: ShortcutProfile,
    pub bindings: Vec<ResolvedBinding>,
    pub conflicts: Vec<KeymapConflict>,
}

/// Resolves exactly one selected first-party template, then user overrides.
pub fn resolve_keymap(
    profile: ShortcutProfile,
    standard: &KeymapDocument,
    vim: &KeymapDocument,
    user: &KeymapDocument,
    registry: &ActionRegistry,
) -> Result<ResolvedKeymap, Vec<KeymapValidationError>> {
    let template = match profile {
        ShortcutProfile::Standard => standard,
        ShortcutProfile::Vim => vim,
    };
    template.validate(registry)?;
    user.validate(registry)?;

    let mut bindings = Vec::new();
    flatten_bindings(template, BindingSource::Template(profile), &mut bindings);
    flatten_bindings(user, BindingSource::User, &mut bindings);

    let mut latest_by_exact_key: HashMap<(ContextExpression, KeySequence), usize> = HashMap::new();
    let mut conflicts = Vec::new();
    for index in 0..bindings.len() {
        let exact_key = (
            bindings[index].context.clone(),
            bindings[index].sequence.clone(),
        );
        if let Some(previous) = latest_by_exact_key.insert(exact_key, index) {
            bindings[previous].state = ResolvedBindingState::Shadowed;
            conflicts.push(KeymapConflict {
                kind: KeymapConflictKind::Shadowed,
                left: previous,
                right: index,
            });
        }
        if matches!(bindings[index].binding, KeymapBinding::Disabled) {
            bindings[index].state = ResolvedBindingState::Disabled;
        }
    }

    for left in 0..bindings.len() {
        if bindings[left].state != ResolvedBindingState::Effective {
            continue;
        }
        for right in (left + 1)..bindings.len() {
            if bindings[right].state != ResolvedBindingState::Effective {
                continue;
            }
            let same_sequence = bindings[left].sequence == bindings[right].sequence;
            let prefix = bindings[left]
                .sequence
                .is_prefix_of(&bindings[right].sequence)
                || bindings[right]
                    .sequence
                    .is_prefix_of(&bindings[left].sequence);
            if !same_sequence && !prefix {
                continue;
            }
            let overlap = context_overlap(&bindings[left].context, &bindings[right].context);
            match overlap {
                ContextOverlap::Disjoint => {}
                ContextOverlap::Exact => conflicts.push(KeymapConflict {
                    kind: if same_sequence {
                        KeymapConflictKind::Exact
                    } else {
                        KeymapConflictKind::Prefix
                    },
                    left,
                    right,
                }),
                ContextOverlap::Possible => conflicts.push(KeymapConflict {
                    kind: KeymapConflictKind::Possible,
                    left,
                    right,
                }),
            }
        }
    }

    Ok(ResolvedKeymap {
        profile,
        bindings,
        conflicts,
    })
}

fn flatten_bindings(
    document: &KeymapDocument,
    source: BindingSource,
    output: &mut Vec<ResolvedBinding>,
) {
    for (entry_index, entry) in document.0.iter().enumerate() {
        for (sequence, binding) in &entry.bindings {
            output.push(ResolvedBinding {
                context: entry.context.clone(),
                sequence: sequence.clone(),
                binding: binding.clone(),
                source,
                source_entry: entry_index,
                state: ResolvedBindingState::Effective,
            });
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContextOverlap {
    Exact,
    Disjoint,
    Possible,
}

fn context_overlap(left: &ContextExpression, right: &ContextExpression) -> ContextOverlap {
    if left == right {
        return ContextOverlap::Exact;
    }
    let Some(left_equalities) = equality_constraints(left.as_str()) else {
        return ContextOverlap::Possible;
    };
    let Some(right_equalities) = equality_constraints(right.as_str()) else {
        return ContextOverlap::Possible;
    };
    for (key, left_value) in &left_equalities {
        if let Some(right_value) = right_equalities.get(key)
            && left_value != right_value
        {
            return ContextOverlap::Disjoint;
        }
    }
    ContextOverlap::Possible
}

fn equality_constraints(context: &str) -> Option<HashMap<String, String>> {
    // Only prove disjointness for a flat conjunction. OR, negation,
    // grouping, and descendant expressions require a real predicate solver;
    // treating a textual equality inside one of them as unconditional would
    // suppress a genuine possible conflict.
    if context.contains("||") || context.contains('!') || context.contains(['(', ')', '>']) {
        return None;
    }

    let mut equalities = HashMap::new();
    for term in context.split("&&") {
        let mut parts = term.split("==");
        let key = parts.next()?;
        let Some(value) = parts.next() else {
            continue;
        };
        if parts.next().is_some() {
            return None;
        }
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            return None;
        }
        match equalities.entry(key.to_owned()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(value.to_owned());
            }
            std::collections::hash_map::Entry::Occupied(entry) if entry.get() == value => {}
            std::collections::hash_map::Entry::Occupied(_) => return None,
        }
    }
    Some(equalities)
}

/// Keeps an active last-known-good map when a hand-edited reload is invalid.
#[derive(Clone, Debug)]
pub struct LastKnownGoodKeymap {
    active: ResolvedKeymap,
    last_error: Option<Vec<KeymapValidationError>>,
}

impl LastKnownGoodKeymap {
    pub fn new(active: ResolvedKeymap) -> Self {
        Self {
            active,
            last_error: None,
        }
    }

    pub fn active(&self) -> &ResolvedKeymap {
        &self.active
    }

    pub fn last_error(&self) -> Option<&[KeymapValidationError]> {
        self.last_error.as_deref()
    }

    pub fn apply(&mut self, candidate: Result<ResolvedKeymap, Vec<KeymapValidationError>>) -> bool {
        match candidate {
            Ok(candidate) => {
                self.active = candidate;
                self.last_error = None;
                true
            }
            Err(errors) => {
                self.last_error = Some(errors);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        ActionDescriptor, ActionEffect, AuditSpec, InvocationPolicy, Recoverability,
        RegistryBuilder, SCHEMA_VERSION,
    };

    fn descriptor(id: &str) -> ActionDescriptor {
        ActionDescriptor {
            schema_version: SCHEMA_VERSION,
            id: ActionId::parse(id).unwrap(),
            label: id.into(),
            description: format!("Description for {id}"),
            category: "Test".into(),
            argument_schema: json!({
                "type": "object",
                "properties": {"steer": {"type": "boolean"}},
                "additionalProperties": false
            }),
            result_schema: json!({"type": "object"}),
            contexts: Vec::new(),
            precondition_domain: None,
            effect: ActionEffect::Navigate,
            invocation_policy: InvocationPolicy::ControllerCallable,
            recoverability: Recoverability::Ephemeral,
            audit: AuditSpec::default(),
            default_bindings: Vec::new(),
            bindable: true,
            terminal_host_action: false,
        }
    }

    fn registry() -> ActionRegistry {
        let mut builder = RegistryBuilder::new();
        for id in ["task.new", "composer.send", "transcript.focus_next"] {
            let action_id = ActionId::parse(id).unwrap();
            builder.register(descriptor(id));
            builder.register_adapter(action_id);
        }
        builder.build().unwrap()
    }

    #[test]
    fn zed_style_wire_format_round_trips_string_parameterized_and_null() {
        let document = KeymapDocument::parse(
            r#"[
                {
                    "context": "MapleApp && profile == vim",
                    "bindings": {
                        "j": "transcript.focus_next",
                        "cmd-k": null,
                        "ctrl-enter": ["composer.send", {"steer": true}]
                    }
                }
            ]"#,
        )
        .unwrap();
        assert_eq!(document.0.len(), 1);
        assert!(matches!(
            document.0[0].bindings[&KeySequence::parse("cmd-k").unwrap()],
            KeymapBinding::Disabled
        ));
        assert!(document.validate(&registry()).is_ok());
        let encoded = serde_json::to_value(&document).unwrap();
        assert!(encoded[0]["bindings"]["cmd-k"].is_null());
        assert_eq!(encoded[0]["bindings"]["j"], "transcript.focus_next");
    }

    #[test]
    fn malformed_context_operator_runs_are_rejected() {
        for context in [
            "MapleApp &&& profile == vim",
            "MapleApp ||| profile == vim",
            "MapleApp & profile == vim",
            "MapleApp | profile == vim",
            "profile = vim",
            "profile === vim",
            "profile !== vim",
        ] {
            assert_eq!(
                ContextExpression::parse(context).unwrap_err(),
                ContextExpressionError::IncompleteOperator,
                "context {context:?} should be rejected"
            );
        }

        for context in [
            "MapleApp && profile == vim",
            "MapleApp || profile != vim",
            "!MapleApp",
            "Pane > Editor",
            "vim_operator == >",
        ] {
            assert!(
                ContextExpression::parse(context).is_ok(),
                "context {context:?} should pass portable lexical validation"
            );
        }
    }

    #[test]
    fn duplicate_binding_keys_are_rejected_during_json_parse() {
        let error = KeymapDocument::parse(
            r#"[{"context":"MapleApp","bindings":{"j":"task.new","j":"composer.send"}}]"#,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate binding sequence \"j\"")
        );
    }

    #[test]
    fn unknown_fields_ids_and_arguments_are_rejected() {
        assert!(
            KeymapDocument::parse(r#"[{"context":"MapleApp","unexpected":true,"bindings":{}}]"#)
                .is_err()
        );
        let unknown =
            KeymapDocument::parse(r#"[{"context":"MapleApp","bindings":{"x":"unknown.action"}}]"#)
                .unwrap();
        assert!(matches!(
            unknown.validate(&registry()).unwrap_err()[0],
            KeymapValidationError::UnknownAction { .. }
        ));
        let invalid_arguments = KeymapDocument::parse(
            r#"[{"context":"MapleApp","bindings":{"x":["composer.send",{"steer":"yes"}]}}]"#,
        )
        .unwrap();
        assert!(matches!(
            invalid_arguments.validate(&registry()).unwrap_err()[0],
            KeymapValidationError::InvalidArguments { .. }
        ));
    }

    #[test]
    fn selected_templates_replace_instead_of_stack() {
        let standard =
            KeymapDocument::parse(r#"[{"context":"MapleApp","bindings":{"cmd-n":"task.new"}}]"#)
                .unwrap();
        let vim = KeymapDocument::parse(
            r#"[{"context":"MapleApp && profile == vim","bindings":{"space s n":"task.new"}}]"#,
        )
        .unwrap();
        let user = KeymapDocument::default();
        let standard_resolved = resolve_keymap(
            ShortcutProfile::Standard,
            &standard,
            &vim,
            &user,
            &registry(),
        )
        .unwrap();
        assert!(
            standard_resolved
                .bindings
                .iter()
                .any(|binding| binding.sequence.as_str() == "cmd-n")
        );
        assert!(
            standard_resolved
                .bindings
                .iter()
                .all(|binding| binding.sequence.as_str() != "space s n")
        );
        let vim_resolved =
            resolve_keymap(ShortcutProfile::Vim, &standard, &vim, &user, &registry()).unwrap();
        assert!(
            vim_resolved
                .bindings
                .iter()
                .all(|binding| binding.sequence.as_str() != "cmd-n")
        );
    }

    #[test]
    fn user_override_and_null_shadow_template_at_equal_context() {
        let standard = KeymapDocument::parse(
            r#"[{"context":"MapleApp","bindings":{"cmd-n":"task.new","cmd-k":"task.new"}}]"#,
        )
        .unwrap();
        let user = KeymapDocument::parse(
            r#"[{"context":"MapleApp","bindings":{"cmd-n":"composer.send","cmd-k":null}}]"#,
        )
        .unwrap();
        let resolved = resolve_keymap(
            ShortcutProfile::Standard,
            &standard,
            &KeymapDocument::default(),
            &user,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            resolved
                .bindings
                .iter()
                .filter(|binding| binding.state == ResolvedBindingState::Shadowed)
                .count(),
            2
        );
        assert!(resolved.bindings.iter().any(|binding| {
            binding.sequence.as_str() == "cmd-k" && binding.state == ResolvedBindingState::Disabled
        }));
    }

    #[test]
    fn conflict_detection_classifies_prefix_and_possible_overlap() {
        let template = KeymapDocument::parse(
            r#"[
                {"context":"MapleApp && profile == vim","bindings":{"g":"task.new","g g":"task.new"}},
                {"context":"MapleApp && region == transcript","bindings":{"g":"transcript.focus_next"}}
            ]"#,
        )
        .unwrap();
        let resolved = resolve_keymap(
            ShortcutProfile::Vim,
            &KeymapDocument::default(),
            &template,
            &KeymapDocument::default(),
            &registry(),
        )
        .unwrap();
        assert!(
            resolved
                .conflicts
                .iter()
                .any(|conflict| conflict.kind == KeymapConflictKind::Prefix)
        );
        assert!(
            resolved
                .conflicts
                .iter()
                .any(|conflict| conflict.kind == KeymapConflictKind::Possible)
        );
    }

    #[test]
    fn overlap_proof_is_conservative_for_complex_predicates() {
        let standard = ContextExpression::parse("profile == standard").unwrap();

        assert_eq!(
            context_overlap(
                &ContextExpression::parse("profile == vim").unwrap(),
                &standard,
            ),
            ContextOverlap::Disjoint
        );

        for context in [
            "profile == vim || region == transcript",
            "!(profile == vim)",
            "(profile == vim)",
            "Pane > profile == vim",
        ] {
            assert_eq!(
                context_overlap(&ContextExpression::parse(context).unwrap(), &standard),
                ContextOverlap::Possible,
                "complex context {context:?} must not produce a false disjointness proof"
            );
        }
    }

    #[test]
    fn invalid_reload_preserves_last_known_good() {
        let standard =
            KeymapDocument::parse(r#"[{"context":"MapleApp","bindings":{"cmd-n":"task.new"}}]"#)
                .unwrap();
        let valid = resolve_keymap(
            ShortcutProfile::Standard,
            &standard,
            &KeymapDocument::default(),
            &KeymapDocument::default(),
            &registry(),
        )
        .unwrap();
        let mut active = LastKnownGoodKeymap::new(valid);
        let old_bindings = active.active().bindings.clone();
        let invalid =
            KeymapDocument::parse(r#"[{"context":"MapleApp","bindings":{"x":"missing.action"}}]"#)
                .unwrap();
        let candidate = resolve_keymap(
            ShortcutProfile::Standard,
            &standard,
            &KeymapDocument::default(),
            &invalid,
            &registry(),
        );
        assert!(!active.apply(candidate));
        assert_eq!(active.active().bindings, old_bindings);
        assert!(active.last_error().is_some());
    }
}
