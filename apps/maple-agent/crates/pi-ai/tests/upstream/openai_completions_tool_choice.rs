use super::common::*;
use pi_ai::api::openai_completions::{
    OpenAICompletionsOptions, ScriptedTransport, convert_messages, stream, stream_simple,
};
use pi_ai::types::*;
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json as j};
use std::sync::Arc;

fn observed_json(value: &impl serde::Serialize) -> Value {
    serde_json::from_str(&pi_ai::utils::js_json::stringify_serializable(value).unwrap()).unwrap()
}

fn unconfigured_model(provider: &str, id: &str) -> Model {
    let mut model = fixture_model(provider, id);
    model.api = "openai-completions".into();
    model.compat = None;
    model
}

fn local_model(id: &str, name: &str) -> Model {
    json(
        j!({"id":id,"name":name,"api":"openai-completions","provider":"local-vllm",
        "baseUrl":"http://localhost:8000/v1","reasoning":true,"input":["text"],
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":128000,"maxTokens":8192}),
    )
}

fn context(text: &str) -> Value {
    j!({"messages":[{"role":"user","content":text,"timestamp":0}]})
}

fn tool(name: &str, description: &str, properties: Value, required: Value) -> Value {
    j!({"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required}})
}

fn ping() -> Value {
    tool(
        "ping",
        "Ping tool",
        j!({"ok":{"type":"boolean"}}),
        j!(["ok"]),
    )
}
fn read_tool() -> Value {
    tool(
        "read",
        "Read a file",
        j!({"path":{"type":"string"}}),
        j!(["path"]),
    )
}
fn with_tools(text: &str, tools: Vec<Value>) -> Value {
    let mut c = context(text);
    c["tools"] = j!(tools);
    c
}
fn usage(input: u64, output: u64, cached: u64, reasoning: u64) -> Value {
    j!({"prompt_tokens":input,"completion_tokens":output,"prompt_tokens_details":{"cached_tokens":cached},"completion_tokens_details":{"reasoning_tokens":reasoning}})
}
fn default_chunks() -> Vec<Value> {
    vec![j!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":usage(1,1,0,0)})]
}

async fn execute(
    model: Model,
    ctx: Value,
    mut options: Value,
    chunks: Vec<Value>,
) -> (Value, Value, Vec<Value>) {
    options["apiKey"] = j!("test");
    let transport = Arc::new(ScriptedTransport::default());
    transport.push_response(
        ProviderResponse {
            status: 200,
            ..Default::default()
        },
        chunks.into_iter().map(|v| Ok(js(v))),
    );
    let mut events = stream_simple(
        model,
        normalize_context(json(ctx)),
        Some(json(options)),
        transport.clone(),
        env(),
    )
    .unwrap();
    let mut emitted = Vec::new();
    while let Some(event) = events.next().await {
        emitted.push(observed_json(&event));
    }
    let response = observed_json(&events.result().await);
    let requests = transport.requests();
    assert_eq!(
        requests.len(),
        1,
        "expected SDK replacement to receive one request: {response}"
    );
    (observed_json(&requests[0].params), response, emitted)
}

async fn capture(model: Model, ctx: Value, options: Value) -> Value {
    execute(model, ctx, options, default_chunks()).await.0
}

fn replay_assistant(provider: &str, model: &str, content: Vec<Value>, stop: &str) -> Value {
    j!({"role":"assistant","api":"openai-completions","provider":provider,"model":model,
        "content":content,"usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":stop,"timestamp":0})
}
fn read_call() -> Value {
    j!({"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"README.md"}})
}
fn read_result() -> Value {
    j!({"role":"toolResult","toolCallId":"call_1","toolName":"read","content":[{"type":"text","text":"contents"}],"isError":false,"timestamp":0})
}
fn assistant(params: &Value) -> &Value {
    params["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap()
}
fn metadata(model: &Model) -> Value {
    observed_json(model)
}

// Retain both the upstream file and describe-block namespaces in coverage paths.
#[allow(clippy::module_inception)]
mod openai_completions_tool_choice {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn forwards_tool_choice_from_simple_options_to_payload() {
        let params = capture(
            unconfigured_model("openai", "gpt-4o-mini"),
            with_tools("Call ping with ok=true", vec![ping()]),
            j!({"toolChoice":"required"}),
        )
        .await;
        assert_eq!(params["tool_choice"], "required");
        assert!(params["tools"].is_array());
        assert!(!params["tools"].as_array().unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn includes_tool_choice_when_no_tools_are_provided() {
        let params = capture(
            unconfigured_model("openai", "gpt-4o-mini"),
            context("Summarize the conversation"),
            j!({"toolChoice":"none"}),
        )
        .await;
        assert_eq!(params["tool_choice"], "none");
        assert!(params.get("tools").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_strict_when_compat_disables_strict_mode() {
        let mut model = unconfigured_model("openai", "gpt-4o-mini");
        model.compat = Some(json(j!({"supportsStrictMode":false})));
        let params = capture(
            model,
            with_tools("Call ping with ok=true", vec![ping()]),
            j!({}),
        )
        .await;
        let function = params["tools"][0].get("function").expect("tool must exist");
        assert!(function.get("strict").is_none());
        assert!(!function.as_object().unwrap().contains_key("strict"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn defaults_unknown_openai_compatible_endpoints_to_non_strict_tools() {
        // Upstream regression test for #9816.
        let mut t = tool(
            "ping",
            "Ping tool",
            j!({"required":{"type":"string"},"optional":{"type":"string"}}),
            j!(["required"]),
        );
        t["constrainedSampling"] = j!({"type":"json_schema","strict":"prefer"});
        let params = capture(
            local_model("local-model", "Local Model"),
            with_tools("Call ping", vec![t]),
            j!({}),
        )
        .await;
        let function = &params["tools"][0]["function"];
        assert!(function.get("strict").is_none());
        assert_eq!(function["parameters"]["required"], j!(["required"]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_strict_tools_for_capable_built_in_chat_completions_models() {
        let model = fixture_model("groq", "openai/gpt-oss-20b");
        assert_eq!(metadata(&model)["compat"]["supportsStrictMode"], true);
        let mut t = tool(
            "ping",
            "Ping tool",
            j!({"required":{"type":"string"},"optional":{"type":"string"}}),
            j!(["required"]),
        );
        t["constrainedSampling"] = j!({"type":"json_schema","strict":"prefer"});
        let params = capture(model, with_tools("Call ping", vec![t]), j!({})).await;
        assert_eq!(params["tools"][0]["function"]["strict"], true);
        assert_eq!(
            params["tools"][0]["function"]["parameters"]["required"],
            j!(["required", "optional"])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn maps_groq_qwen_reasoning_levels_to_default_reasoning_effort() {
        let params = capture(
            fixture_model("groq", "qwen/qwen3.6-27b"),
            context("Hi"),
            j!({"reasoning":"medium"}),
        )
        .await;
        assert_eq!(params["reasoning_effort"], "default");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keeps_normal_reasoning_effort_for_groq_models_without_compat_mapping() {
        let params = capture(
            fixture_model("groq", "openai/gpt-oss-20b"),
            context("Hi"),
            j!({"reasoning":"medium"}),
        )
        .await;
        assert_eq!(params["reasoning_effort"], "medium");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enables_tool_stream_for_supported_z_ai_models_with_tools() {
        let params = capture(
            fixture_model("zai", "glm-5.2"),
            with_tools("Call ping with ok=true", vec![ping()]),
            j!({}),
        )
        .await;
        assert_eq!(params["tool_stream"], true);
    }

    #[test]
    fn stores_z_ai_tool_stream_support_in_model_compat_metadata() {
        for id in ["glm-4.7", "glm-4.7", "glm-5-turbo", "glm-5.2"] {
            assert_eq!(
                metadata(&fixture_model("zai", id))["compat"]["zaiToolStream"],
                true
            );
        }
    }

    #[test]
    fn stores_z_ai_effort_metadata() {
        for id in ["glm-5.2", "glm-5.2-highspeed"] {
            let model = metadata(&fixture_model("zai", id));
            assert_eq!(model["compat"]["supportsReasoningEffort"], true);
            assert_eq!(
                model["thinkingLevelMap"],
                j!({"off":"none","minimal":null,"low":null,"medium":null,"high":"high","xhigh":null,"max":"max"})
            );
        }
        for provider in ["zai", "zai-coding-cn"] {
            let model = metadata(&fixture_model(provider, "glm-5.3"));
            assert_eq!(model["compat"]["supportsReasoningEffort"], true);
            assert_eq!(
                model["thinkingLevelMap"],
                j!({"off":null,"minimal":null,"low":"low","medium":null,"high":"high","xhigh":null,"max":"max"})
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn maps_z_ai_glm_5_2_thinking_levels_to_reasoning_effort() {
        for (reasoning, effort) in [
            ("low", "high"),
            ("medium", "high"),
            ("high", "high"),
            ("max", "max"),
        ] {
            let params = capture(
                fixture_model("zai", "glm-5.2"),
                context("Hi"),
                j!({"reasoning":reasoning}),
            )
            .await;
            assert_eq!(
                params["thinking"],
                j!({"type":"enabled","clear_thinking":false})
            );
            assert_eq!(params["reasoning_effort"], effort);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_z_ai_thinking_when_replaying_reasoning_content() {
        let a = replay_assistant(
            "zai",
            "glm-5.2",
            vec![
                j!({"type":"thinking","thinking":"prior reasoning","thinkingSignature":"reasoning_content"}),
                read_call(),
            ],
            "toolUse",
        );
        let params = capture(fixture_model("zai","glm-5.2"),j!({"messages":[{"role":"user","content":"Read README.md","timestamp":0},a,read_result(),{"role":"user","content":"Continue","timestamp":0}]}),j!({"reasoning":"high"})).await;
        assert_eq!(assistant(&params)["reasoning_content"], "prior reasoning");
        assert_eq!(
            params["thinking"],
            j!({"type":"enabled","clear_thinking":false})
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_z_ai_glm_5_2_reasoning_effort_when_thinking_is_off() {
        let params = capture(fixture_model("zai", "glm-5.2"), context("Hi"), j!({})).await;
        assert_eq!(params["thinking"], j!({"type":"disabled"}));
        assert!(params.get("reasoning_effort").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn respects_explicit_z_ai_tool_stream_compat_override() {
        let mut model = fixture_model("zai", "glm-5.2");
        model.compat.as_mut().unwrap().zai_tool_stream = Some(true);
        let params = capture(
            model,
            with_tools("Call ping with ok=true", vec![ping()]),
            j!({}),
        )
        .await;
        assert_eq!(params["tool_stream"], true);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_tool_stream_when_no_tools_are_provided() {
        let params = capture(fixture_model("zai", "glm-5.2"), context("Hi"), j!({})).await;
        assert!(params.get("tool_stream").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn maps_non_standard_provider_finish_reason_values_to_stop_reason_error() {
        let (_,response,_) = execute(fixture_model("zai","glm-5.2"),context("Hi"),j!({}),vec![
            j!({"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}),
            j!({"choices":[{"delta":{},"finish_reason":"network_error"}],"usage":usage(1,1,0,0)})]).await;
        assert_eq!(response["stopReason"], "error");
        assert_eq!(
            response["errorMessage"],
            "Provider finish_reason: network_error"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ignores_null_stream_chunks_from_openai_compatible_providers() {
        let (_,response,_) = execute(unconfigured_model("openai","gpt-4o-mini"),context("Reply with exactly OK"),j!({}),vec![Value::Null,
            j!({"id":"chatcmpl-test","choices":[{"delta":{"content":"OK"},"finish_reason":null}]}),
            j!({"id":"chatcmpl-test","choices":[{"delta":{},"finish_reason":"stop"}],"usage":usage(3,1,0,0)})]).await;
        assert_eq!(response["stopReason"], "stop");
        assert!(response.get("errorMessage").is_none());
        assert_eq!(response["responseId"], "chatcmpl-test");
        assert_eq!(response["usage"]["totalTokens"], 4);
        assert_eq!(response["content"], j!([{"type":"text","text":"OK"}]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn errors_when_a_stream_ends_after_only_null_finish_reason_chunks() {
        let chunk = j!({"id":"chatcmpl-truncated","choices":[{"delta":{"content":"partial answer"},"finish_reason":null}]});
        let (_, response, _) = execute(
            unconfigured_model("openai", "gpt-4o-mini"),
            context("Reply with a longer sentence"),
            j!({}),
            vec![chunk.clone(), chunk],
        )
        .await;
        assert_eq!(response["stopReason"], "error");
        assert_eq!(
            response["errorMessage"],
            "Stream ended without finish_reason"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accepts_streams_without_finish_reason_when_compat_disables_it() {
        let mut model = unconfigured_model("openai", "gpt-4o-mini");
        model.compat = Some(json(j!({"supportsFinishReason":false})));
        let (_,response,_) = execute(model,context("Reply with a complete answer"),j!({}),vec![j!({"id":"chatcmpl-no-finish-reason","choices":[{"delta":{"content":"complete answer"},"finish_reason":null}]})]).await;
        assert_eq!(response["stopReason"], "stop");
        assert!(response.get("errorMessage").is_none());
        assert_eq!(
            response["content"],
            j!([{"type":"text","text":"complete answer"}])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ignores_empty_custom_objects_on_function_tool_call_deltas() {
        let (_,response,_) = execute(unconfigured_model("openai","gpt-4o-mini"),with_tools("Read README.md",vec![read_tool()]),j!({}),vec![
            j!({"id":"chatcmpl-empty-custom","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"README.md\"}"},"custom":{}}]},"finish_reason":"tool_calls"}]})]).await;
        assert_eq!(response["content"], j!([read_call()]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn coalesces_tool_call_deltas_by_stable_index_when_provider_mutates_ids_mid_stream() {
        let (_,response,events) = execute(unconfigured_model("openai","gpt-4o-mini"),with_tools("Read README.md",vec![read_tool()]),j!({}),vec![
            j!({"id":"chatcmpl-kimi-bad-stream","choices":[{"delta":{"tool_calls":[{"index":0,"id":"functions.read:0","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}),
            j!({"id":"chatcmpl-kimi-bad-stream","choices":[{"delta":{"tool_calls":[{"index":0,"id":"chatcmpl-tool-a","type":"function","function":{"name":null,"arguments":"{\"path\":\"README"}}]},"finish_reason":null}]}),
            j!({"id":"chatcmpl-kimi-bad-stream","choices":[{"delta":{"tool_calls":[{"index":0,"id":"chatcmpl-tool-b","type":"function","function":{"name":null,"arguments":".md\"}"}}]},"finish_reason":"tool_calls"}],"usage":usage(10,5,0,0)})]).await;
        let indexes: Vec<_> = events
            .iter()
            .filter(|e| {
                matches!(
                    e["type"].as_str(),
                    Some("toolcall_start" | "toolcall_delta" | "toolcall_end")
                )
            })
            .map(|e| e["contentIndex"].clone())
            .collect();
        assert_eq!(response["stopReason"], "toolUse");
        assert_eq!(indexes, j!([0, 0, 0, 0, 0]).as_array().unwrap().clone());
        assert_eq!(response["content"].as_array().unwrap().len(), 1);
        let call = &response["content"][0];
        assert_eq!(call["type"], "toolCall");
        assert_eq!(call["id"], "functions.read:0");
        assert_eq!(call["name"], "read");
        assert_eq!(call["arguments"], j!({"path":"README.md"}));
        assert!(call.get("streamIndex").is_none());
        assert!(call.get("partialArgs").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accumulates_mixed_content_reasoning_and_parallel_tool_call_deltas_independently() {
        let tools = vec![
            read_tool(),
            tool(
                "grep",
                "Search a file",
                j!({"pattern":{"type":"string"},"path":{"type":"string"}}),
                j!(["pattern", "path"]),
            ),
            tool(
                "list",
                "List a directory",
                j!({"path":{"type":"string"}}),
                j!(["path"]),
            ),
            tool(
                "write",
                "Write a file",
                j!({"path":{"type":"string"},"content":{"type":"string"}}),
                j!(["path", "content"]),
            ),
        ];
        let (_,response,events) = execute(unconfigured_model("openai","gpt-4o-mini"),with_tools("Think, answer, and use tools.",tools),j!({}),vec![
            j!({"id":"chatcmpl-mixed-deltas","choices":[{"delta":{"content":"answer 1","reasoning_content":"think 1","tool_calls":[
                {"index":0,"id":"tc_read_initial","type":"function","function":{"name":"read","arguments":"{\"path\":\"README"}},
                {"index":1,"id":"tc_grep_initial","type":"function","function":{"name":"grep","arguments":"{\"pattern\":\"TODO"}},
                {"id":"tc_list_no_index","type":"function","function":{"name":"list","arguments":"{\"path\":\"packages"}},
                {"id":"tc_write_no_index","type":"function","function":{"name":"write","arguments":"{\"path\":\"out"}}]},"finish_reason":null}]}),
            j!({"id":"chatcmpl-mixed-deltas","choices":[{"delta":{"content":" answer 2","tool_calls":[
                {"index":1,"id":"tc_grep_changed","type":"function","function":{"arguments":"\",\"path\":\"src"}},
                {"id":"tc_write_no_index","type":"function","function":{"arguments":".txt\",\"content\":\"ok\"}"}},
                {"id":"tc_list_no_index","type":"function","function":{"arguments":"/ai\"}"}}]},"finish_reason":null}]}),
            j!({"id":"chatcmpl-mixed-deltas","choices":[{"delta":{"content":"\n","reasoning_content":" think 2","tool_calls":[
                {"index":0,"id":"tc_read_changed","type":"function","function":{"arguments":".md\"}"}},
                {"index":1,"type":"function","function":{"arguments":"\"}"}}]},"finish_reason":"tool_calls"}],"usage":usage(10,8,0,2)})]).await;
        assert_eq!(response["stopReason"], "toolUse");
        for (kind, count) in [
            ("text_start", 1),
            ("text_delta", 3),
            ("text_end", 1),
            ("thinking_start", 1),
            ("thinking_delta", 2),
            ("thinking_end", 1),
            ("toolcall_start", 4),
            ("toolcall_delta", 9),
            ("toolcall_end", 4),
        ] {
            assert_eq!(
                events.iter().filter(|e| e["type"] == kind).count(),
                count,
                "{kind}"
            );
        }
        for (index, delta_count) in [(2, 2), (3, 3), (4, 2), (5, 2)] {
            let actual: Vec<_> = events
                .iter()
                .filter(|e| {
                    matches!(
                        e["type"].as_str(),
                        Some("toolcall_start" | "toolcall_delta" | "toolcall_end")
                    ) && e["contentIndex"] == index
                })
                .map(|e| e["type"].as_str().unwrap())
                .collect();
            let mut expected = vec!["toolcall_start"];
            expected.extend(std::iter::repeat_n("toolcall_delta", delta_count));
            expected.push("toolcall_end");
            assert_eq!(actual, expected);
        }
        assert_eq!(response["content"].as_array().unwrap().len(), 6);
        assert_eq!(
            response["content"][0],
            j!({"type":"text","text":"answer 1 answer 2\n"})
        );
        assert_eq!(
            response["content"][1],
            j!({"type":"thinking","thinking":"think 1 think 2","thinkingSignature":"reasoning_content"})
        );
        for (index, id, name, args) in [
            (2, "tc_read_initial", "read", j!({"path":"README.md"})),
            (
                3,
                "tc_grep_initial",
                "grep",
                j!({"pattern":"TODO","path":"src"}),
            ),
            (4, "tc_list_no_index", "list", j!({"path":"packages/ai"})),
            (
                5,
                "tc_write_no_index",
                "write",
                j!({"path":"out.txt","content":"ok"}),
            ),
        ] {
            let call = &response["content"][index];
            assert_eq!(call["type"], "toolCall");
            assert_eq!(call["id"], id);
            assert_eq!(call["name"], name);
            assert_eq!(call["arguments"], args);
            assert!(call.get("streamIndex").is_none());
            assert!(call.get("partialArgs").is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uses_system_messages_for_non_openai_anthropic_open_router_reasoning_model_instructions()
     {
        let mut ctx = context("Hi");
        ctx["systemPrompt"] = j!("Follow instructions.");
        let params = capture(
            fixture_model("openrouter", "deepseek/deepseek-v4-pro"),
            ctx,
            j!({}),
        )
        .await;
        assert_eq!(params["messages"][0]["role"], "system");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keeps_developer_messages_for_openai_and_anthropic_open_router_batch_instructions() {
        for id in ["openai/gpt-5.2-codex", "anthropic/claude-fable-5.1:batch"] {
            let mut ctx = context("Hi");
            ctx["systemPrompt"] = j!("Follow instructions.");
            let params = capture(fixture_model("openrouter", id), ctx, j!({})).await;
            assert_eq!(params["messages"][0]["role"], "developer");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keeps_developer_messages_for_openai_reasoning_model_instructions() {
        let mut ctx = context("Hi");
        ctx["systemPrompt"] = j!("Follow instructions.");
        let params = capture(unconfigured_model("openai", "gpt-5.5"), ctx, j!({})).await;
        assert_eq!(params["messages"][0]["role"], "developer");
    }

    #[test]
    fn stores_open_router_kimi_k2_6_reasoning_replay_compat_in_built_in_metadata() {
        let model = metadata(&fixture_model("openrouter", "moonshotai/kimi-k2.6"));
        assert_eq!(model["compat"]["supportsDeveloperRole"], false);
        assert_eq!(
            model["compat"]["requiresReasoningContentOnAssistantMessages"],
            true
        );
    }

    #[test]
    fn stores_xiaomi_mi_mo_reasoning_replay_compat_in_built_in_metadata() {
        for provider in [
            "xiaomi",
            "xiaomi-token-plan-cn",
            "xiaomi-token-plan-ams",
            "xiaomi-token-plan-sgp",
        ] {
            let model = metadata(&fixture_model(provider, "mimo-v2.5-pro"));
            let compat = &model["compat"];
            assert_eq!(compat["requiresReasoningContentOnAssistantMessages"], true);
            assert_eq!(compat["thinkingFormat"], "deepseek");
            assert!(compat.get("maxTokensField").is_none());
            assert!(compat.get("supportsDeveloperRole").is_none());
        }
    }

    #[test]
    fn stores_qwen_token_plan_reasoning_replay_compat_in_built_in_metadata() {
        for provider in [
            "qwen-token-plan",
            "qwen-token-plan-cn",
            "qwen-token-plan-individual",
        ] {
            let model = metadata(&fixture_model(provider, "qwen3.7-max"));
            let compat = &model["compat"];
            assert_eq!(compat["thinkingFormat"], "qwen");
            assert!(
                compat
                    .get("requiresReasoningContentOnAssistantMessages")
                    .is_none()
            );
            assert_eq!(compat["supportsDeveloperRole"], false);
            assert_eq!(compat["supportsStore"], false);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replays_xiaomi_mi_mo_assistant_tool_calls_with_empty_reasoning_content_when_thinking_is_missing()
     {
        let a = replay_assistant("xiaomi", "mimo-v2.5-pro", vec![read_call()], "toolUse");
        let params = capture(fixture_model("xiaomi","mimo-v2.5-pro"),j!({"messages":[{"role":"user","content":"Read README.md","timestamp":0},a,read_result()]}),j!({"reasoning":"high"})).await;
        assert_eq!(assistant(&params)["role"], "assistant");
        assert_eq!(assistant(&params)["reasoning_content"], "");
        assert_eq!(params["thinking"], j!({"type":"enabled"}));
        assert_eq!(params["reasoning_effort"], "high");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn normalizes_open_code_go_reasoning_deltas_to_reasoning_content_for_replay() {
        let (_,response,_) = execute(unconfigured_model("opencode-go","kimi-k3"),context("Use reasoning."),j!({}),vec![j!({"id":"chatcmpl-opencode-go-reasoning","choices":[{"delta":{"reasoning":"think"},"finish_reason":"stop"}]})]).await;
        assert_eq!(
            response["content"],
            j!([{"type":"thinking","thinking":"think","thinkingSignature":"reasoning_content"}])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keeps_non_open_code_go_reasoning_deltas_on_the_original_reasoning_field() {
        let (_,response,_) = execute(unconfigured_model("openai","gpt-4o-mini"),context("Use reasoning."),j!({}),vec![j!({"id":"chatcmpl-reasoning","choices":[{"delta":{"reasoning":"think"},"finish_reason":"stop"}]})]).await;
        assert_eq!(
            response["content"],
            j!([{"type":"thinking","thinking":"think","thinkingSignature":"reasoning"}])
        );
    }

    #[test]
    fn replays_open_code_go_reasoning_thinking_blocks_as_reasoning_content() {
        let model = unconfigured_model("opencode-go", "kimi-k3");
        let a = replay_assistant(
            "opencode-go",
            "kimi-k3",
            vec![
                j!({"type":"thinking","thinking":"think","thinkingSignature":"reasoning"}),
                read_call(),
            ],
            "stop",
        );
        let compat = json(
            j!({"supportsStore":false,"supportsDeveloperRole":false,"supportsReasoningEffort":true,"supportsUsageInStreaming":true,
            "supportsFinishReason":true,"maxTokensField":"max_completion_tokens","requiresToolResultName":false,"requiresAssistantAfterToolResult":false,
            "requiresThinkingAsText":false,"requiresReasoningContentOnAssistantMessages":false,"thinkingFormat":"openai","openRouterRouting":{},
            "vercelGatewayRouting":{},"chatTemplateKwargs":{},"chatTemplateArgs":{},"zaiToolStream":false,"supportsStrictMode":true,
            "supportsOpenAIGrammarTools":false,"sendSessionAffinityHeaders":false,"sessionAffinityFormat":"openai","supportsLongCacheRetention":true}),
        );
        let messages = convert_messages(
            &model,
            &normalize_context(json(j!({"messages":[a]}))),
            &compat,
            None,
            env().as_ref(),
        )
        .unwrap();
        let messages = observed_json(&messages);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["reasoning_content"], "think");
        assert!(messages[0].get("reasoning").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sends_thinking_disabled_for_open_code_kimi_k2_6_when_thinking_is_off() {
        let params = capture(
            fixture_model("opencode", "kimi-k2.6"),
            context("Hi"),
            j!({}),
        )
        .await;
        assert_eq!(params["thinking"], j!({"type":"disabled"}));
        assert!(params.get("reasoning_effort").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sends_thinking_enabled_for_open_code_kimi_k2_6_when_thinking_is_enabled() {
        let params = capture(
            fixture_model("opencode", "kimi-k2.6"),
            context("Hi"),
            j!({"reasoning":"high"}),
        )
        .await;
        assert_eq!(params["thinking"], j!({"type":"enabled"}));
        assert!(params.get("reasoning_effort").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_disabled_thinking_for_moonshot_kimi_k2_7_code_models() {
        for provider in ["moonshotai", "moonshotai-cn"] {
            let params = capture(
                fixture_model(provider, "kimi-k2.7-code"),
                context("Hi"),
                j!({}),
            )
            .await;
            assert!(params.get("thinking").is_none());
            assert!(params.get("reasoning_effort").is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keeps_disabled_thinking_for_moonshot_kimi_k2_6_when_thinking_is_off() {
        let params = capture(
            fixture_model("moonshotai-cn", "kimi-k2.6"),
            context("Hi"),
            j!({}),
        )
        .await;
        assert_eq!(params["thinking"], j!({"type":"disabled"}));
        assert!(params.get("reasoning_effort").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sends_max_tokens_for_open_code_completions_models() {
        for (provider, id) in [("opencode-go", "kimi-k3"), ("opencode", "kimi-k2.6")] {
            let model = fixture_model(provider, id);
            assert_eq!(metadata(&model)["compat"]["maxTokensField"], "max_tokens");
            let params = capture(model, context("Hi"), j!({"maxTokens":123})).await;
            assert_eq!(params["max_tokens"], 123);
            assert!(params.get("max_completion_tokens").is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sends_max_tokens_for_built_in_and_custom_deep_seek_api_models() {
        let mut custom = local_model("custom-deepseek-model", "Custom DeepSeek Model");
        custom.provider = "custom-deepseek".into();
        custom.base_url = "https://api.deepseek.com".into();
        let mut uppercase = custom.clone();
        uppercase.id = "custom-uppercase-deepseek-model".into();
        uppercase.name = "Custom Uppercase DeepSeek Model".into();
        uppercase.base_url = "https://API.DeepSeek.COM".into();
        let native = [
            fixture_model("deepseek", "deepseek-flash"),
            fixture_model("deepseek", "deepseek-v4-pro"),
        ];
        for model in &native {
            assert_eq!(metadata(model)["compat"]["maxTokensField"], "max_tokens");
        }
        for model in native.into_iter().chain([custom, uppercase]) {
            let params = capture(model, context("Hi"), j!({"maxTokens":123})).await;
            assert_eq!(params["max_tokens"], 123);
            assert!(params.get("max_completion_tokens").is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sends_max_tokens_for_z_ai_completions_models() {
        for id in ["glm-5-turbo", "glm-5.2"] {
            let model = fixture_model("zai", id);
            assert_eq!(metadata(&model)["compat"]["maxTokensField"], "max_tokens");
            let params = capture(model, context("Hi"), j!({"maxTokens":123})).await;
            assert_eq!(params["max_tokens"], 123);
            assert!(params.get("max_completion_tokens").is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_reasoning_effort_for_open_code_grok_build() {
        let params = capture(
            fixture_model("opencode", "grok-build-0.1"),
            context("Hi"),
            j!({"reasoning":"high"}),
        )
        .await;
        assert!(params.get("reasoning_effort").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn does_not_double_count_reasoning_tokens_in_completion_usage() {
        let (_,response,_) = execute(unconfigured_model("openai","gpt-4o-mini"),context("Use reasoning."),j!({}),vec![
            j!({"id":"chatcmpl-reasoning-usage","choices":[{"delta":{},"finish_reason":"stop"}],"usage":usage(10,33,0,21)})]).await;
        assert_eq!(response["usage"]["input"], 10);
        assert_eq!(response["usage"]["output"], 33);
        assert_eq!(response["usage"]["totalTokens"], 43);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_prompt_tokens_details_cache_read_write_fields_from_chunk_usage() {
        let mut u = usage(100, 5, 50, 0);
        u["prompt_tokens_details"]["cache_write_tokens"] = j!(30);
        let (_,response,_) = execute(unconfigured_model("openai","gpt-4o-mini"),context("Reply with exactly OK"),j!({}),vec![
            j!({"id":"chatcmpl-cache-write","choices":[{"delta":{"content":"OK"},"finish_reason":null}]}),
            j!({"id":"chatcmpl-cache-write","choices":[{"delta":{},"finish_reason":"stop"}],"usage":u})]).await;
        assert_eq!(response["usage"]["input"], 20);
        assert_eq!(response["usage"]["cacheRead"], 50);
        assert_eq!(response["usage"]["cacheWrite"], 30);
        assert_eq!(response["usage"]["totalTokens"], 105);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_prompt_tokens_details_cache_read_write_fields_from_choice_usage_fallback() {
        let mut u = usage(100, 5, 50, 0);
        u["prompt_tokens_details"]["cache_write_tokens"] = j!(30);
        let (_,response,_) = execute(unconfigured_model("openai","gpt-4o-mini"),context("Reply with exactly OK"),j!({}),vec![
            j!({"id":"chatcmpl-cache-write-choice","choices":[{"delta":{"content":"OK"},"finish_reason":null}]}),
            j!({"id":"chatcmpl-cache-write-choice","choices":[{"delta":{},"finish_reason":"stop","usage":u}]})]).await;
        assert_eq!(response["usage"]["input"], 20);
        assert_eq!(response["usage"]["cacheRead"], 50);
        assert_eq!(response["usage"]["cacheWrite"], 30);
        assert_eq!(response["usage"]["totalTokens"], 105);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uses_open_router_reasoning_object_instead_of_reasoning_effort() {
        let params = capture(
            fixture_model("openrouter", "deepseek/deepseek-r1"),
            context("Hi"),
            j!({"reasoning":"high"}),
        )
        .await;
        assert_eq!(params["reasoning"], j!({"effort":"high"}));
        assert!(params.get("reasoning_effort").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uses_configurable_chat_template_boolean_thinking_kwargs() {
        let mut model = local_model("deepseek-ai/DeepSeek-V3.1", "DeepSeek V3.1 via vLLM");
        model.compat = Some(json(
            j!({"thinkingFormat":"chat-template","supportsReasoningEffort":false,"chatTemplateKwargs":{"thinking":{"$var":"thinking.enabled"}}}),
        ));
        for (options, expected) in [(j!({"reasoning":"high"}), true), (j!({}), false)] {
            let params = capture(model.clone(), context("Hi"), options).await;
            assert_eq!(params["chat_template_kwargs"], j!({"thinking":expected}));
            assert!(params.get("thinking").is_none());
            assert!(params.get("reasoning_effort").is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uses_qwen_chat_template_thinking_kwargs() {
        let mut model = local_model("Qwen/Qwen3-Coder", "Qwen3 Coder via vLLM");
        model.compat = Some(json(
            j!({"thinkingFormat":"qwen-chat-template","supportsReasoningEffort":false}),
        ));
        for (options, expected) in [(j!({"reasoning":"high"}), true), (j!({}), false)] {
            let params = capture(model.clone(), context("Hi"), options).await;
            assert_eq!(
                params["chat_template_kwargs"],
                j!({"enable_thinking":expected,"preserve_thinking":true})
            );
            assert!(params.get("reasoning_effort").is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uses_configurable_chat_template_effort_kwargs_with_static_kwargs() {
        let mut model = local_model("unsloth/gpt-oss-120b-GGUF", "GPT OSS via vLLM");
        model.thinking_level_map = Some(json(j!({"xhigh":"max"})));
        model.compat = Some(json(
            j!({"thinkingFormat":"chat-template","supportsReasoningEffort":false,"chatTemplateKwargs":{"preserve_thinking":true,"reasoning_effort":{"$var":"thinking.effort","omitWhenOff":true}}}),
        ));
        let params = capture(model, context("Hi"), j!({"reasoning":"xhigh"})).await;
        assert_eq!(
            params["chat_template_kwargs"],
            j!({"preserve_thinking":true,"reasoning_effort":"max"})
        );
        assert!(params.get("reasoning_effort").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uses_ant_ling_compatibility_metadata() {
        let model = fixture_model("ant-ling", "Ring-2.6-1T");
        let m = metadata(&model);
        let compat = &m["compat"];
        for (key, value) in [
            ("supportsStore", j!(false)),
            ("supportsDeveloperRole", j!(false)),
            ("supportsReasoningEffort", j!(false)),
            ("maxTokensField", j!("max_tokens")),
            ("thinkingFormat", j!("ant-ling")),
            ("supportsLongCacheRetention", j!(false)),
        ] {
            assert_eq!(compat[key], value, "{key}");
        }
        assert_eq!(compat["supportsStrictMode"], true);
        assert!(
            compat
                .get("requiresReasoningContentOnAssistantMessages")
                .is_none()
        );
        let mut ctx = context("Hi");
        ctx["systemPrompt"] = j!("Follow instructions.");
        let params = capture(model,ctx,j!({"maxTokens":123,"reasoning":"high","cacheRetention":"long","sessionId":"ant-ling-session"})).await;
        assert_eq!(params["max_tokens"], 123);
        assert!(params.get("max_completion_tokens").is_none());
        assert_eq!(params["messages"][0]["role"], "system");
        assert_eq!(params["reasoning"], j!({"effort":"high"}));
        for key in [
            "reasoning_effort",
            "store",
            "prompt_cache_key",
            "prompt_cache_retention",
        ] {
            assert!(params.get(key).is_none(), "{key}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_ant_ling_reasoning_for_unmapped_direct_reasoning_efforts_and_non_reasoning_models()
     {
        let transport = Arc::new(ScriptedTransport::default());
        transport.push_response(
            ProviderResponse {
                status: 200,
                ..Default::default()
            },
            default_chunks().into_iter().map(|v| Ok(js(v))),
        );
        let options: OpenAICompletionsOptions =
            json(j!({"apiKey":"test","reasoningEffort":"medium"}));
        stream(
            fixture_model("ant-ling", "Ring-2.6-1T"),
            normalize_context(json(context("Hi"))),
            Some(options),
            transport.clone(),
            env(),
        )
        .unwrap()
        .result()
        .await;
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].params.get("reasoning").is_none());
        let params = capture(
            fixture_model("ant-ling", "Ling-2.6-flash"),
            context("Hi"),
            j!({"reasoning":"high"}),
        )
        .await;
        assert!(params.get("reasoning").is_none());
    }
}
