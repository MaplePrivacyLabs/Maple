use super::common::*;
use pi_ai::api::constrained_sampling::*;
use pi_ai::api::openai_completions::{ScriptedTransport, convert_tools, get_compat, stream};
use pi_ai::types::*;
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json};
use std::sync::Arc;

fn tool(sampling: Option<Value>) -> Tool {
    let mut value = json!({
        "name": "sample_tool", "description": "Sample tool",
        "parameters": {"type": "object", "properties": {"payload": {"type": "string"}},
            "required": ["payload"], "additionalProperties": false}
    });
    if let Some(sampling) = sampling {
        value["constrainedSampling"] = sampling;
    }
    json(value)
}

fn model() -> Model {
    json(
        json!({"id":"gpt-test", "name":"GPT Test", "api":"openai-completions",
        "provider":"openai", "baseUrl":"https://api.openai.com/v1", "reasoning":false,
        "input":["text","image"], "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
        "contextWindow":128000,"maxTokens":4096,"compat":{"supportsOpenAIGrammarTools":true,"supportsStrictMode":true}}),
    )
}

mod constrained_tool_sampling {
    use super::*;

    #[test]
    fn converts_supported_constraints_and_falls_back_when_unsupported() {
        let prefer = tool(Some(json!({"type":"json_schema","strict":"prefer"})));
        assert_eq!(
            resolve_json_schema_strict_sampling(&prefer, true, None).unwrap(),
            Some(true)
        );
        assert_eq!(
            get_json_schema_tool_parameters(&prefer, Some(true)).unwrap(),
            make_strict_json_schema(&prefer.parameters, None).unwrap()
        );

        let require = tool(Some(json!({"type":"json_schema","strict":"require"})));
        assert!(
            resolve_json_schema_strict_sampling(&require, false, None)
                .unwrap_err()
                .as_str()
                .unwrap()
                .contains("Tool \"sample_tool\" requires JSON-schema constrained sampling")
        );

        let grammar_tool = tool(Some(
            json!({"type":"grammar","variants":{"openai_lark":"start: /[a-z]+/"}}),
        ));
        let grammar = resolve_grammar_constrained_sampling(&grammar_tool, true)
            .unwrap()
            .unwrap();
        assert_eq!(grammar.format, "lark");
        assert_eq!(grammar.definition, JsString::from("start: /[a-z]+/"));
        assert_eq!(grammar.input_property, JsString::from("payload"));
        let invalid = tool(Some(json!({"type":"grammar","variants":{}})));
        assert!(resolve_grammar_constrained_sampling(&invalid, true).unwrap_err().as_str().unwrap()
            .contains("Tool \"sample_tool\" cannot use grammar constrained sampling: no supported grammar variant was provided"));

        assert_eq!(
            resolve_grammar_constrained_sampling(&grammar_tool, false).unwrap(),
            None
        );
        assert_eq!(
            resolve_json_schema_strict_sampling(&grammar_tool, false, None).unwrap(),
            None
        );
        assert_eq!(
            get_json_schema_tool_parameters(&grammar_tool, None).unwrap(),
            grammar_tool.parameters.schema
        );
        let mut compat = get_compat(&model());
        compat.supports_open_ai_grammar_tools = Some(false);
        compat.supports_strict_mode = Some(false);
        let fallback =
            serde_json::to_value(convert_tools(&[grammar_tool], &compat).unwrap()).unwrap();
        assert_eq!(fallback[0]["type"], "function");
        assert_eq!(fallback[0]["function"]["name"], "sample_tool");
        assert!(fallback[0]["function"].get("strict").is_none());

        let disabled = tool(Some(json!(false)));
        let default_tool = tool(None);
        assert_eq!(
            resolve_json_schema_strict_sampling(&disabled, true, None).unwrap(),
            resolve_json_schema_strict_sampling(&default_tool, true, None).unwrap()
        );
        assert_eq!(
            resolve_grammar_constrained_sampling(&disabled, true).unwrap(),
            resolve_grammar_constrained_sampling(&default_tool, true).unwrap()
        );
        assert_eq!(
            convert_tools(&[disabled], &get_compat(&model())).unwrap(),
            convert_tools(&[default_tool], &get_compat(&model())).unwrap()
        );
    }

