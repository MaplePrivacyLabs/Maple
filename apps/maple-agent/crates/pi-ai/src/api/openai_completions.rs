//! Pi v1.0.4 chat-completions request conversion and parsed-chunk state machine.
//!
//! HTTP, SSE framing, provider retries, authentication, and transport error
//! formatting belong to the injected host transport. Pi-authored errors remain exact.
use super::{
    constrained_sampling::*,
    openai_prompt_cache::clamp_open_ai_prompt_cache_key,
    simple_options::*,
    transform_messages::{has_non_whitespace, transform_messages},
};
use crate::{
    env::PiEnv,
    models::{calculate_cost, clamp_thinking_level},
    types::*,
    utils::{
        event_stream::{
            AssistantMessageEventStream, AssistantMessageEventStreamWriter,
            create_assistant_message_event_stream,
        },
        hash::short_hash,
        js_json::stringify,
        js_value::to_js_value,
        provider_env::get_provider_env_value,
        sanitize_unicode::sanitize_surrogates,
        transcript::{
            get_declared_tools, get_message_system_text, render_message_system_update,
            resolve_transcript, resolve_transcript_tools, system_tools_added,
        },
    },
};
use futures_util::{Stream, StreamExt};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, pin::Pin};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICompletionsOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<JsValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
}
impl std::ops::Deref for OpenAICompletionsOptions {
    type Target = StreamOptions;
    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}
