//! Per-model normalization of client inference requests before a provider sees
//! them: reasoning-effort validation and clamping, thinking-off switches a
//! model cannot honor, and message roles a model's renderer rejects.
//!
//! Every rule reads one table, [`crate::model_config::ModelReasoning`], which
//! also produces the catalog `reasoning` object. Chat Completions and Responses
//! both call in here after alias resolution, so a level means the same thing
//! on every surface and matches what the catalog promised.

use crate::model_config::{model_config, model_reasoning, ReasoningEffort};
use crate::ApiError;
use serde_json::{json, Map, Value};
use std::fmt;
use tracing::debug;

/// The request field that carries the effort, named as OpenAI names it on
/// each API.
pub(crate) const CHAT_REASONING_EFFORT_PARAM: &str = "reasoning_effort";
pub(crate) const RESPONSES_REASONING_EFFORT_PARAM: &str = "reasoning.effort";

/// Template switches that turn thinking off on models that support it.
const THINKING_OFF_SWITCHES: [&str; 2] = ["enable_thinking", "thinking"];

/// Longest client value echoed back in an error.
const MAX_ECHOED_VALUE_CHARS: usize = 32;

/// How the caller chose the model. An explicit model gets exactly what it
/// asked for or a 400, as OpenAI does. An Auto alias is moved to the nearest
/// effort the resolved model accepts, because a health fallback to another
/// model must never fail a running task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelSelection {
    Explicit,
    Alias,
}

impl ModelSelection {
    pub(crate) fn for_request(requested_model: &str, alias_target: &str) -> Self {
        if requested_model == alias_target {
            Self::Explicit
        } else {
            Self::Alias
        }
    }
}

/// The effort a request ends up with for the resolved model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectiveReasoningEffort {
    /// The request named no effort, or the model has no reasoning controls;
    /// the body is forwarded as the client sent it.
    Unchanged,
    /// The request names, or is moved to, this effort.
    Set(ReasoningEffort),
}

/// A reasoning effort the resolved model does not accept. Rendered in
/// OpenAI's wording so clients that already parse OpenAI's rejections learn
/// the accepted values from the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedReasoningEffort {
    param: &'static str,
    model: String,
    value: String,
    supported: Vec<&'static str>,
}

impl UnsupportedReasoningEffort {
    /// The OpenAI `error.param` naming the rejected field.
    pub(crate) fn param(&self) -> &'static str {
        self.param
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        param: &'static str,
        model: &str,
        value: &str,
        supported: Vec<&'static str>,
    ) -> Self {
        Self {
            param,
            model: model.to_string(),
            value: value.to_string(),
            supported,
        }
    }
}

impl fmt::Display for UnsupportedReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let supported = self
            .supported
            .iter()
            .map(|value| format!("'{value}'"))
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "Unsupported value: '{}' is not supported with the '{}' model. Supported values are: {}.",
            self.value, self.model, supported
        )
    }
}

/// The client's value, printable and bounded, for the error message only.
fn describe_value(value: &Value) -> String {
    let raw = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    raw.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(MAX_ECHOED_VALUE_CHARS)
        .collect()
}

/// Validate the effort a client named against the resolved model.
///
/// * absent or `null`: the model's default stands;
/// * a model without reasoning controls: forwarded untouched, which is what
///   every deployed build does with the field;
/// * an accepted effort: kept;
/// * anything else: a 400 for an explicit model, the nearest accepted effort
///   for an alias. A value that is not an effort at all is a 400 either way.
pub(crate) fn resolve_reasoning_effort(
    model: &str,
    requested: Option<&Value>,
    selection: ModelSelection,
    param: &'static str,
) -> Result<EffectiveReasoningEffort, ApiError> {
    let Some(requested) = requested.filter(|value| !value.is_null()) else {
        return Ok(EffectiveReasoningEffort::Unchanged);
    };
    let Some(reasoning) = model_reasoning(model) else {
        return Ok(EffectiveReasoningEffort::Unchanged);
    };
    let parsed = requested.as_str().and_then(ReasoningEffort::parse);
    match (parsed, selection) {
        (Some(effort), _) if reasoning.accepts(effort) => Ok(EffectiveReasoningEffort::Set(effort)),
        (Some(effort), ModelSelection::Alias) => {
            let clamped = reasoning.clamp(effort);
            debug!(
                "Moved alias reasoning effort {} to {} for {}",
                effort.as_str(),
                clamped.as_str(),
                model
            );
            Ok(EffectiveReasoningEffort::Set(clamped))
        }
        (Some(_), ModelSelection::Explicit) | (None, _) => Err(
            ApiError::UnsupportedReasoningEffort(UnsupportedReasoningEffort {
                param,
                model: model.to_string(),
                value: describe_value(requested),
                supported: reasoning
                    .accepted_efforts()
                    .into_iter()
                    .map(ReasoningEffort::as_str)
                    .collect(),
            }),
        ),
    }
}

