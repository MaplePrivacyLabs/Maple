use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use thiserror::Error;

use crate::{
    ActionDescriptor, ActionEffect, ActionId, AuditSpecError, ContextExpression,
    ContextExpressionError, InvocationPolicy, KeySequence, KeySequenceError, SCHEMA_VERSION,
};

#[derive(Clone, Debug, Default)]
pub struct RegistryBuilder {
    descriptors: Vec<ActionDescriptor>,
    adapters: Vec<ActionId>,
}

impl RegistryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, descriptor: ActionDescriptor) -> &mut Self {
        self.descriptors.push(descriptor);
        self
    }

    /// Records that the app provides the one typed GPUI adapter corresponding
    /// to a bindable semantic action. The core stores only the stable ID and
    /// remains GPUI-free.
    pub fn register_adapter(&mut self, action_id: ActionId) -> &mut Self {
        self.adapters.push(action_id);
        self
    }

    pub fn build(self) -> Result<ActionRegistry, Vec<RegistryValidationError>> {
        let mut errors = Vec::new();
        let mut descriptors = BTreeMap::new();
        for descriptor in self.descriptors {
            if descriptors.contains_key(&descriptor.id) {
                errors.push(RegistryValidationError::DuplicateActionId(
                    descriptor.id.clone(),
                ));
                continue;
            }
            validate_descriptor(&descriptor, &mut errors);
            descriptors.insert(descriptor.id.clone(), descriptor);
        }

        let mut adapters = BTreeSet::new();
        for action_id in self.adapters {
            if !adapters.insert(action_id.clone()) {
                errors.push(RegistryValidationError::DuplicateAdapter(action_id));
            }
        }
        for adapter in &adapters {
            match descriptors.get(adapter) {
                None => errors.push(RegistryValidationError::AdapterForUnknownAction(
                    adapter.clone(),
                )),
                Some(descriptor) if !descriptor.bindable => errors.push(
                    RegistryValidationError::AdapterForNonBindableAction(adapter.clone()),
                ),
                Some(_) => {}
            }
        }
        for descriptor in descriptors.values() {
            if descriptor.bindable && !adapters.contains(&descriptor.id) {
                errors.push(RegistryValidationError::MissingAdapter(
                    descriptor.id.clone(),
                ));
            }
        }

        if errors.is_empty() {
            Ok(ActionRegistry {
                descriptors,
                adapters,
            })
        } else {
            Err(errors)
        }
    }
}

fn validate_descriptor(descriptor: &ActionDescriptor, errors: &mut Vec<RegistryValidationError>) {
    if descriptor.schema_version != SCHEMA_VERSION {
        errors.push(RegistryValidationError::UnsupportedSchemaVersion {
            action_id: descriptor.id.clone(),
            schema_version: descriptor.schema_version,
            supported: SCHEMA_VERSION,
        });
    }
    validate_required_copy(descriptor, errors);
    validate_schema(
        &descriptor.id,
        SchemaKind::Arguments,
        &descriptor.argument_schema,
        errors,
    );
    validate_schema(
        &descriptor.id,
        SchemaKind::Result,
        &descriptor.result_schema,
        errors,
    );
    if let Err(source) = descriptor.audit.validate() {
        errors.push(RegistryValidationError::UnsafeAuditSpec {
            action_id: descriptor.id.clone(),
            source,
        });
    }
    if !descriptor.bindable && !descriptor.default_bindings.is_empty() {
        errors.push(RegistryValidationError::DefaultBindingOnNonBindableAction(
            descriptor.id.clone(),
        ));
    }
    if requires_human_only(&descriptor.id)
        && descriptor.invocation_policy != InvocationPolicy::HumanOnly
    {
        errors.push(RegistryValidationError::AuthorityActionNotHumanOnly(
            descriptor.id.clone(),
        ));
    }
    if descriptor.id.as_str() == "permission.respond"
        && (descriptor.invocation_policy != InvocationPolicy::ControllerCallable
            || descriptor.effect != ActionEffect::MutateMaple)
    {
        errors.push(RegistryValidationError::InvalidPermissionRespondContract(
            descriptor.id.clone(),
        ));
    }
    if descriptor.id.as_str() == "app.quit"
        && (!descriptor.terminal_host_action
            || descriptor.invocation_policy != InvocationPolicy::ControllerCallable)
    {
        errors.push(RegistryValidationError::InvalidAppQuitContract(
            descriptor.id.clone(),
        ));
    }
    for (binding_index, binding) in descriptor.default_bindings.iter().enumerate() {
        if let Err(source) = ContextExpression::parse(binding.context.clone()) {
            errors.push(RegistryValidationError::InvalidDefaultContext {
                action_id: descriptor.id.clone(),
                binding_index,
                source,
            });
        }
        if let Err(source) = KeySequence::parse(binding.sequence.clone()) {
            errors.push(RegistryValidationError::InvalidDefaultSequence {
                action_id: descriptor.id.clone(),
                binding_index,
                source,
            });
        }
        if let Err(source) = descriptor.validate_arguments(&binding.arguments) {
            errors.push(RegistryValidationError::InvalidDefaultArguments {
                action_id: descriptor.id.clone(),
                binding_index,
                message: source.to_string(),
            });
        }
    }
}