impl std::ops::DerefMut for OpenAICompletionsOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stream
    }
}
pub type ResolvedOpenAICompletionsCompat = OpenAICompletionsCompat;
fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsValue {
    JsValue::Object(entries.into_iter().collect())
}
fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
        JsValue::String(value) => !value.is_empty(),
        _ => true,
    }
}
fn string(value: Option<&JsValue>) -> Option<JsString> {
    value.and_then(JsValue::as_js_str).cloned()
}
fn nonempty(value: Option<&JsValue>) -> Option<JsString> {
    string(value).filter(|s| !s.is_empty())
}
fn is_enabled(value: Option<bool>) -> bool {
    value == Some(true)
}
pub fn resolve_cache_retention(
    retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
    environment_key: Option<&str>,
) -> CacheRetention {
    retention.unwrap_or_else(|| {
        if environment_key
            .and_then(|key| get_provider_env_value(key, env))
            .as_deref()
            == Some("long")
        {
            CacheRetention::Long
        } else {
            CacheRetention::Short
        }
    })
}
pub fn detect_compat(model: &Model) -> ResolvedOpenAICompletionsCompat {
    let p = model.provider.as_str();
    let b = model.base_url.as_str();
    let zai = matches!(p, "zai" | "zai-coding-cn")
        || b.contains("api.z.ai")
        || b.contains("open.bigmodel.cn");
    let together =
        p == "together" || b.contains("api.together.ai") || b.contains("api.together.xyz");
    let moonshot = matches!(p, "moonshotai" | "moonshotai-cn") || b.contains("api.moonshot.");
    let openrouter = p == "openrouter" || b.contains("openrouter.ai");
    let workers = p == "cloudflare-workers-ai" || b.contains("api.cloudflare.com");
    let gateway = p == "cloudflare-ai-gateway" || b.contains("gateway.ai.cloudflare.com");
    let nvidia = p == "nvidia" || b.contains("integrate.api.nvidia.com");
    let ant = p == "ant-ling" || b.contains("api.ant-ling.com");
    let cerebras = p == "cerebras" || b.contains("cerebras.ai");
    let deepseek = p == "deepseek" || b.to_lowercase().contains("deepseek.com");
    let grok = p == "xai" || b.contains("api.x.ai");
    let nonstandard = nvidia
        || cerebras
        || grok
        || together
        || b.contains("chutes.ai")
        || deepseek
        || zai
        || moonshot
        || p == "opencode"
        || b.contains("opencode.ai")
        || workers
        || gateway
        || ant;
    let max_tokens = b.contains("chutes.ai")
        || deepseek
        || moonshot
        || gateway
        || together
        || nvidia
        || ant
        || zai;
    OpenAICompletionsCompat {
        supports_store: Some(!nonstandard),
        supports_developer_role: Some(
            (openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/")))
                || (!nonstandard && !openrouter),
        ),
        supports_reasoning_effort: Some(
            !grok && !zai && !moonshot && !together && !gateway && !nvidia && !ant,
        ),
        supports_usage_in_streaming: Some(true),
        supports_finish_reason: Some(true),
        max_tokens_field: Some(if max_tokens {
            MaxTokensField::MaxTokens
        } else {
            MaxTokensField::MaxCompletionTokens
        }),
        requires_tool_result_name: Some(false),
        requires_assistant_after_tool_result: Some(false),
        requires_thinking_as_text: Some(false),
        requires_reasoning_content_on_assistant_messages: Some(deepseek),
        thinking_format: Some(if deepseek {
            ThinkingFormat::Deepseek
        } else if zai {
            ThinkingFormat::Zai
        } else if together {
            ThinkingFormat::Together
        } else if ant {
            ThinkingFormat::AntLing
        } else if openrouter {
            ThinkingFormat::Openrouter
        } else {
            ThinkingFormat::Openai
        }),
        open_router_routing: Some(Default::default()),
        vercel_gateway_routing: Some(Default::default()),
        chat_template_kwargs: Some(Default::default()),
        chat_template_args: Some(Default::default()),
        zai_tool_stream: Some(false),
        supports_thinking_token_budget: Some(false),
        supports_strict_mode: Some(false),
        supports_open_ai_grammar_tools: Some(false),
        supports_mid_convo_system_messages: Some(false),
        supports_mid_convo_tool_additions: Some(false),
        cache_control_format: if p == "openrouter" && model.id.starts_with("anthropic/") {
            Some(CacheControlFormat::Anthropic)
        } else {
            None
        },
        send_session_affinity_headers: Some(openrouter),
        session_affinity_format: Some(if openrouter {
            SessionAffinityFormat::Openrouter
        } else {
            SessionAffinityFormat::Openai
        }),
        supports_long_cache_retention: Some(!(together || workers || gateway || nvidia || ant)),
        ..Default::default()
    }
}
pub fn get_compat(model: &Model) -> ResolvedOpenAICompletionsCompat {
    let mut resolved = detect_compat(model);
    if let Some(compat) = &model.compat {
        macro_rules! field { ($($field:ident),* $(,)?) => { $(if compat.$field.is_some() { resolved.$field = compat.$field.clone(); })* }; }
        field!(
            supports_store,
            supports_developer_role,
            supports_reasoning_effort,
            supports_usage_in_streaming,
            supports_finish_reason,
            max_tokens_field,
            requires_tool_result_name,
            requires_assistant_after_tool_result,
            requires_thinking_as_text,
            requires_reasoning_content_on_assistant_messages,
            thinking_format,
            open_router_routing,
            vercel_gateway_routing,
            chat_template_kwargs,
            chat_template_args,
            zai_tool_stream,
            supports_thinking_token_budget,
            thinking_token_budget_field,
            supports_strict_mode,
            supports_open_ai_grammar_tools,
            supports_mid_convo_system_messages,
            supports_mid_convo_tool_additions,
            cache_control_format,
            send_session_affinity_headers,
            session_affinity_format,
            supports_long_cache_retention,
            vllm_priority
        );
    }
    resolved
}
/// Model headers and computed affinity are offered to the host; request headers win.
pub fn request_headers(
    model: &Model,
    options: &OpenAICompletionsOptions,
    compat: &ResolvedOpenAICompletionsCompat,
    cache_retention: CacheRetention,
) -> ProviderHeaders {
    let mut headers: ProviderHeaders = model
        .headers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|(key, value)| (key, Some(value)))
        .collect();
    if cache_retention != CacheRetention::None
        && is_enabled(compat.send_session_affinity_headers)
        && let Some(id) = options.session_id.as_ref().filter(|s| !s.is_empty())
    {
        match compat.session_affinity_format.unwrap_or_default() {
            SessionAffinityFormat::Openrouter => {
                headers.insert("x-session-id".into(), Some(id.clone()));
            }
            format => {
                if format == SessionAffinityFormat::Openai {
                    headers.insert("session_id".into(), Some(id.clone()));
                }
                headers.insert("x-client-request-id".into(), Some(id.clone()));
                headers.insert("x-session-affinity".into(), Some(id.clone()));
            }
        }
    }
    if let Some(extra) = &options.headers {
        headers.extend(extra.clone());
    }
    headers
}
fn has_tool_history(messages: &[Message]) -> Result<bool, JsString> {
    for message in messages {
        if message.role() == "toolResult" {
            return Ok(true);
        }
        match message {
            Message::Assistant(message)
                if message
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::ToolCall(_))) =>
            {
                return Ok(true);
            }
            Message::Raw(raw) if message.role() == "assistant" => {
                let found = raw.read(|message| {
                    let blocks = crate::utils::raw_message::array(
                        message.get("content"),
                        "msg.content",
                        "some",
                    )?;
                    for block in blocks {
                        if crate::utils::raw_message::property(Some(block), "type")?
                            .and_then(JsValue::as_str)
                            == Some("toolCall")
                        {
                            return Ok(true);
                        }
                    }
                    Ok::<_, JsString>(false)
                })?;
                if found {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}
fn mapped_effort(model: &Model, effort: Option<ThinkingLevel>) -> Option<&Option<String>> {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&effort.map(Into::into).unwrap_or(ModelThinkingLevel::Off)))
}
fn effort_with_fallback(model: &Model, effort: ThinkingLevel) -> JsValue {
    mapped_effort(model, Some(effort))
        .and_then(Option::as_ref)
        .map_or_else(|| effort.as_str().into(), |value| value.clone().into())
}
fn explicit_effort(model: &Model, effort: Option<ThinkingLevel>) -> Option<JsValue> {
    match mapped_effort(model, effort) {
        Some(Some(value)) => Some(value.clone().into()),
        Some(None) => None,
        None => effort.map(|e| e.as_str().into()),
    }
}
fn build_chat_template_values(
    model: &Model,
    options: &OpenAICompletionsOptions,
    values: Option<&IndexMap<String, ChatTemplateKwargValue>>,
    budget: Option<f64>,
) -> Option<JsValue> {
    let mut resolved = JsObject::new();
    for (key, value) in values.into_iter().flatten() {
        let value = match value {
            ChatTemplateKwargValue::Variable(variable) => {
                if options.reasoning_effort.is_none() && variable.omit_when_off == Some(true) {
                    continue;
                }
                match variable.var {
                    ThinkingVariable::Enabled => Some(options.reasoning_effort.is_some().into()),
                    ThinkingVariable::Budget => budget.map(Into::into),
                    ThinkingVariable::Effort => explicit_effort(model, options.reasoning_effort),
                }
            }
            ChatTemplateKwargValue::String(value) => Some(value.clone().into()),
            ChatTemplateKwargValue::Number(value) => Some((*value).into()),
            ChatTemplateKwargValue::Boolean(value) => Some((*value).into()),
            ChatTemplateKwargValue::Null => Some(JsValue::Null),
        };
        if let Some(value) = value {
            resolved.insert(key, value);
        }
    }
    (!resolved.is_empty()).then_some(resolved.into())
}
pub fn build_params(
    model: &Model,
    context: &TranscriptContext,
    options: Option<&OpenAICompletionsOptions>,
    env: &dyn PiEnv,
) -> Result<JsValue, JsString> {
    let default_options = OpenAICompletionsOptions::default();
    let options = options.unwrap_or(&default_options);
    let compat = get_compat(model);
    let context = resolve_transcript(context.clone(), compat.supports_mid_convo_system_messages)?;
    let cache_retention = resolve_cache_retention(
        options.cache_retention,
        options.env.as_ref(),
        options.cache_retention_env.as_deref(),
    );
    let properties = create_grammar_tool_input_properties(
        &get_declared_tools(&context.messages)?,
        is_enabled(compat.supports_open_ai_grammar_tools),
    )?;
    build_params_resolved(
        model,
        &context,
        options,
        &compat,
        cache_retention,
        &properties,
        env,
    )
}
fn build_params_resolved(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICompletionsOptions,
    compat: &ResolvedOpenAICompletionsCompat,
    cache_retention: CacheRetention,
    properties: &IndexMap<JsString, JsString>,
    env: &dyn PiEnv,
) -> Result<JsValue, JsString> {
    let transcript_tools = resolve_transcript_tools(
        &context.messages,
        is_enabled(compat.supports_mid_convo_system_messages)
            && is_enabled(compat.supports_mid_convo_tool_additions),
    )?;
    let mut messages = convert_messages(model, context, compat, Some(properties), env)?;
    let mut params = JsObject::new();
    params.insert("model", model.id.clone().into());
    params.insert("messages", JsValue::Array(Vec::new()));
    params.insert("stream", true.into());
    if ((model.base_url.contains("api.openai.com") && cache_retention != CacheRetention::None)
        || (cache_retention == CacheRetention::Long
            && is_enabled(compat.supports_long_cache_retention)))
        && let Some(key) = options.session_id.as_ref()
    {
        params.insert(
            "prompt_cache_key",
            clamp_open_ai_prompt_cache_key(Some(&key.clone().into()))
                .expect("provided key")
                .into(),
        );
    }
    if cache_retention == CacheRetention::Long && is_enabled(compat.supports_long_cache_retention) {
        params.insert("prompt_cache_retention", "24h".into());
    }
    if compat.supports_usage_in_streaming != Some(false) {
        params.insert("stream_options", object([("include_usage", true.into())]));
    }
    if is_enabled(compat.supports_store) {
        params.insert("store", false.into());
    }
    if let Some(max) = options.max_tokens.filter(|n| *n != 0.0 && !n.is_nan()) {
        params.insert(
            compat.max_tokens_field.unwrap_or_default().as_str(),
            max.into(),
        );
    }
    if let Some(temperature) = options.temperature {
        params.insert("temperature", temperature.into());
    }
    let mut tools = if !transcript_tools.request_tools.is_empty() {
        if is_enabled(compat.zai_tool_stream) { /* inserted after tools below */ }
        Some(convert_tools(&transcript_tools.request_tools, compat)?)
    } else if has_tool_history(&context.messages)? {
        Some(Vec::new())
    } else {
        None
    };
    if compat.cache_control_format == Some(CacheControlFormat::Anthropic)
        && cache_retention != CacheRetention::None
    {
        let mut cache = JsObject::new();
        cache.insert("type", "ephemeral".into());
        if cache_retention == CacheRetention::Long
            && is_enabled(compat.supports_long_cache_retention)
        {
            cache.insert("ttl", "1h".into());
        }
        apply_anthropic_cache_control(&mut messages, tools.as_deref_mut(), &cache.into());
    }
    params.insert("messages", messages.into());
    if let Some(tools) = tools {
        params.insert("tools", tools.into());
        if !transcript_tools.request_tools.is_empty() && is_enabled(compat.zai_tool_stream) {
            params.insert("tool_stream", true.into());
        }
    }
    if let Some(choice) = options.tool_choice.as_ref().filter(|v| truthy(v)) {
        params.insert("tool_choice", choice.clone());
    }
    if let Some(priority) = compat.vllm_priority {
        params.insert("priority", priority.into());
    }
    let budget_field = compat.thinking_token_budget_field.or_else(|| {
        is_enabled(compat.supports_thinking_token_budget)
            .then_some(ThinkingTokenBudgetField::ThinkingTokenBudget)
    });
    let budget = options
        .reasoning_effort
        .filter(|_| model.reasoning)
        .and_then(|effort| {
            let ceiling = params
                .get("max_tokens")
                .or_else(|| params.get("max_completion_tokens"))
                .and_then(JsValue::as_f64)
                .unwrap_or(model.max_tokens);
            let budget = clamp_thinking_budget_to_answer_room(
                thinking_budget_for_level(effort, options.thinking_budgets.as_ref()),
                ceiling,
            );
            (budget > 0.0).then_some(budget)
        });
    let effort = options.reasoning_effort;
    let enabled = effort.is_some();
    if model.reasoning {
        match compat.thinking_format.unwrap_or_default() {
            ThinkingFormat::Zai => {
                params.insert(
                    "thinking",
                    if enabled {
                        object([("type", "enabled".into()), ("clear_thinking", false.into())])
                    } else {
                        object([("type", "disabled".into())])
                    },
                );
                if enabled
                    && is_enabled(compat.supports_reasoning_effort)
                    && let Some(effort) = explicit_effort(model, effort)
                {
                    params.insert("reasoning_effort", effort);
                }
            }
            ThinkingFormat::Qwen => {
                params.insert("enable_thinking", enabled.into());
                if let Some(effort) =
                    effort.filter(|_| is_enabled(compat.supports_reasoning_effort))
                {
                    params.insert("reasoning_effort", effort_with_fallback(model, effort));
                }
            }
            ThinkingFormat::QwenChatTemplate => {
                params.insert(
                    "chat_template_kwargs",
                    object([
                        ("enable_thinking", enabled.into()),
                        ("preserve_thinking", true.into()),
                    ]),
                );
            }
            ThinkingFormat::ChatTemplate => {
                if let Some(values) = build_chat_template_values(
                    model,
                    options,
                    compat.chat_template_kwargs.as_ref(),
                    budget,
                ) {
                    params.insert("chat_template_kwargs", values);
                }
            }
            ThinkingFormat::Baseten => {
                if let Some(values) = build_chat_template_values(
                    model,
                    options,
                    compat.chat_template_args.as_ref(),
                    budget,
                ) {
                    params.insert("chat_template_args", values);
                }
                if is_enabled(compat.supports_reasoning_effort)
                    && let Some(effort) = explicit_effort(model, effort)
                {
                    params.insert("reasoning_effort", effort);
                }
            }
            ThinkingFormat::Deepseek => {
                if enabled {
                    params.insert("thinking", object([("type", "enabled".into())]));
                } else if mapped_effort(model, None) != Some(&None) {
                    params.insert("thinking", object([("type", "disabled".into())]));
                }
                if let Some(effort) =
                    effort.filter(|_| is_enabled(compat.supports_reasoning_effort))
                {
                    params.insert("reasoning_effort", effort_with_fallback(model, effort));
                }
            }
            ThinkingFormat::Openrouter => {
                if let Some(effort) = effort {
                    params.insert(
                        "reasoning",
                        object([("effort", effort_with_fallback(model, effort))]),
                    );
                } else if mapped_effort(model, None) != Some(&None) {
                    params.insert(
                        "reasoning",
                        object([(
                            "effort",
                            mapped_effort(model, None)
                                .and_then(Option::as_ref)
                                .map_or_else(|| "none".into(), |v| v.clone().into()),
                        )]),
                    );
                }
            }
            ThinkingFormat::AntLing if enabled => {
                if let Some(Some(effort)) = mapped_effort(model, effort) {
                    params.insert("reasoning", object([("effort", effort.clone().into())]));
                }
            }
            ThinkingFormat::Together => {
                params.insert("reasoning", object([("enabled", enabled.into())]));
                if let Some(effort) =
                    effort.filter(|_| is_enabled(compat.supports_reasoning_effort))
                {
                    params.insert("reasoning_effort", effort_with_fallback(model, effort));
                }
            }
            ThinkingFormat::StringThinking => {
                if let Some(effort) = effort {
                    params.insert("thinking", effort_with_fallback(model, effort));
                } else if mapped_effort(model, None) != Some(&None) {
                    params.insert(
                        "thinking",
                        mapped_effort(model, None)
                            .and_then(Option::as_ref)
                            .map_or_else(|| "none".into(), |v| v.clone().into()),
                    );
                }
            }
            _ if is_enabled(compat.supports_reasoning_effort) => {
                if let Some(effort) = effort {
                    params.insert("reasoning_effort", effort_with_fallback(model, effort));
                } else if let Some(Some(off)) = mapped_effort(model, None) {
                    params.insert("reasoning_effort", off.clone().into());
                }
            }
            _ => {}
        }
    }
    if let (Some(field), Some(budget)) = (budget_field, budget) {
        params.insert(field.as_str(), budget.into());
    }
    if let Some(compat) = &model.compat {
        if let Some(routing) = &compat.open_router_routing {
            params.insert(
                "provider",
                to_js_value(routing).expect("routing is serializable"),
            );
        }
        if let Some(routing) = &compat.vercel_gateway_routing
            && (routing.only.is_some() || routing.order.is_some())
        {
            params.insert(
                "providerOptions",
                object([(
                    "gateway",
                    to_js_value(routing).expect("routing is serializable"),
                )]),
            );
        }
    }
    if let Some(sampling) = resolve_sampling_params(
        model,
        effort.map(Into::into).unwrap_or(ModelThinkingLevel::Off),
        options.sampling_params.as_ref(),
    ) {
        for (key, value) in sampling {
            params.insert(key, value);
        }
    }
    Ok(params.into())
}
fn add_cache_to_text(message: &mut JsValue, cache: &JsValue) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    if let Some(text) = content.as_js_str() {
        if text.is_empty() {
            return false;
        }
        *content = vec![object([
            ("type", "text".into()),
            ("text", text.clone().into()),
            ("cache_control", cache.clone()),
        ])]
        .into();
        return true;
    }
    if let Some(parts) = content.as_array_mut() {
        for part in parts.iter_mut().rev() {
            if part.get("type").and_then(JsValue::as_str) == Some("text") {
                part.as_object_mut()
                    .expect("text block is object")
                    .insert("cache_control", cache.clone());
                return true;
            }
        }
    }
    false
}
fn apply_anthropic_cache_control(
    messages: &mut [JsValue],
    tools: Option<&mut [JsValue]>,
    cache: &JsValue,
) {
    if let Some(instruction) = messages.iter_mut().find(|m| {
        matches!(
            m.get("role").and_then(JsValue::as_str),
            Some("system" | "developer")
        )
    }) {
        add_cache_to_text(instruction, cache);
    }
    if let Some(tool) = tools.and_then(|tools| tools.last_mut()) {
        tool.as_object_mut()
            .expect("tool is object")
            .insert("cache_control", cache.clone());
    }
    for message in messages.iter_mut().rev() {
        if matches!(
            message.get("role").and_then(JsValue::as_str),
            Some("user" | "assistant" | "tool")
        ) && add_cache_to_text(message, cache)
        {
            break;
        }
    }
}
pub fn convert_tools(
    tools: &[Tool],
    compat: &ResolvedOpenAICompletionsCompat,
) -> Result<Vec<JsValue>, JsString> {
    tools
        .iter()
        .map(|tool| {
            if let Some(grammar) = resolve_grammar_constrained_sampling(
                tool,
                is_enabled(compat.supports_open_ai_grammar_tools),
            )? {
                return Ok(object([
                    ("type", "custom".into()),
                    (
                        "custom",
                        object([
                            ("name", tool.name.clone().into()),
                            ("description", tool.description.clone().into()),
                            (
                                "format",
                                object([
                                    ("type", "grammar".into()),
                                    (
                                        "grammar",
                                        object([
                                            ("syntax", grammar.format.into()),
                                            ("definition", grammar.definition.into()),
                                        ]),
                                    ),
                                ]),
                            ),
                        ]),
                    ),
                ]));
            }
            let strict = resolve_json_schema_strict_sampling(
                tool,
                compat.supports_strict_mode != Some(false),
                None,
            )?;
            let mut function = JsObject::new();
            function.insert("name", tool.name.clone().into());
            function.insert("description", tool.description.clone().into());
            function.insert("parameters", get_json_schema_tool_parameters(tool, strict)?);
            if compat.supports_strict_mode != Some(false) {
                function.insert("strict", strict.unwrap_or(false).into());
            }
            Ok(object([
                ("type", "function".into()),
                ("function", function.into()),
            ]))
        })
        .collect()
}
fn valid_reasoning_detail(detail: &JsValue) -> bool {
    if !detail.is_object() {
        return false;
    }
    if detail
        .get("id")
        .is_some_and(|v| !v.is_null() && !v.is_string())
        || detail.get("format").is_some_and(|v| !v.is_string())
        || detail.get("index").is_some_and(|v| !v.is_number())
    {
        return false;
    }
    match detail.get("type").and_then(JsValue::as_str) {
        Some("reasoning.summary") => detail.get("summary").is_some_and(JsValue::is_string),
        Some("reasoning.encrypted") => detail.get("data").is_some_and(JsValue::is_string),
        Some("reasoning.text") => {
            detail.get("text").is_some_and(JsValue::is_string)
                && detail
                    .get("signature")
                    .is_none_or(|v| v.is_null() || v.is_string())
        }
        _ => false,
    }
}
fn parse_reasoning_details(signature: Option<&JsString>) -> Option<Vec<JsValue>> {
    let parsed = crate::utils::json_parse::parse_json_utf16(signature?).ok()?;
    let parsed = parsed.as_array()?;
    (!parsed.is_empty() && parsed.iter().all(valid_reasoning_detail)).then(|| parsed.clone())
}
fn parse_legacy_reasoning_detail(signature: Option<&JsString>) -> Option<JsValue> {
    let parsed = crate::utils::json_parse::parse_json_utf16(signature?).ok()?;
    (valid_reasoning_detail(&parsed)
        && parsed.get("type").and_then(JsValue::as_str) == Some("reasoning.encrypted")
        && nonempty(parsed.get("id")).is_some()
        && nonempty(parsed.get("data")).is_some())
    .then_some(parsed)
}
fn append_reasoning_detail(details: &mut Vec<JsValue>, detail: &JsValue) {
    let kind = detail.get("type").and_then(JsValue::as_str);
    let merge_field = match kind {
        Some("reasoning.text") => Some("text"),
        Some("reasoning.summary") => Some("summary"),
        _ => None,
    };
    if let (Some(last), Some(field)) = (details.last_mut(), merge_field)
        && last.get("type") == detail.get("type")
    {
        let mut text = string(last.get(field)).expect("validated detail");
        text.push(&string(detail.get(field)).expect("validated detail"));
        let last = last.as_object_mut().expect("validated detail");
        last.insert(field, text.into());
        if field == "text"
            && last.get("signature").is_none_or(|v| !truthy(v))
            && let Some(signature) = detail.get("signature")
        {
            last.insert("signature", signature.clone());
        }
        for key in ["id", "format", "index"] {
            let missing = last.get(key).is_none_or(|v| {
                if key == "format" {
                    !truthy(v)
                } else {
                    v.is_null()
                }
            });
            if missing && let Some(value) = detail.get(key) {
                last.insert(key, value.clone());
            }
        }
        return;
    }
    details.push(detail.clone());
}
fn normalize_tool_call_id(id: &JsString, model: &Model) -> JsString {
    let units = id.as_utf16();
    if let Some(separator) = units.iter().position(|u| *u == u16::from(b'|')) {
        let sanitize = |units: &[u16]| -> String {
            units
                .iter()
                .map(|u| {
                    if *u <= 127
                        && (u8::try_from(*u).expect("ascii").is_ascii_alphanumeric()
                            || matches!(*u, 45 | 95))
                    {
                        char::from_u32(u32::from(*u)).expect("ascii")
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        let call = sanitize(&units[..separator]);
        let item = sanitize(&units[separator + 1..]);
        let combined = if item.is_empty() {
            call.clone()
        } else {
            format!("{call}_{item}")
        };
        if combined.len() <= 40 {
            return combined.into();
        }
        let hash = short_hash(id);
        let hash = &hash[..hash.len().min(8)];
        return format!(
            "{}_{}",
            &call[..call.len().min((40 - hash.len() - 1).max(1))],
            hash
        )
        .into();
    }
    if model.provider == "openai" && units.len() > 40 {
        return id.slice(0, 40);
    }
    id.clone()
}
fn text_part(text: impl Into<JsValue>) -> JsValue {
    object([("type", "text".into()), ("text", text.into())])
}
fn message(role: &str, content: JsValue) -> JsValue {
    object([("role", role.into()), ("content", content)])
}
fn image_part(image: &ImageContent) -> JsValue {
    object([
        ("type", "image_url".into()),
        (
            "image_url",
            object([(
                "url",
                format!("data:{};base64,{}", image.mime_type, image.data).into(),
            )]),
        ),
    ])
}
fn project_wire_message(raw: RawMessage) -> Result<Message, JsString> {
    let mut object = raw.snapshot();
    let role = object
        .get("role")
        .and_then(JsValue::as_str)
        .unwrap_or("")
        .to_owned();
    if role == "system" {
        return Ok(Message::Raw(raw));
    }
    // Historical metadata is not part of the wire conversion. Missing fields
    // below receive projection-only placeholders; the raw history is untouched.
    if !object.contains_key("timestamp") {
        object.insert("timestamp", 0.0.into());
    }
    if role == "assistant" {
        for field in ["api", "provider", "model"] {
            if !object.contains_key(field) {
                object.insert(field, "".into());
            }
        }
        object.insert(
            "usage",
            to_js_value(&Usage::default()).map_err(|error| JsString::from(error.to_string()))?,
        );
        object.insert("stopReason", "stop".into());
        if let Some(JsValue::Array(blocks)) = object.get_mut("content") {
            blocks.retain(|block| {
                matches!(
                    block.get("type").and_then(JsValue::as_str),
                    Some("text" | "thinking" | "toolCall")
                )
            });
        }
    } else if role == "toolResult" {
        let blocks = object
            .get_mut("content")
            .and_then(JsValue::as_array_mut)
            .ok_or_else(|| JsString::from("toolMsg.content.filter is not a function"))?;
        // Source filters text and image blocks; primitive entries produced by
        // string iteration contribute neither and still yield a tool message.
        let mut projected = Vec::new();
        for block in blocks.iter() {
            let kind = crate::utils::raw_message::property(Some(block), "type")?;
            if matches!(kind.and_then(JsValue::as_str), Some("text" | "image")) {
                projected.push(block.clone());
            }
        }
        *blocks = projected;
        if !object.contains_key("isError") {
            object.insert("isError", false.into());
        }
        if !object.contains_key("toolName") {
            object.insert("toolName", "".into());
        }
    }
    crate::utils::js_value::from_js_value(JsValue::Object(object))
        .map_err(|error| JsString::from(error.to_string()))
}
pub fn convert_messages(
    model: &Model,
    context: &TranscriptContext,
    compat: &ResolvedOpenAICompletionsCompat,
    properties: Option<&IndexMap<JsString, JsString>>,
    env: &dyn PiEnv,
) -> Result<Vec<JsValue>, JsString> {
    let context = resolve_transcript(context.clone(), compat.supports_mid_convo_system_messages)?;
    let transformed = transform_messages(
        &context.messages,
        model,
        Some(&|id, model, _| normalize_tool_call_id(id, model)),
        env,
    )?;
    // The raw transform retains historical metadata. Wire conversion reads only
    // the provider fields, so project repaired known-role objects at this point.
    let transformed = transformed
        .into_iter()
        .map(|message| match message {
            Message::Raw(raw) => project_wire_message(raw),
            message => Ok(message),
        })
        .collect::<Result<Vec<Message>, JsString>>()?;
    let transcript_tools = resolve_transcript_tools(
        &context.messages,
        is_enabled(compat.supports_mid_convo_system_messages)
            && is_enabled(compat.supports_mid_convo_tool_additions),
    )?;
    let role = if model.reasoning && is_enabled(compat.supports_developer_role) {
        "developer"
    } else {
        "system"
    };
    let mut params = Vec::new();
    let mut last_role = None;
    let mut i = 0;
    while i < transformed.len() {
        let msg = &transformed[i];
        if msg.role() == "system" {
            let added_tools = system_tools_added(msg)?;
            if i > 0 && transcript_tools.anchors_additions && !added_tools.is_empty() {
                params.push(object([
                    ("role", "system".into()),
                    ("tools", convert_tools(&added_tools, compat)?.into()),
                ]));
            }
            let text = if i == 0 {
                get_message_system_text(msg)?
            } else {
                render_message_system_update(msg)?
            };
            if !text.is_empty() {
                params.push(message(role, sanitize_surrogates(text).into()));
            }
            last_role = Some("system");
            i += 1;
            continue;
        }
        if is_enabled(compat.requires_assistant_after_tool_result)
            && last_role == Some("toolResult")
            && matches!(msg, Message::User(_))
        {
            params.push(message(
                "assistant",
                "I have processed the tool results.".into(),
            ));
        }
        match msg {
            Message::Raw(_) | Message::System(_) => {}
            Message::User(user) => match &user.content {
                UserMessageContent::Text(text) => {
                    params.push(message("user", sanitize_surrogates(text).into()))
                }
                UserMessageContent::Blocks(blocks) => {
                    let parts = blocks
                        .iter()
                        .filter_map(|b| match b {
                            UserContent::Text(text) if text.text.is_empty() => None,
                            UserContent::Text(text) => {
                                Some(text_part(sanitize_surrogates(&text.text)))
                            }
                            UserContent::Image(image) => Some(image_part(image)),
                        })
                        .collect::<Vec<_>>();
                    if parts.is_empty() {
                        i += 1;
                        continue;
                    }
                    params.push(message("user", parts.into()));
                }
            },
            Message::Assistant(assistant) => {
                let mut output = JsObject::new();
                output.insert("role", "assistant".into());
                output.insert(
                    "content",
                    if is_enabled(compat.requires_assistant_after_tool_result) {
                        "".into()
                    } else {
                        JsValue::Null
                    },
                );
                let texts = assistant
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let AssistantContent::Text(text) = b {
                            has_non_whitespace(&text.text).then(|| sanitize_surrogates(&text.text))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                let text = texts.join("");
                let thinking = assistant
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let AssistantContent::Thinking(thinking) = b {
                            Some(thinking)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                let calls = assistant
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let AssistantContent::ToolCall(call) = b {
                            Some(call)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                let signed = thinking
                    .iter()
                    .find_map(|b| parse_reasoning_details(b.thinking_signature.as_ref()));
                let legacy = calls
                    .iter()
                    .filter_map(|call| {
                        parse_legacy_reasoning_detail(call.thought_signature.as_ref())
                    })
                    .collect::<Vec<_>>();
                let preserved = signed.or_else(|| (!legacy.is_empty()).then_some(legacy));
                let thinking = thinking
                    .into_iter()
                    .filter(|b| has_non_whitespace(&b.thinking))
                    .collect::<Vec<_>>();
                if !thinking.is_empty() {
                    if is_enabled(compat.requires_thinking_as_text) {
                        let mut parts = vec![text_part(
                            thinking
                                .iter()
                                .map(|b| sanitize_surrogates(&b.thinking))
                                .collect::<Vec<_>>()
                                .join("\n\n"),
                        )];
                        parts.extend(texts.iter().cloned().map(text_part));
                        output.insert("content", parts.into());
                    } else {
                        if !text.is_empty() {
                            output.insert("content", text.into());
                        }
                        if preserved.is_none() {
                            let signature = thinking[0]
                                .thinking_signature
                                .as_ref()
                                .and_then(JsString::as_str)
                                .map(|s| {
                                    if model.provider == "opencode-go" && s == "reasoning" {
                                        "reasoning_content"
                                    } else {
                                        s
                                    }
                                });
                            if let Some(
                                signature @ ("reasoning" | "reasoning_content" | "reasoning_text"),
                            ) = signature
                            {
                                output.insert(
                                    signature,
                                    JsString::join(thinking.iter().map(|b| &b.thinking), "\n")
                                        .into(),
                                );
                            }
                        }
                    }
                } else if !text.is_empty() {
                    output.insert("content", text.into());
                }
                if !calls.is_empty() {
                    let converted = calls
                        .into_iter()
                        .map(|call| {
                            if let Some(property) = properties.and_then(|p| p.get(&call.name)) {
                                Ok(object([
                                    ("id", call.id.clone().into()),
                                    ("type", "custom".into()),
                                    (
                                        "custom",
                                        object([
                                            ("name", call.name.clone().into()),
                                            (
                                                "input",
                                                sanitize_surrogates(get_grammar_tool_input(
                                                    &call.name,
                                                    &call.arguments.snapshot(),
                                                    property,
                                                )?)
                                                .into(),
                                            ),
                                        ]),
                                    ),
                                ]))
                            } else {
                                Ok(object([
                                    ("id", call.id.clone().into()),
                                    ("type", "function".into()),
                                    (
                                        "function",
                                        object([
                                            ("name", call.name.clone().into()),
                                            (
                                                "arguments",
                                                stringify(&call.arguments.snapshot()).into(),
                                            ),
                                        ]),
                                    ),
                                ]))
                            }
                        })
                        .collect::<Result<Vec<_>, JsString>>()?;
                    output.insert("tool_calls", converted.into());
                }
                if let Some(details) = preserved {
                    output.insert("reasoning_details", details.into());
                }
                if is_enabled(compat.requires_reasoning_content_on_assistant_messages)
                    && model.reasoning
                    && !output.contains_key("reasoning_content")
                {
                    output.insert("reasoning_content", "".into());
                }
                let has_content = output.get("content").is_some_and(|v| {
                    v.as_js_str().is_some_and(|s| !s.is_empty())
                        || v.as_array().is_some_and(|a| !a.is_empty())
                });
                if !has_content && !output.contains_key("tool_calls") {
                    i += 1;
                    continue;
                }
                params.push(output.into());
            }
            Message::ToolResult(_) => {
                let mut images = Vec::new();
                while let Some(Message::ToolResult(tool)) = transformed.get(i) {
                    let text = JsString::join(
                        tool.content.iter().filter_map(|b| {
                            if let UserContent::Text(t) = b {
                                Some(&t.text)
                            } else {
                                None
                            }
                        }),
                        "\n",
                    );
                    let has_images = tool
                        .content
                        .iter()
                        .any(|b| matches!(b, UserContent::Image(_)));
                    let text = if !text.is_empty() {
                        text
                    } else if has_images {
                        "(see attached image)".into()
                    } else {
                        "(no tool output)".into()
                    };
                    let mut output = JsObject::new();
                    output.insert("role", "tool".into());
                    output.insert("content", sanitize_surrogates(text).into());
                    output.insert("tool_call_id", tool.tool_call_id.clone().into());
                    if is_enabled(compat.requires_tool_result_name) && !tool.tool_name.is_empty() {
                        output.insert("name", tool.tool_name.clone().into());
                    }
                    params.push(output.into());
                    if has_images && model.input.contains(&InputModality::Image) {
                        images.extend(tool.content.iter().filter_map(|b| {
                            if let UserContent::Image(image) = b {
                                Some(image_part(image))
                            } else {
                                None
                            }
                        }));
                    }
                    i += 1;
                }
                if !images.is_empty() {
                    if is_enabled(compat.requires_assistant_after_tool_result) {
                        params.push(message(
                            "assistant",
                            "I have processed the tool results.".into(),
                        ));
                    }
                    let mut parts = vec![text_part("Attached image(s) from tool result:")];
                    parts.extend(images);
                    params.push(message("user", parts.into()));
                    last_role = Some("user");
                } else {
                    last_role = Some("toolResult");
                }
                continue;
            }
        }
        last_role = Some(msg.role());
        i += 1;
    }
    Ok(params)
}
/// Parsed frames: the adapter consumes SSE framing and `[DONE]`; transport error
/// frames become `Err` with the adapter's already formatted display text.
pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<JsValue, JsString>> + Send>>;
#[derive(Clone)]
pub struct CompletionsRequest {
    pub model: Model,
    pub params: JsValue,
    pub headers: ProviderHeaders,
    pub signal: Option<crate::env::CancellationToken>,
    pub timeout_ms: Option<f64>,
    pub max_retries: Option<f64>,
    pub max_retry_delay_ms: Option<f64>,
}
pub struct CompletionsResponse {
    pub response: ProviderResponse,
    pub chunks: ChunkStream,
}
pub trait CompletionsTransport: Send + Sync {
    fn send(&self, request: CompletionsRequest)
    -> BoxFuture<Result<CompletionsResponse, JsString>>;
}
/// An ordered parsed-chunk transport for port tests and corpus replay.
#[derive(Clone, Default)]
pub struct ScriptedTransport {
    requests: Arc<Mutex<Vec<CompletionsRequest>>>,
    responses: Arc<Mutex<VecDeque<Result<CompletionsResponse, JsString>>>>,
}
impl ScriptedTransport {
    pub fn push_response(
        &self,
        response: ProviderResponse,
        chunks: impl IntoIterator<Item = Result<JsValue, JsString>>,
    ) {
        self.responses
            .lock()
            .expect("transport queue lock")
            .push_back(Ok(CompletionsResponse {
                response,
                chunks: Box::pin(futures_util::stream::iter(
                    chunks.into_iter().collect::<Vec<_>>(),
                )),
            }));
    }
    /// Supply an explicitly gated parsed stream for wire-corpus replay.
    pub fn push_stream_response(&self, response: ProviderResponse, chunks: ChunkStream) {
        self.responses
            .lock()
            .expect("transport queue lock")
            .push_back(Ok(CompletionsResponse { response, chunks }));
    }
    pub fn push_error(&self, error: impl Into<JsString>) {
        self.responses
            .lock()
            .expect("transport queue lock")
            .push_back(Err(error.into()));
    }
    pub fn requests(&self) -> Vec<CompletionsRequest> {
        self.requests
            .lock()
            .expect("transport requests lock")
            .clone()
    }
}
impl CompletionsTransport for ScriptedTransport {
    fn send(
        &self,
        request: CompletionsRequest,
    ) -> BoxFuture<Result<CompletionsResponse, JsString>> {
        self.requests
            .lock()
            .expect("transport requests lock")
            .push(request);
        let response = self
            .responses
            .lock()
            .expect("transport queue lock")
            .pop_front()
            .unwrap_or_else(|| Err("No scripted completion response".into()));
        Box::pin(async move { response })
    }
}
#[derive(Default)]
struct ToolScratch {
    partial_args: Option<JsString>,
    custom_input: Option<(JsString, GrammarToolInputJsonBuffer)>,
    stream_index: Option<f64>,
}
struct ChunkParser {
    model: Model,
    output: AssistantMessage,
    partial: SharedAssistantMessage,
    events: AssistantMessageEventStreamWriter,
    properties: IndexMap<JsString, JsString>,
    thinking_details: Option<Vec<JsValue>>,
    text: Option<usize>,
    thinking: Option<usize>,
    indices: HashMap<u64, usize>,
    ids: HashMap<JsString, usize>,
    scratch: IndexMap<usize, ToolScratch>,
    block_count: usize,
    has_finish_reason: bool,
}
impl ChunkParser {
    fn new(
        model: Model,
        timestamp: f64,
        events: AssistantMessageEventStreamWriter,
        properties: IndexMap<JsString, JsString>,
    ) -> Self {
        let output = AssistantMessage::new(&model, timestamp);
        Self {
            model,
            partial: output.clone().into(),
            output,
            events,
            properties,
            thinking_details: None,
            text: None,
            thinking: None,
            indices: HashMap::new(),
            ids: HashMap::new(),
            scratch: IndexMap::new(),
            block_count: 0,
            has_finish_reason: false,
        }
    }
    fn sync_metadata(&self) {
        self.partial.replace_metadata(self.output.clone());
    }
    fn ensure_text(&mut self) -> usize {
        if let Some(index) = self.text {
            return index;
        }
        let index = self.block_count;
        self.block_count += 1;
        self.partial.push_content(TextContent::default().into());
        self.text = Some(index);
        self.events.push(AssistantMessageEvent::TextStart {
            content_index: index,
            partial: self.partial.clone(),
        });
        index
    }
    fn ensure_thinking(&mut self, signature: JsString) -> usize {
        if let Some(index) = self.thinking {
            return index;
        }
        let index = self.block_count;
        self.block_count += 1;
        self.partial.push_content(
            ThinkingContent {
                thinking_signature: Some(signature),
                ..Default::default()
            }
            .into(),
        );
        self.thinking = Some(index);
        self.events.push(AssistantMessageEvent::ThinkingStart {
            content_index: index,
            partial: self.partial.clone(),
        });
        index
    }
    fn sync_scratch(&self, index: usize) {
        let scratch = &self.scratch[&index];
        self.partial.update_block(index, |block| {
            if let AssistantContent::ToolCall(call) = block {
                if let Some(arguments) = &scratch.partial_args {
                    call.extra.insert("partialArgs", arguments.clone().into());
                } else {
                    call.extra.remove("partialArgs");
                }
                if let Some((property, buffer)) = &scratch.custom_input {
                    call.extra.insert(
                        "customInput",
                        object([
                            ("property", property.clone().into()),
                            (
                                "jsonBuffer",
                                object([
                                    ("input", buffer.input.clone().into()),
                                    ("started", buffer.started.into()),
                                    ("closed", buffer.closed.into()),
                                ]),
                            ),
                        ]),
                    );
                } else {
                    call.extra.remove("customInput");
                }
                if let Some(index) = scratch.stream_index {
                    call.extra.insert("streamIndex", index.into());
                }
            }
        });
    }
    fn clear_scratch(&self, index: usize, error: bool) {
        self.partial.update_block(index, |block| {
            if let AssistantContent::ToolCall(call) = block {
                for key in ["partialArgs", "customInput", "streamIndex"] {
                    call.extra.remove(key);
                }
                if error {
                    call.extra.remove("index");
                }
            }
        });
    }
    fn ensure_tool(&mut self, delta: &JsValue) -> usize {
        let stream_index = delta.get("index").and_then(JsValue::as_f64);
        // JavaScript Map uses SameValueZero for numeric keys.
        let key = stream_index.map(|v| {
            if v == 0.0 {
                0
            } else if v.is_nan() {
                f64::NAN.to_bits()
            } else {
                v.to_bits()
            }
        });
        let id = nonempty(delta.get("id"));
        let name = delta
            .get("function")
            .and_then(|f| string(f.get("name")))
            .or_else(|| delta.get("custom").and_then(|f| string(f.get("name"))))
            .unwrap_or_default();
        let custom =
            delta.get("custom").is_some_and(truthy) && !delta.get("function").is_some_and(truthy);
        let found = key
            .and_then(|key| self.indices.get(&key).copied())
            .or_else(|| id.as_ref().and_then(|id| self.ids.get(id).copied()));
        let index = if let Some(index) = found {
            index
        } else {
            let property = custom.then(|| {
                self.properties
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| "input".into())
            });
            let arguments: JsValue = property.as_ref().map_or_else(
                || JsObject::new().into(),
                |property| JsObject::from_iter([(property.clone(), "".into())]).into(),
            );
            let index = self.block_count;
            self.block_count += 1;
            self.partial.push_content(
                ToolCall::new(id.clone().unwrap_or_default(), name.clone(), arguments).into(),
            );
            self.scratch.insert(
                index,
                ToolScratch {
                    partial_args: property.is_none().then(JsString::default),
                    custom_input: property.map(|p| (p, GrammarToolInputJsonBuffer::default())),
                    stream_index,
                },
            );
            if let Some(key) = key {
                self.indices.insert(key, index);
            }
            if let Some(id) = &id {
                self.ids.insert(id.clone(), index);
            }
            self.sync_scratch(index);
            self.events.push(AssistantMessageEvent::ToolcallStart {
                content_index: index,
                partial: self.partial.clone(),
            });
            index
        };
        let scratch = self.scratch.get_mut(&index).expect("tool scratch exists");
        if let Some(stream_index) = stream_index.filter(|_| scratch.stream_index.is_none()) {
            scratch.stream_index = Some(stream_index);
            self.indices.insert(key.expect("numeric index"), index);
        }
        if let Some(id) = id {
            self.ids.insert(id, index);
        }
        let mut actual_name = name.clone();
        self.partial.update_block(index, |block| {
            if let AssistantContent::ToolCall(call) = block {
                if call.name.is_empty() && !name.is_empty() {
                    call.name = name;
                }
                actual_name = call.name.clone();
            }
        });
        if custom && scratch.custom_input.is_none() {
            let property = self
                .properties
                .get(&actual_name)
                .cloned()
                .unwrap_or_else(|| "input".into());
            self.partial.update_block(index, |block| {
                if let AssistantContent::ToolCall(call) = block {
                    call.arguments = JsObject::from_iter([(property.clone(), "".into())]).into();
                }
            });
            scratch.custom_input = Some((property, GrammarToolInputJsonBuffer::default()));
            scratch.partial_args = None;
        }
        index
    }
    fn append_custom(
        &mut self,
        index: usize,
        next: &JsString,
        close: bool,
    ) -> Result<Option<JsString>, JsString> {
        let Some((property, buffer)) = self
            .scratch
            .get_mut(&index)
            .and_then(|s| s.custom_input.as_mut())
        else {
            return Ok(None);
        };
        let delta = append_grammar_tool_input_json_delta(buffer, property, next, close)?;
        self.partial.update_block(index, |block| {
            if let AssistantContent::ToolCall(call) = block {
                call.arguments =
                    JsObject::from_iter([(property.clone(), next.clone().into())]).into();
            }
        });
        self.sync_scratch(index);
        Ok(delta)
    }
    fn custom_input(&self, index: usize) -> JsString {
        let Some((property, _)) = self
            .scratch
            .get(&index)
            .and_then(|s| s.custom_input.as_ref())
        else {
            return JsString::default();
        };
        match self
            .partial
            .content_block(index)
            .expect("tool exists")
            .snapshot()
        {
            AssistantContent::ToolCall(call) => call
                .arguments
                .read(|value| string(value.get(property)))
                .unwrap_or_default(),
            _ => unreachable!(),
        }
    }
    fn apply_reasoning_details(&self) {
        if let (Some(index), Some(details)) = (self.thinking, &self.thinking_details) {
            let signature = stringify(&JsValue::Array(details.clone()));
            self.partial.update_block(index, |block| {
                if let AssistantContent::Thinking(block) = block {
                    block.thinking_signature = Some(signature.into());
                }
            });
        }
    }
    fn accept(&mut self, chunk: &JsValue) -> Result<(), JsString> {
        if !chunk.is_object() && !chunk.is_array() {
            return Ok(());
        }
        if self
            .output
            .response_id
            .as_ref()
            .is_none_or(JsString::is_empty)
        {
            self.output.response_id = string(chunk.get("id"));
        }
        if let Some(model) =
            nonempty(chunk.get("model")).filter(|m| m != &JsString::from(&self.model.id))
            && self
                .output
                .response_model
                .as_ref()
                .is_none_or(JsString::is_empty)
        {
            self.output.response_model = Some(model);
        }
        if let Some(usage) = chunk.get("usage").filter(|v| truthy(v)) {
            self.output.usage = parse_chunk_usage(usage, &self.model);
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(JsValue::as_array)
            .and_then(|a| a.first())
            .filter(|v| truthy(v))
        else {
            self.sync_metadata();
            return Ok(());
        };
        if !chunk.get("usage").is_some_and(truthy)
            && let Some(usage) = choice.get("usage").filter(|v| truthy(v))
        {
            self.output.usage = parse_chunk_usage(usage, &self.model);
        }
        if let Some(reason) = nonempty(choice.get("finish_reason")) {
            let (stop, error) = map_stop_reason(&reason);
            self.output.raw_stop_reason = Some(reason);
            self.output.stop_reason = stop;
            if error.is_some() {
                self.output.error_message = error;
            }
            self.has_finish_reason = true;
        }
        self.sync_metadata();
        if let Some(delta) = choice.get("delta").filter(|v| truthy(v)) {
            if let Some(text) = nonempty(delta.get("content")) {
                let index = self.ensure_text();
                self.partial.update_block(index, |block| {
                    if let AssistantContent::Text(block) = block {
                        block.text.push(&text);
                    }
                });
                self.events.push(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: text,
                    partial: self.partial.clone(),
                });
            }
            for field in ["reasoning_content", "reasoning", "reasoning_text"] {
                if let Some(text) = nonempty(delta.get(field)) {
                    let signature = if self.model.provider == "opencode-go" && field == "reasoning"
                    {
                        "reasoning_content"
                    } else {
                        field
                    };
                    let index = self.ensure_thinking(signature.into());
                    self.partial.update_block(index, |block| {
                        if let AssistantContent::Thinking(block) = block {
                            block.thinking.push(&text);
                        }
                    });
                    self.events.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: text,
                        partial: self.partial.clone(),
                    });
                    break;
                }
            }
            if let Some(calls) = delta.get("tool_calls").filter(|value| truthy(value)) {
                let calls: Cow<'_, [JsValue]> = match calls {
                    JsValue::Array(calls) => Cow::Borrowed(calls),
                    JsValue::String(text) => {
                        // JavaScript's string iterator yields Unicode code points, retaining
                        // lone surrogates as individual values.
                        let mut units = text.units().peekable();
                        let mut calls = Vec::new();
                        while let Some(first) = units.next() {
                            let mut point = vec![first];
                            if (0xd800..=0xdbff).contains(&first)
                                && units
                                    .peek()
                                    .is_some_and(|next| (0xdc00..=0xdfff).contains(next))
                            {
                                point.push(units.next().expect("peeked low surrogate"));
                            }
                            calls.push(JsValue::String(JsString::from_utf16(point)));
                        }
                        Cow::Owned(calls)
                    }
                    _ => return Err("choice.delta.tool_calls is not iterable".into()),
                };
                for call in calls.iter() {
                    if call.is_null() {
                        return Err("Cannot read properties of null (reading 'index')".into());
                    }
                    let index = self.ensure_tool(call);
                    if let Some(id) = nonempty(call.get("id")) {
                        self.partial.update_block(index, |block| {
                            if let AssistantContent::ToolCall(call) = block
                                && call.id.is_empty()
                            {
                                call.id = id.clone();
                            }
                        });
                        self.ids.insert(id, index);
                    }
                    let name = call
                        .get("function")
                        .and_then(|f| string(f.get("name")))
                        .or_else(|| call.get("custom").and_then(|f| string(f.get("name"))))
                        .filter(|name| !name.is_empty());
                    if let Some(name) = name {
                        self.partial.update_block(index, |block| {
                            if let AssistantContent::ToolCall(call) = block
                                && call.name.is_empty()
                            {
                                call.name = name;
                            }
                        });
                    }
                    let mut delta = JsString::default();
                    if let Some(arguments) = call
                        .get("function")
                        .and_then(|f| nonempty(f.get("arguments")))
                    {
                        delta = arguments.clone();
                        let scratch = self.scratch.get_mut(&index).expect("tool scratch");
                        let partial = scratch.partial_args.get_or_insert_default();
                        partial.push(&arguments);
                        let parsed =
                            crate::utils::json_parse::parse_streaming_json_utf16(Some(partial));
                        self.partial.update_block(index, |block| {
                            if let AssistantContent::ToolCall(call) = block {
                                call.arguments = parsed.into();
                            }
                        });
                    } else if let Some(input) =
                        call.get("custom").and_then(|f| nonempty(f.get("input")))
                    {
                        let mut next = self.custom_input(index);
                        next.push(&input);
                        delta = self.append_custom(index, &next, false)?.unwrap_or_default();
                    }
                    self.sync_scratch(index);
                    self.events.push(AssistantMessageEvent::ToolcallDelta {
                        content_index: index,
                        delta,
                        partial: self.partial.clone(),
                    });
                }
            }
            if let Some(details) = delta.get("reasoning_details").and_then(JsValue::as_array) {
                for detail in details.iter().filter(|d| valid_reasoning_detail(d)) {
                    self.ensure_thinking(JsString::default());
                    append_reasoning_detail(self.thinking_details.get_or_insert_default(), detail);
                }
            }
        }
        Ok(())
    }
    fn finish_blocks(&mut self) -> Result<(), JsString> {
        for index in 0..self.block_count {
            match self
                .partial
                .content_block(index)
                .expect("block exists")
                .snapshot()
            {
                AssistantContent::Text(block) => self.events.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: block.text,
                    partial: self.partial.clone(),
                }),
                AssistantContent::Thinking(block) => {
                    self.apply_reasoning_details();
                    self.events.push(AssistantMessageEvent::ThinkingEnd {
                        content_index: index,
                        content: block.thinking,
                        partial: self.partial.clone(),
                    });
                }
                AssistantContent::ToolCall(_) => {
                    if self.scratch[&index].custom_input.is_some() {
                        let input = self.custom_input(index);
                        if let Some(delta) = self.append_custom(index, &input, true)? {
                            self.events.push(AssistantMessageEvent::ToolcallDelta {
                                content_index: index,
                                delta,
                                partial: self.partial.clone(),
                            });
                        }
                    } else {
                        let arguments = crate::utils::json_parse::parse_streaming_json_utf16(
                            self.scratch[&index].partial_args.as_ref(),
                        );
                        self.partial.update_block(index, |block| {
                            if let AssistantContent::ToolCall(call) = block {
                                call.arguments = arguments.into();
                            }
                        });
                    }
                    self.clear_scratch(index, false);
                    let AssistantContent::ToolCall(call) = self
                        .partial
                        .content_block(index)
                        .expect("tool exists")
                        .snapshot()
                    else {
                        unreachable!()
                    };
                    self.events.push(AssistantMessageEvent::ToolcallEnd {
                        content_index: index,
                        tool_call: call,
                        partial: self.partial.clone(),
                    });
                }
            }
        }
        Ok(())
    }
    fn final_message(&self) -> AssistantMessage {
        self.sync_metadata();
        self.partial.snapshot()
    }
}
pub fn parse_chunk_usage(raw: &JsValue, model: &Model) -> Usage {
    let numeric = |v: Option<&JsValue>| {
        v.and_then(JsValue::as_f64)
            .filter(|n| *n != 0.0 && !n.is_nan())
            .unwrap_or(0.0)
    };
    let prompt = numeric(raw.get("prompt_tokens"));
    let details = raw.get("prompt_tokens_details");
    let cache_read = details
        .and_then(|v| v.get("cached_tokens"))
        .filter(|v| !v.is_null())
        .or_else(|| raw.get("prompt_cache_hit_tokens").filter(|v| !v.is_null()))
        .or_else(|| raw.get("cached_tokens").filter(|v| !v.is_null()))
        .and_then(JsValue::as_f64)
        .unwrap_or(0.0);
    let cache_write = numeric(details.and_then(|v| v.get("cache_write_tokens")));
    let input = js_max(0.0, prompt - cache_read - cache_write);
    let output = numeric(raw.get("completion_tokens"));
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: Some(numeric(
            raw.get("completion_tokens_details")
                .and_then(|v| v.get("reasoning_tokens")),
        )),
        total_tokens: input + output + cache_read + cache_write,
        ..Default::default()
    };
    calculate_cost(model, &mut usage);
    usage
}
pub fn map_stop_reason(reason: &JsString) -> (StopReason, Option<JsString>) {
    match reason.as_str() {
        Some("stop" | "end") => (StopReason::Stop, None),
        Some("length") => (StopReason::Length, None),
        Some("function_call" | "tool_calls") => (StopReason::ToolUse, None),
        _ => {
            let mut error = JsString::from("Provider finish_reason: ");
            error.push(reason);
            (StopReason::Error, Some(error))
        }
    }
}
pub fn stream(
    model: Model,
    context: TranscriptContext,
    options: Option<OpenAICompletionsOptions>,
    transport: Arc<dyn CompletionsTransport>,
    env: Arc<dyn PiEnv>,
) -> Result<AssistantMessageEventStream, JsString> {
    let events = create_assistant_message_event_stream();
    let writer = events.writer();
    let compat = get_compat(&model);
    let context = resolve_transcript(context, compat.supports_mid_convo_system_messages)?;
    let timestamp = env.now_ms() as f64;
    let options = options.unwrap_or_default();
    let producer = async move {
        let mut parser =
            ChunkParser::new(model.clone(), timestamp, writer.clone(), IndexMap::new());
        let result: Result<(), JsString> = async {
            parser.properties = create_grammar_tool_input_properties(
                &get_declared_tools(&context.messages)?,
                is_enabled(compat.supports_open_ai_grammar_tools),
            )?;
            let retention = resolve_cache_retention(
                options.cache_retention,
                options.env.as_ref(),
                options.cache_retention_env.as_deref(),
            );
            let headers = request_headers(&model, &options, &compat, retention);
            let mut params = build_params_resolved(
                &model,
                &context,
                &options,
                &compat,
                retention,
                &parser.properties,
                env.as_ref(),
            )?;
            if let Some(callback) = &options.on_payload
                && let Some(next) = callback(params.clone(), model.clone())
                    .await
                    .map_err(|e| JsString::from(e.to_string()))?
            {
                params = next;
            }
            let mut response = transport
                .send(CompletionsRequest {
                    model: model.clone(),
                    params,
                    headers,
                    signal: options.signal.clone(),
                    timeout_ms: options.timeout_ms,
                    max_retries: options.max_retries,
                    max_retry_delay_ms: options.max_retry_delay_ms,
                })
                .await?;
            if let Some(callback) = &options.on_response {
                callback(response.response, model.clone())
                    .await
                    .map_err(|e| JsString::from(e.to_string()))?;
            }
            writer.push(AssistantMessageEvent::Start {
                partial: parser.partial.clone(),
            });
            loop {
                let chunk = response.chunks.next().await;
                microtask().await;
                let Some(chunk) = chunk else {
                    break;
                };
                let chunk = chunk?;
                if let Some(callback) = &options.on_provider_stream_event {
                    callback(chunk.clone(), model.clone())
                        .await
                        .map_err(|e| JsString::from(e.to_string()))?;
                }
                microtask().await;
                parser.accept(&chunk)?;
            }
            parser.finish_blocks()?;
            if options.signal.as_ref().is_some_and(|s| s.is_cancelled())
                || parser.output.stop_reason == StopReason::Aborted
            {
                return Err("Request was aborted".into());
            }
            if !parser.has_finish_reason && !is_enabled(compat.supports_finish_reason) {
                parser.output.stop_reason = if parser.scratch.is_empty() {
                    StopReason::Stop
                } else {
                    StopReason::ToolUse
                };
            }
            if parser.output.stop_reason == StopReason::Error {
                return Err(parser
                    .output
                    .error_message
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "Provider returned an error stop reason".into()));
            }
            if (is_enabled(compat.supports_finish_reason) && !parser.has_finish_reason)
                || parser.output.stop_reason == StopReason::Pending
            {
                return Err("Stream ended without finish_reason".into());
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                let message = parser.final_message();
                let reason = match message.stop_reason {
                    StopReason::Stop => DoneReason::Stop,
                    StopReason::Length => DoneReason::Length,
                    StopReason::ToolUse => DoneReason::ToolUse,
                    StopReason::Deferred => DoneReason::Deferred,
                    _ => unreachable!("completed stream reason"),
                };
                writer.push(AssistantMessageEvent::Done { reason, message });
            }
            Err(error) => {
                parser.apply_reasoning_details();
                for index in parser.scratch.keys() {
                    parser.clear_scratch(*index, true);
                }
                let aborted = options.signal.as_ref().is_some_and(|s| s.is_cancelled());
                parser.output.stop_reason = if aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                parser.output.error_message = Some(error);
                writer.push(AssistantMessageEvent::Error {
                    reason: if aborted {
                        ErrorReason::Aborted
                    } else {
                        ErrorReason::Error
                    },
                    error: parser.final_message(),
                });
            }
        }
        writer.end(None);
    };
    events.set_producer(producer);
    Ok(events)
}
pub fn stream_simple(
    model: Model,
    context: TranscriptContext,
    options: Option<SimpleStreamOptions>,
    transport: Arc<dyn CompletionsTransport>,
    env: Arc<dyn PiEnv>,
) -> Result<AssistantMessageEventStream, JsString> {
    let options = options.unwrap_or_default();
    let base = build_base_options(&model, &context, Some(&options), options.api_key.as_deref())?;
    let reasoning = options
        .reasoning
        .map(|level| clamp_thinking_level(&model, level.into()));
    let reasoning_effort = reasoning.and_then(|level| match level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Minimal => Some(ThinkingLevel::Minimal),
        ModelThinkingLevel::Low => Some(ThinkingLevel::Low),
        ModelThinkingLevel::Medium => Some(ThinkingLevel::Medium),
        ModelThinkingLevel::High => Some(ThinkingLevel::High),
        ModelThinkingLevel::Xhigh => Some(ThinkingLevel::Xhigh),
        ModelThinkingLevel::Max => Some(ThinkingLevel::Max),
    });
    stream(
        model,
        context,
        Some(OpenAICompletionsOptions {
            stream: base,
            tool_choice: options.tool_choice,
            reasoning_effort,
            thinking_budgets: options.thinking_budgets,
        }),
        transport,
        env,
    )
}