/// The effort for a model-turn request whose model was decided after the
/// request was validated (an Auto alias may resolve to an alternate model).
/// Never fails: values that are not efforts were rejected on arrival.
pub(crate) fn clamp_reasoning_effort(
    model: &str,
    requested: Option<&Value>,
    param: &'static str,
) -> EffectiveReasoningEffort {
    resolve_reasoning_effort(model, requested, ModelSelection::Alias, param)
        .unwrap_or(EffectiveReasoningEffort::Unchanged)
}

/// Rewrite a Chat Completions body for the resolved model:
///
/// * write the effective `reasoning_effort`;
/// * drop template switches that would turn thinking off on a model whose
///   reasoning is mandatory (on the deployed GLM builds they make the
///   reasoning come back as `content`);
/// * send `developer` messages as `system` where the renderer rejects them.
pub(crate) fn apply_model_request_policy(
    body: &mut Map<String, Value>,
    model: &str,
    effort: EffectiveReasoningEffort,
) {
    if let EffectiveReasoningEffort::Set(effort) = effort {
        body.insert(
            CHAT_REASONING_EFFORT_PARAM.to_string(),
            json!(effort.as_str()),
        );
    }
    let config = model_config(model);
    if config
        .reasoning
        .is_some_and(|reasoning| reasoning.mandatory)
    {
        strip_thinking_off_switches(body);
    }
    if config.developer_role_as_system {
        rename_developer_role(body);
    }
}

fn strip_thinking_off_switches(body: &mut Map<String, Value>) {
    let Some(kwargs) = body
        .get_mut("chat_template_kwargs")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for key in THINKING_OFF_SWITCHES {
        if kwargs.get(key).and_then(Value::as_bool) == Some(false) {
            kwargs.remove(key);
            debug!("Dropped chat_template_kwargs.{key}=false: the model's reasoning is mandatory");
        }
    }
    if kwargs.is_empty() {
        body.remove("chat_template_kwargs");
    }
}