fn validate_required_copy(
    descriptor: &ActionDescriptor,
    errors: &mut Vec<RegistryValidationError>,
) {
    for (field, value) in [
        (DescriptorCopyField::Label, descriptor.label.as_str()),
        (
            DescriptorCopyField::Description,
            descriptor.description.as_str(),
        ),
        (DescriptorCopyField::Category, descriptor.category.as_str()),
    ] {
        if value.trim().is_empty() || value.trim() != value {
            errors.push(RegistryValidationError::MissingOrInvalidCopy {
                action_id: descriptor.id.clone(),
                field,
            });
        }
    }
}

fn validate_schema(
    action_id: &ActionId,
    kind: SchemaKind,
    schema: &Value,
    errors: &mut Vec<RegistryValidationError>,
) {
    if !schema.is_object() {
        errors.push(RegistryValidationError::MissingSchema {
            action_id: action_id.clone(),
            kind,
        });
        return;
    }
    if let Err(error) = jsonschema::validator_for(schema) {
        errors.push(RegistryValidationError::InvalidSchema {
            action_id: action_id.clone(),
            kind,
            message: error.to_string(),
        });
    }
}

fn requires_human_only(action_id: &ActionId) -> bool {
    let id = action_id.as_str();
    id.starts_with("auth.")
        || matches!(
            id,
            "account.sign_out"
                | "account.delete"
                | "code_mode.set_enabled"
                | "code_mode.set_controller_access"
        )
        || (id.starts_with("account.")
            && [
                "delete",
                "email",
                "password",
                "recovery",
                "mfa",
                "credential",
                "token",
                "revoke",
            ]
            .iter()
            .any(|sensitive| id.contains(sensitive)))
}

#[derive(Clone, Debug)]
pub struct ActionRegistry {
    descriptors: BTreeMap<ActionId, ActionDescriptor>,
    adapters: BTreeSet<ActionId>,
}

impl ActionRegistry {
    pub fn descriptor(&self, action_id: &ActionId) -> Option<&ActionDescriptor> {
        self.descriptors.get(action_id)
    }

    pub fn contains(&self, action_id: &ActionId) -> bool {
        self.descriptors.contains_key(action_id)
    }

    pub fn has_adapter(&self, action_id: &ActionId) -> bool {
        self.adapters.contains(action_id)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&ActionId, &ActionDescriptor)> {
        self.descriptors.iter()
    }

    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaKind {
    Arguments,
    Result,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorCopyField {
    Label,
    Description,
    Category,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum RegistryValidationError {
    #[error("duplicate stable action ID {0}")]
    DuplicateActionId(ActionId),
    #[error(
        "action {action_id} uses schema version {schema_version}, but this host supports {supported}"
    )]
    UnsupportedSchemaVersion {
        action_id: ActionId,
        schema_version: u16,
        supported: u16,
    },
    #[error("action {action_id} has missing or invalid {field:?}")]
    MissingOrInvalidCopy {
        action_id: ActionId,
        field: DescriptorCopyField,
    },
    #[error("action {action_id} is missing an object-shaped {kind:?} schema")]
    MissingSchema {
        action_id: ActionId,
        kind: SchemaKind,
    },
    #[error("action {action_id} has invalid {kind:?} schema: {message}")]
    InvalidSchema {
        action_id: ActionId,
        kind: SchemaKind,
        message: String,
    },
    #[error("action {action_id} has an unsafe audit specification: {source}")]
    UnsafeAuditSpec {
        action_id: ActionId,
        source: AuditSpecError,
    },
    #[error("authority-changing action {0} must be Human Only")]
    AuthorityActionNotHumanOnly(ActionId),
    #[error("{0} must be a controller-callable Mutate Maple action")]
    InvalidPermissionRespondContract(ActionId),
    #[error("{0} must be controller-callable and marked as a terminal host action")]
    InvalidAppQuitContract(ActionId),
    #[error("action {action_id} default binding {binding_index} has invalid context: {source}")]
    InvalidDefaultContext {
        action_id: ActionId,
        binding_index: usize,
        source: ContextExpressionError,
    },
    #[error("action {action_id} default binding {binding_index} has invalid sequence: {source}")]
    InvalidDefaultSequence {
        action_id: ActionId,
        binding_index: usize,
        source: KeySequenceError,
    },
    #[error("action {action_id} default binding {binding_index} has invalid arguments: {message}")]
    InvalidDefaultArguments {
        action_id: ActionId,
        binding_index: usize,
        message: String,
    },
    #[error("bindable action {0} has no registered typed GPUI adapter")]
    MissingAdapter(ActionId),
    #[error("non-bindable action {0} cannot declare default key bindings")]
    DefaultBindingOnNonBindableAction(ActionId),
    #[error("duplicate typed GPUI adapter registration for {0}")]
    DuplicateAdapter(ActionId),
    #[error("typed GPUI adapter references unknown action {0}")]
    AdapterForUnknownAction(ActionId),
    #[error("typed GPUI adapter references non-bindable action {0}")]
    AdapterForNonBindableAction(ActionId),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        ActionEffect, AuditSpec, DefaultBinding, InvocationPolicy, Recoverability, ShortcutProfile,
    };