    #[test]
    fn derives_strict_provider_schemas_without_changing_tool_definitions() {
        let parameters: Schema = json(json!({"type":"object","properties":{
            "path":{"type":"string"}, "offset":{"type":"number"},
            "metadata":{"type":"object","properties":{"enabled":{"type":"boolean"}}},
            "nullable":{"anyOf":[{"type":"string"},{"type":"null"}]}
        },"required":["path","metadata"]}));
        let original = parameters.schema.clone();
        let strict = make_strict_json_schema(&parameters, None).unwrap();
        assert!(parameters.schema.get("additionalProperties").is_none());
        assert_eq!(
            parameters.schema.get("required"),
            Some(&js(json!(["path", "metadata"])))
        );
        assert_eq!(parameters.schema, original);
        assert_eq!(
            strict,
            js(json!({"type":"object","additionalProperties":false,
                "required":["path","offset","metadata","nullable"],"properties":{
                "path":{"type":"string"},
                "offset":{"anyOf":[{"type":"number"},{"type":"null"}]},
                "metadata":{"type":"object","additionalProperties":false,"required":["enabled"],
                    "properties":{"enabled":{"anyOf":[{"type":"boolean"},{"type":"null"}]}}},
                "nullable":{"anyOf":[{"type":"string"},{"type":"null"}]}
            }}))
        );
    }

    #[test]
    fn falls_back_or_rejects_schemas_that_cannot_be_safely_converted() {
        let cases = [
            (
                json!({"type":"object","properties":{"metadata":{"type":"object","properties":{},"additionalProperties":{"type":"string"}}},"required":["metadata"]}),
                "additionalProperties is unsupported",
            ),
            (
                json!({"type":"object","allOf":[{"type":"object","properties":{"a":{"type":"string"}},"required":["a"]},{"type":"object","properties":{"b":{"type":"number"}},"required":["b"]}]}),
                "allOf schemas are unsupported",
            ),
            (
                json!({"type":"object","properties":{"value":{"anyOf":[{"type":"object","properties":{"nested":{"type":"string"}},"required":["nested"]},{"type":"null"}]}},"required":["value"]}),
                "object and array unions are unsupported",
            ),
            (
                json!({"type":"object","properties":{"child":{"$ref":"https://example.com/child.json"}},"required":["child"]}),
                "$ref schemas are unsupported",
            ),
        ];
        for (parameters, error) in cases {
            let mut constrained = tool(Some(json!({"type":"json_schema","strict":"prefer"})));
            constrained.parameters = json(parameters.clone());
            assert!(
                make_strict_json_schema(&constrained.parameters, None)
                    .unwrap_err()
                    .as_str()
                    .unwrap()
                    .contains(error)
            );
            assert_eq!(
                resolve_json_schema_strict_sampling(&constrained, true, None).unwrap(),
                None
            );
            let converted = serde_json::to_value(
                convert_tools(&[constrained.clone()], &get_compat(&model())).unwrap(),
            )
            .unwrap();
            assert_eq!(converted[0]["function"]["strict"], false);
            assert_eq!(converted[0]["function"]["parameters"], parameters);
            constrained.constrained_sampling =
                Some(json(json!({"type":"json_schema","strict":"require"})));
            assert!(
                resolve_json_schema_strict_sampling(&constrained, true, None)
                    .unwrap_err()
                    .as_str()
                    .unwrap()
                    .contains(error)
            );
        }
    }

    #[test]
    fn replays_grammar_calls_as_custom_responses_items() {
        for arguments in [json!({}), json!({"payload":42})] {
            assert!(get_grammar_tool_input(&"sample_tool".into(), &js(arguments), &"payload".into())
                .unwrap_err().as_str().unwrap().contains("Grammar tool call \"sample_tool\" requires argument \"payload\" to be a string"));
        }
        assert_eq!(
            get_grammar_tool_input(
                &"sample_tool".into(),
                &js(json!({"payload":"abc"})),
                &"payload".into()
            )
            .unwrap(),
            JsString::from("abc")
        );
    }

