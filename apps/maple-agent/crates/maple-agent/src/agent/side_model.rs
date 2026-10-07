//! Shared plumbing for the small side models Maple runs alongside a session.
//!
//! The image describer and the tool-summary helpers reach for the same
//! shape: a fixed model, isolated from session-level reasoning settings,
//! asked one bounded question. This module owns that shape so the callers
//! only describe what they ask.

use goose_providers::model::ModelConfig;
use std::collections::HashMap;

/// Request knobs that disable thinking on OpenAI-compatible endpoints that
/// need it spelled out in the request body rather than the model config.
pub(crate) fn thinking_disabled_request_params() -> HashMap<String, serde_json::Value> {
    HashMap::from([
        ("include_reasoning".to_string(), serde_json::json!(false)),
        (
            "chat_template_kwargs".to_string(),
            serde_json::json!({ "enable_thinking": false }),
        ),
    ])
}

/// Materialize a model config for a side model.
///
/// Side models are intentionally isolated from session-level reasoning and
/// request settings. In particular, Goose may otherwise inherit a global
/// thinking effort while materializing this isolated request.
pub(crate) fn side_model_config(
    provider_name: &str,
    model_name: &str,
    request_params: Option<HashMap<String, serde_json::Value>>,
    temperature: f32,
    max_tokens: i32,
) -> anyhow::Result<ModelConfig> {
    let mut model_config =
        goose::model_config::model_config_from_user_config_with_session_settings(
            provider_name,
            model_name,
            None,
            request_params.clone(),
            None,
        )?;
    model_config.request_params = request_params;
    model_config.reasoning = Some(false);
    Ok(model_config
        .with_temperature(Some(temperature))
        .with_max_tokens(Some(max_tokens)))
}