    fn descriptor(id: &str) -> ActionDescriptor {
        ActionDescriptor {
            schema_version: SCHEMA_VERSION,
            id: ActionId::parse(id).unwrap(),
            label: "Test action".into(),
            description: "A complete test action descriptor".into(),
            category: "Tests".into(),
            argument_schema: json!({"type": "object", "additionalProperties": false}),
            result_schema: json!({"type": "object"}),
            contexts: Vec::new(),
            effect: ActionEffect::Observe,
            invocation_policy: InvocationPolicy::ControllerCallable,
            recoverability: Recoverability::Ephemeral,
            audit: AuditSpec::default(),
            default_bindings: Vec::new(),
            precondition_domain: None,
            bindable: true,
            terminal_host_action: false,
        }
    }

    #[test]
    fn registry_rejects_duplicate_descriptors() {
        let descriptor = descriptor("task.open");
        let mut builder = RegistryBuilder::new();
        builder.register(descriptor.clone()).register(descriptor);
        builder.register_adapter(ActionId::parse("task.open").unwrap());
        assert!(
            builder
                .build()
                .unwrap_err()
                .iter()
                .any(|error| matches!(error, RegistryValidationError::DuplicateActionId(_)))
        );
    }

    #[test]
    fn registry_requires_documentation_schemas_and_typed_adapter() {
        let mut invalid = descriptor("task.open");
        invalid.label = " ".into();
        invalid.result_schema = Value::Null;
        let mut builder = RegistryBuilder::new();
        builder.register(invalid);
        let errors = builder.build().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| matches!(error, RegistryValidationError::MissingOrInvalidCopy { .. }))
        );
        assert!(errors.iter().any(|error| matches!(
            error,
            RegistryValidationError::MissingSchema {
                kind: SchemaKind::Result,
                ..
            }
        )));
        assert!(
            errors
                .iter()
                .any(|error| matches!(error, RegistryValidationError::MissingAdapter(_)))
        );
    }

    #[test]
    fn registry_rejects_unparsable_or_schema_invalid_defaults() {
        let mut invalid = descriptor("composer.send");
        invalid.argument_schema = json!({
            "type": "object",
            "properties": {"steer": {"type": "boolean"}},
            "additionalProperties": false
        });
        invalid.default_bindings = vec![DefaultBinding {
            profile: ShortcutProfile::Standard,
            context: "MapleApp && (".into(),
            sequence: "ctrl-enter  ".into(),
            arguments: json!({"steer": "yes"}),
        }];
        let mut builder = RegistryBuilder::new();
        builder.register(invalid);
        builder.register_adapter(ActionId::parse("composer.send").unwrap());
        let errors = builder.build().unwrap_err();
        assert!(
            errors.iter().any(|error| matches!(
                error,
                RegistryValidationError::InvalidDefaultContext { .. }
            ))
        );
        assert!(errors.iter().any(|error| matches!(
            error,
            RegistryValidationError::InvalidDefaultSequence { .. }
        )));
        assert!(errors.iter().any(|error| matches!(
            error,
            RegistryValidationError::InvalidDefaultArguments { .. }
        )));
    }

    #[test]
    fn authority_changing_actions_must_be_human_only() {
        let mut authority = descriptor("code_mode.set_controller_access");
        authority.invocation_policy = InvocationPolicy::ControllerCallable;
        let mut builder = RegistryBuilder::new();
        builder.register(authority);
        builder.register_adapter(ActionId::parse("code_mode.set_controller_access").unwrap());
        assert!(builder.build().unwrap_err().iter().any(|error| matches!(
            error,
            RegistryValidationError::AuthorityActionNotHumanOnly(_)
        )));
    }

    #[test]
    fn valid_registry_preserves_descriptor_and_adapter_identity() {
        let action_id = ActionId::parse("permission.respond").unwrap();
        let mut action = descriptor(action_id.as_str());
        action.effect = ActionEffect::MutateMaple;
        let mut builder = RegistryBuilder::new();
        builder.register(action).register_adapter(action_id.clone());
        let registry = builder.build().unwrap();
        assert!(registry.contains(&action_id));
        assert!(registry.has_adapter(&action_id));
        assert_eq!(registry.descriptor(&action_id).unwrap().id, action_id);
    }
}