async fn microtask() {
    let mut queued = false;
    std::future::poll_fn(move |cx| {
        if queued {
            std::task::Poll::Ready(())
        } else {
            queued = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

/// Rust's associated-type counterpart of Pi's compile-time `ApiOptionsMap`.
pub trait ApiOptionsMap {
    type Options;
}
/// The sole selected named API mapping.
pub struct OpenAICompletionsApi;
impl ApiOptionsMap for OpenAICompletionsApi {
    type Options = OpenAICompletionsOptions;
}
/// Custom API names use Pi's generic stream-options shape.
pub struct GenericApi;
impl ApiOptionsMap for GenericApi {
    type Options = ProviderStreamOptions;
}
pub type ApiStreamOptions<A = OpenAICompletionsApi> = <A as ApiOptionsMap>::Options;

#[cfg(test)]
mod parser_regressions {
    use super::*;
    #[test]
    fn malformed_tool_call_containers_preserve_javascript_iteration() {
        for (raw, expected_count, expected_error) in [
            ("{}", 0, Some("choice.delta.tool_calls is not iterable")),
            ("true", 0, Some("choice.delta.tool_calls is not iterable")),
            ("42", 0, Some("choice.delta.tool_calls is not iterable")),
            (
                "[null]",
                0,
                Some("Cannot read properties of null (reading 'index')"),
            ),
            ("[42]", 1, None),
            (r#""ab""#, 2, None),
            (r#""\ud83d\ude00\ud800""#, 2, None),
            ("null", 0, None),
            ("false", 0, None),
            ("0", 0, None),
            (r#""""#, 0, None),
        ] {
            let events = create_assistant_message_event_stream();
            let mut parser =
                ChunkParser::new(Model::default(), 0.0, events.writer(), IndexMap::new());
            let chunk = crate::utils::json_parse::parse_json(&format!(
                r#"{{"choices":[{{"delta":{{"tool_calls":{raw}}},"finish_reason":"tool_calls"}}]}}"#
            ))
            .unwrap();
            let result = parser.accept(&chunk);
            assert_eq!(
                result.as_ref().err().and_then(JsString::as_str),
                expected_error,
                "{raw}"
            );
            if result.is_ok() {
                parser.finish_blocks().unwrap();
                let output = parser.final_message();
                assert_eq!(output.content.len(), expected_count, "{raw}");
                for block in &output.content {
                    let AssistantContent::ToolCall(call) = block else {
                        panic!("expected tool call")
                    };
                    assert!(call.name.is_empty());
                    assert!(call.id.is_empty());
                    assert_eq!(call.arguments, JsValue::Object(JsObject::new()));
                    assert!(call.extra.is_empty());
                }
            }
        }
    }
    #[test]
    fn empty_function_name_wins_over_custom_name_in_the_same_delta() {
        let events = create_assistant_message_event_stream();
        let mut parser = ChunkParser::new(Model::default(), 0.0, events.writer(), IndexMap::new());
        let chunk = crate::utils::json_parse::parse_json(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"one","function":{"name":"","arguments":"{}"},"custom":{"name":"other","input":"ignored"}}]},"finish_reason":"tool_calls"}]}"#).unwrap();
        parser.accept(&chunk).unwrap();
        parser.finish_blocks().unwrap();
        let output = parser.final_message();
        let AssistantContent::ToolCall(call) = &output.content[0] else {
            panic!("expected tool call")
        };
        assert!(call.name.is_empty());
        assert_eq!(call.arguments, JsValue::Object(JsObject::new()));
        assert!(call.extra.is_empty());
        assert_eq!(output.stop_reason, StopReason::ToolUse);
    }
}

#[cfg(test)]
mod host_cache_environment_tests {
    use super::*;
    #[test]
    fn cache_environment_requires_a_host_key_and_explicit_options_win() {
        let env = ProviderEnv::from([("HOST_CACHE_RETENTION".into(), "long".into())]);
        assert_eq!(
            resolve_cache_retention(None, Some(&env), None),
            CacheRetention::Short
        );
        assert_eq!(
            resolve_cache_retention(None, Some(&env), Some("HOST_CACHE_RETENTION")),
            CacheRetention::Long
        );
        assert_eq!(
            resolve_cache_retention(
                Some(CacheRetention::None),
                Some(&env),
                Some("HOST_CACHE_RETENTION")
            ),
            CacheRetention::None
        );
        assert_eq!(
            resolve_cache_retention(None, Some(&env), Some("HOST_OTHER_CACHE")),
            CacheRetention::Short
        );
    }
}