fn rename_developer_role(body: &mut Map<String, Value>) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        if message.get("role").and_then(Value::as_str) == Some("developer") {
            message["role"] = json!("system");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{
        DEEPSEEK_V4_1_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID, GLM_5_3_MODEL_ID, KIMI_K3_MODEL_ID,
        QUICK_MODEL_ID,
    };

    const REASONING_MODELS: [&str; 7] = [
        QUICK_MODEL_ID,
        "gpt-oss-safeguard-120b",
        "gemma4-31b",
        KIMI_K3_MODEL_ID,
        GLM_5_3_MODEL_ID,
        GLM_5_3_FLASH_MODEL_ID,
        DEEPSEEK_V4_1_FLASH_MODEL_ID,
    ];

    fn resolve(
        model: &str,
        value: Value,
        selection: ModelSelection,
    ) -> Result<EffectiveReasoningEffort, String> {
        resolve_reasoning_effort(model, Some(&value), selection, CHAT_REASONING_EFFORT_PARAM)
            .map_err(|error| error.to_string())
    }

    #[test]
    fn explicit_models_accept_exactly_the_catalog_efforts_and_reject_the_rest() {
        for model in REASONING_MODELS {
            let reasoning = model_reasoning(model).expect("reasoning model");
            for effort in ReasoningEffort::ALL {
                let outcome = resolve(model, json!(effort.as_str()), ModelSelection::Explicit);
                if reasoning.accepts(effort) {
                    assert_eq!(
                        outcome,
                        Ok(EffectiveReasoningEffort::Set(effort)),
                        "{model} {effort:?}"
                    );
                } else {
                    let message = outcome.expect_err(&format!("{model} must reject {effort:?}"));
                    assert!(message.starts_with(&format!(
                        "Unsupported value: '{}' is not supported with the '{model}' model. Supported values are: ",
                        effort.as_str()
                    )), "{message}");
                }
            }
        }
    }

    #[test]
    fn rejections_list_the_accepted_values_in_openai_wording() {
        let message =
            resolve(GLM_5_3_MODEL_ID, json!("none"), ModelSelection::Explicit).unwrap_err();
        assert_eq!(
            message,
            "Unsupported value: 'none' is not supported with the 'glm-5-3' model. Supported values are: 'low', 'high', 'max'."
        );
        let message =
            resolve(KIMI_K3_MODEL_ID, json!("medium"), ModelSelection::Explicit).unwrap_err();
        assert_eq!(
            message,
            "Unsupported value: 'medium' is not supported with the 'kimi-k3' model. Supported values are: 'none', 'low', 'high', 'max'."
        );
    }

    #[test]
    fn aliases_move_to_the_nearest_accepted_effort() {
        for (model, requested, expected) in [
            (GLM_5_3_MODEL_ID, "medium", "high"),
            (GLM_5_3_MODEL_ID, "minimal", "low"),
            (GLM_5_3_MODEL_ID, "none", "low"),
            (GLM_5_3_MODEL_ID, "xhigh", "max"),
            (GLM_5_3_FLASH_MODEL_ID, "none", "low"),
            (QUICK_MODEL_ID, "none", "low"),
            (QUICK_MODEL_ID, "minimal", "low"),
            (QUICK_MODEL_ID, "xhigh", "high"),
            (QUICK_MODEL_ID, "max", "high"),
            (KIMI_K3_MODEL_ID, "medium", "high"),
            (KIMI_K3_MODEL_ID, "none", "none"),
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, "minimal", "low"),
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, "medium", "high"),
            ("gemma4-31b", "none", "none"),
            ("gemma4-31b", "medium", "medium"),
        ] {
            let expected = ReasoningEffort::parse(expected).unwrap();
            assert_eq!(
                resolve(model, json!(requested), ModelSelection::Alias),
                Ok(EffectiveReasoningEffort::Set(expected)),
                "{model} {requested}"
            );
        }
    }

    #[test]
    fn values_that_are_not_efforts_are_rejected_on_every_selection() {
        for selection in [ModelSelection::Explicit, ModelSelection::Alias] {
            assert!(resolve(GLM_5_3_MODEL_ID, json!("ultra"), selection).is_err());
            assert!(resolve(GLM_5_3_MODEL_ID, json!(50), selection).is_err());
            assert!(resolve(GLM_5_3_MODEL_ID, json!({"effort": "low"}), selection).is_err());
        }
        let message = resolve(
            GLM_5_3_MODEL_ID,
            json!("ultra\n\u{7}<script>"),
            ModelSelection::Explicit,
        )
        .unwrap_err();
        assert!(
            message.starts_with("Unsupported value: 'ultra<script>' is not"),
            "{message}"
        );
    }

    #[test]
    fn absent_null_and_non_reasoning_models_leave_the_body_alone() {
        for selection in [ModelSelection::Explicit, ModelSelection::Alias] {
            assert_eq!(
                resolve_reasoning_effort(
                    GLM_5_3_MODEL_ID,
                    None,
                    selection,
                    CHAT_REASONING_EFFORT_PARAM
                )
                .unwrap(),
                EffectiveReasoningEffort::Unchanged
            );
            assert_eq!(
                resolve(GLM_5_3_MODEL_ID, Value::Null, selection),
                Ok(EffectiveReasoningEffort::Unchanged)
            );
            assert_eq!(
                resolve("llama3-3-70b", json!("max"), selection),
                Ok(EffectiveReasoningEffort::Unchanged)
            );
            assert_eq!(
                resolve("unknown-model", json!("ultra"), selection),
                Ok(EffectiveReasoningEffort::Unchanged)
            );
        }
    }

    #[test]
    fn clamping_never_fails_after_validation() {
        assert_eq!(
            clamp_reasoning_effort(
                GLM_5_3_MODEL_ID,
                Some(&json!("ultra")),
                RESPONSES_REASONING_EFFORT_PARAM
            ),
            EffectiveReasoningEffort::Unchanged
        );
        assert_eq!(
            clamp_reasoning_effort(
                GLM_5_3_MODEL_ID,
                Some(&json!("medium")),
                RESPONSES_REASONING_EFFORT_PARAM
            ),
            EffectiveReasoningEffort::Set(ReasoningEffort::High)
        );
    }

    fn body(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn policy_writes_the_effective_effort() {
        let mut request = body(json!({"model": GLM_5_3_MODEL_ID, "messages": []}));
        apply_model_request_policy(
            &mut request,
            GLM_5_3_MODEL_ID,
            EffectiveReasoningEffort::Set(ReasoningEffort::High),
        );
        assert_eq!(request["reasoning_effort"], "high");

        let mut untouched =
            body(json!({"model": GLM_5_3_MODEL_ID, "messages": [], "reasoning_effort": "low"}));
        apply_model_request_policy(
            &mut untouched,
            GLM_5_3_MODEL_ID,
            EffectiveReasoningEffort::Unchanged,
        );
        assert_eq!(untouched["reasoning_effort"], "low");
    }

    #[test]
    fn mandatory_models_drop_thinking_off_switches_and_keep_the_rest() {
        for model in [GLM_5_3_MODEL_ID, GLM_5_3_FLASH_MODEL_ID, QUICK_MODEL_ID] {
            let mut request = body(json!({
                "model": model,
                "messages": [],
                "chat_template_kwargs": {"enable_thinking": false, "thinking": false, "clear_thinking": true}
            }));
            apply_model_request_policy(&mut request, model, EffectiveReasoningEffort::Unchanged);
            assert_eq!(
                request["chat_template_kwargs"],
                json!({"clear_thinking": true}),
                "{model}"
            );

            let mut only_switch = body(json!({
                "model": model,
                "messages": [],
                "chat_template_kwargs": {"enable_thinking": false}
            }));
            apply_model_request_policy(
                &mut only_switch,
                model,
                EffectiveReasoningEffort::Unchanged,
            );
            assert!(only_switch.get("chat_template_kwargs").is_none(), "{model}");

            let mut enabled = body(json!({
                "model": model,
                "messages": [],
                "chat_template_kwargs": {"enable_thinking": true}
            }));
            apply_model_request_policy(&mut enabled, model, EffectiveReasoningEffort::Unchanged);
            assert_eq!(
                enabled["chat_template_kwargs"]["enable_thinking"], true,
                "{model}"
            );
        }
    }

    #[test]
    fn optional_models_keep_their_thinking_switches() {
        for model in [KIMI_K3_MODEL_ID, DEEPSEEK_V4_1_FLASH_MODEL_ID, "gemma4-31b"] {
            let mut request = body(json!({
                "model": model,
                "messages": [],
                "chat_template_kwargs": {"enable_thinking": false}
            }));
            apply_model_request_policy(&mut request, model, EffectiveReasoningEffort::Unchanged);
            assert_eq!(
                request["chat_template_kwargs"]["enable_thinking"], false,
                "{model}"
            );
        }
    }

    #[test]
    fn kimi_k3_receives_developer_messages_as_system() {
        let messages = json!([
            {"role": "developer", "content": "Be terse."},
            {"role": "user", "content": "hi"}
        ]);
        let mut kimi = body(json!({"model": KIMI_K3_MODEL_ID, "messages": messages}));
        apply_model_request_policy(
            &mut kimi,
            KIMI_K3_MODEL_ID,
            EffectiveReasoningEffort::Unchanged,
        );
        assert_eq!(kimi["messages"][0]["role"], "system");
        assert_eq!(kimi["messages"][0]["content"], "Be terse.");
        assert_eq!(kimi["messages"][1]["role"], "user");

        let mut gpt_oss = body(json!({"model": QUICK_MODEL_ID, "messages": messages}));
        apply_model_request_policy(
            &mut gpt_oss,
            QUICK_MODEL_ID,
            EffectiveReasoningEffort::Unchanged,
        );
        assert_eq!(gpt_oss["messages"][0]["role"], "developer");
    }
}