    #[test]
    fn keeps_grammar_input_json_deltas_append_only() {
        let mut buffer = GrammarToolInputJsonBuffer::default();
        let first = append_grammar_tool_input_json_delta(
            &mut buffer,
            &"payload".into(),
            &"a\"".into(),
            false,
        )
        .unwrap()
        .unwrap()
        .to_string_lossy();
        let second = append_grammar_tool_input_json_delta(
            &mut buffer,
            &"payload".into(),
            &"a\"\nb".into(),
            true,
        )
        .unwrap()
        .unwrap()
        .to_string_lossy();
        assert_eq!(
            serde_json::from_str::<Value>(&format!("{first}{second}")).unwrap(),
            json!({"payload":"a\"\nb"})
        );
        assert_eq!(
            append_grammar_tool_input_json_delta(
                &mut buffer,
                &"payload".into(),
                &"a\"\nb".into(),
                true
            )
            .unwrap(),
            None
        );
        assert!(
            append_grammar_tool_input_json_delta(
                &mut buffer,
                &"payload".into(),
                &"changed".into(),
                true
            )
            .unwrap_err()
            .as_str()
            .unwrap()
            .contains("grammar tool input for property \"payload\" changed after it was closed")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn starts_custom_responses_tool_calls_with_their_initial_input() {
        // Preserve the selected initial-input/buffer semantics independently of
        // Responses' excluded output_item.added event and composite item ID.
        let mut buffer = GrammarToolInputJsonBuffer::default();
        let first = append_grammar_tool_input_json_delta(
            &mut buffer,
            &"payload".into(),
            &"a".into(),
            false,
        )
        .unwrap()
        .unwrap()
        .to_string_lossy();
        assert_eq!(buffer.input, JsString::from("a"));
        assert_eq!(
            serde_json::from_str::<Value>(&format!("{first}\"}}")).unwrap(),
            json!({"payload":"a"})
        );
        let second = append_grammar_tool_input_json_delta(
            &mut buffer,
            &"payload".into(),
            &"ab".into(),
            false,
        )
        .unwrap()
        .unwrap()
        .to_string_lossy();
        let third = append_grammar_tool_input_json_delta(
            &mut buffer,
            &"payload".into(),
            &"abc".into(),
            true,
        )
        .unwrap()
        .unwrap()
        .to_string_lossy();
        assert_eq!(
            serde_json::from_str::<Value>(&format!("{first}{second}{third}")).unwrap(),
            json!({"payload":"abc"})
        );

        let transport = Arc::new(ScriptedTransport::default());
        transport.push_response(ProviderResponse { status: 200, ..Default::default() }, [
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"custom","custom":{"name":"sample_tool","input":"a"}}]},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"custom":{"input":"b"}}]},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"custom":{"input":"c"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
        ].into_iter().map(|chunk| Ok(js(chunk))));
        let context = normalize_context(Context {
            messages: vec![],
            tools: Some(vec![tool(Some(
                json!({"type":"grammar","variants":{"openai_lark":"start: /[a-z]+/"}}),
            ))]),
            ..Default::default()
        });
        let mut events = stream(model(), context, None, transport, env()).unwrap();
        let output = events.result().await;
        let mut starts = 0;
        let mut deltas = String::new();
        while let Some(event) = events.next().await {
            match event {
                AssistantMessageEvent::ToolcallStart { .. } => starts += 1,
                AssistantMessageEvent::ToolcallDelta { delta, .. } => {
                    deltas.push_str(&delta.to_string_lossy())
                }
                _ => {}
            }
        }
        assert_eq!(starts, 1);
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        assert_eq!(
            serde_json::to_value(&output.content).unwrap(),
            json!([
                {"type":"toolCall","id":"call_1","name":"sample_tool","arguments":{"payload":"abc"}}
            ])
        );
        assert_eq!(
            serde_json::from_str::<Value>(&deltas).unwrap(),
            json!({"payload":"abc"})
        );
    }
}
