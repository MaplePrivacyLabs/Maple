use pi_ai::types::*;
use pi_ai::utils::text::{get_system_message_text, render_system_message_update};
use pi_ai::utils::transcript::*;
use serde_json::{Value, json};

fn tool(name: &str) -> Tool {
    described_tool(name, &format!("{name} tool"))
}
fn described_tool(name: &str, description: &str) -> Tool {
    Tool {
        name: name.into(),
        description: description.into(),
        parameters: Schema::typebox(json!({"type":"object","properties":{}})),
        ..Tool::default()
    }
}
fn message(value: Value) -> Message {
    serde_json::from_value(value).expect("test message")
}
fn user(text: &str, timestamp: f64) -> Message {
    UserMessage {
        content: UserMessageContent::Text(text.into()),
        timestamp,
        ..UserMessage::default()
    }
    .into()
}
fn transcript() -> TranscriptContext {
    normalize_context(Context {
        messages: vec![
            message(
                json!({"role":"system","content":"base","sections":{"a":"<a>1</a>","b":"<b>1</b>"},"toolsAdded":[tool("first")],"timestamp":10}),
            ),
            user("hello", 11.0),
            message(json!({"role":"system","content":"also do this","timestamp":12})),
            AssistantMessage {
                content: vec![TextContent::new("ok").into()],
                timestamp: 13.0,
                ..AssistantMessage::default()
            }
            .into(),
            message(
                json!({"role":"system","content":"","sections":{"a":"<a>2</a>","b":null,"c":"<c>1</c>"},"toolsRemoved":[{"name":"first"}],"toolsAdded":[tool("second")],"timestamp":14}),
            ),
        ],
        ..Context::default()
    })
}

#[allow(clippy::module_inception)] // Preserve the upstream describe path.
mod system_message_replay {
    use super::*;

    #[test]
    fn replays_content_sections_and_tools_into_one_leading_message() {
        let transcript = transcript();
        let current = get_current_system_message(&transcript.messages)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(current).unwrap(),
            json!({"role":"system","content":"base\n\nalso do this","sections":{"a":"<a>2</a>","c":"<c>1</c>"},"toolsAdded":[tool("second")],"timestamp":10.0})
        );
        assert_eq!(
            get_current_system_prompt(&transcript.messages).unwrap(),
            "base\n\nalso do this\n\n<a>2</a>\n\n<c>1</c>"
        );
    }
    #[test]
    fn collapse_keeps_only_non_system_messages_after_the_replayed_head() {
        let collapsed = collapse_system_messages(transcript()).unwrap();
        assert_eq!(
            collapsed
                .messages
                .iter()
                .map(Message::role)
                .collect::<Vec<_>>(),
            ["system", "user", "assistant"]
        );
        assert_eq!(
            collapse_system_messages(collapsed.clone()).unwrap(),
            collapsed
        );
    }
    #[test]
    fn replay_of_a_transcript_without_system_messages_is_empty() {
        let context = normalize_context(Context {
            messages: vec![user("hi", 1.0)],
            ..Context::default()
        });
        assert!(
            get_current_system_message(&context.messages)
                .unwrap()
                .is_none()
        );
        assert_eq!(get_current_system_prompt(&context.messages).unwrap(), "");
        assert_eq!(
            collapse_system_messages(context.clone()).unwrap().messages,
            context.messages
        );
    }
    #[test]
    fn a_late_full_patch_on_a_transcript_without_a_leading_message_replays_as_the_prompt() {
        let context = normalize_context(Context {
            messages: vec![
                user("old session", 1.0),
                message(
                    json!({"role":"system","content":"","sections":{"preamble":"You are pi."},"toolsAdded":[tool("x")],"timestamp":2}),
                ),
            ],
            ..Context::default()
        });
        assert_eq!(
            get_current_system_prompt(&context.messages).unwrap(),
            "You are pi."
        );
        let collapsed = collapse_system_messages(context).unwrap();
        let head = get_initial_system_message(&collapsed.messages).unwrap();
        assert_eq!(
            serde_json::to_value(system_tools_added(head).unwrap()).unwrap(),
            json!([tool("x")])
        );
    }
    #[test]
    fn renders_complete_prompts_and_framed_updates() {
        let transcript = transcript();
        let Message::System(leading) = &transcript.messages[0] else {
            panic!("expected system messages")
        };
        let Message::System(update) = &transcript.messages[4] else {
            panic!("expected system messages")
        };
        assert_eq!(
            get_system_message_text(leading),
            "base\n\n<a>1</a>\n\n<b>1</b>"
        );
        assert_eq!(
            render_system_message_update(update),
            [
                "Updated system prompt section \"a\":\n\n<a>2</a>",
                "Removed system prompt section \"b\".",
                "Updated system prompt section \"c\":\n\n<c>1</c>"
            ]
            .join("\n\n")
        );
    }
    #[test]
    fn normalizes_the_legacy_prompt_and_tool_fields_into_a_leading_system_message() {
        let messages = vec![user("hi", 1.0)];
        assert_eq!(
            normalize_context(Context {
                messages: messages.clone(),
                ..Context::default()
            })
            .messages,
            messages
        );
        assert_eq!(
            normalize_context(Context {
                system_prompt: Some(JsString::default()),
                tools: Some(vec![]),
                messages: messages.clone()
            })
            .messages,
            messages
        );
        let context = normalize_context(Context {
            system_prompt: Some("be brief".into()),
            tools: Some(vec![tool("a")]),
            messages: messages.clone(),
        });
        assert_eq!(
            context.messages,
            vec![
                SystemMessage {
                    content: SystemContent::Text("be brief".into()),
                    tools_added: Some(vec![tool("a")]),
                    ..SystemMessage::default()
                }
                .into(),
                messages[0].clone()
            ]
        );
    }
    #[test]
    fn compares_tool_declarations_without_executable_or_undefined_fields() {
        // Executable closures live outside the serializable Rust Tool contract;
        // TypeBox's runtime identity is the remaining non-JSON declaration field.
        let executable = tool("a");
        assert!(declarations_equal(&executable, &tool("a")));
        assert!(declarations_equal(
            &executable,
            &to_tool_declaration(&tool("a"))
        ));
        assert!(!declarations_equal(
            &tool("a"),
            &described_tool("a", "changed")
        ));
        assert!(!declarations_equal(
            &tool("a"),
            &Tool {
                constrained_sampling: Some(ConstrainedSampling::Disabled(false)),
                ..tool("a")
            }
        ));
    }
    #[test]
    fn tool_state_changes_treat_changed_definitions_as_removal_plus_addition() {
        let changes = get_tool_state_changes(
            &[tool("a"), tool("b")],
            &[described_tool("b", "changed"), tool("c")],
        );
        assert_eq!(
            serde_json::to_value(changes.tools_added).unwrap(),
            json!([described_tool("b", "changed"), tool("c")])
        );
        assert_eq!(
            changes.tools_removed,
            [
                ToolReference { name: "a".into() },
                ToolReference { name: "b".into() }
            ]
        );
        assert_eq!(
            get_tool_state_changes(&[tool("a")], &[tool("a")]),
            ToolStateChanges::default()
        );
    }
    #[test]
    fn detects_non_additive_tool_history_and_redefinitions() {
        let transcript = transcript();
        assert!(has_non_additive_tool_changes(&transcript.messages).unwrap());
        assert!(!has_tool_redefinitions(&transcript.messages).unwrap());
        let additive = normalize_context(Context {
            messages: vec![
                message(
                    json!({"role":"system","content":"","toolsAdded":[tool("a")],"timestamp":1}),
                ),
                message(
                    json!({"role":"system","content":"","toolsAdded":[tool("b")],"timestamp":2}),
                ),
            ],
            ..Context::default()
        });
        assert!(!has_non_additive_tool_changes(&additive.messages).unwrap());
        let redeclared = normalize_context(Context {
            messages: vec![
                message(
                    json!({"role":"system","content":"","toolsAdded":[tool("a")],"timestamp":1}),
                ),
                message(
                    json!({"role":"system","content":"","toolsAdded":[described_tool("a", "changed")],"timestamp":2}),
                ),
            ],
            ..Context::default()
        });
        assert!(has_non_additive_tool_changes(&redeclared.messages).unwrap());
        assert!(has_tool_redefinitions(&redeclared.messages).unwrap());
    }
}

mod rust_adaptations {
    use super::*;
    use pi_ai::utils::estimate::estimate_message_tokens;
    use pi_ai::utils::js_json::stringify;
    use pi_ai::utils::js_value::to_js_value;

    #[test]
    fn malformed_tool_names_and_system_section_keys_keep_distinct_identity() {
        let a = JsString::from_utf16(vec![0xd800]);
        let b = JsString::from_utf16(vec![0xd801]);
        let first = Tool {
            name: a.clone(),
            ..tool("first")
        };
        let second = Tool {
            name: b.clone(),
            ..tool("second")
        };
        let changes = get_tool_state_changes(
            &[first.clone(), second.clone()],
            std::slice::from_ref(&first),
        );
        assert!(changes.tools_added.is_empty());
        assert_eq!(changes.tools_removed, [ToolReference { name: b.clone() }]);
        let context = normalize_context(Context {
            messages: vec![
                SystemMessage {
                    sections: Some(
                        [
                            (a.clone(), Some("first".into())),
                            (b.clone(), Some("second".into())),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                    tools_added: Some(vec![first, second.clone()]),
                    ..Default::default()
                }
                .into(),
                SystemMessage {
                    sections: Some([(a.clone(), None)].into_iter().collect()),
                    tools_removed: Some(vec![ToolReference { name: a }]),
                    ..Default::default()
                }
                .into(),
            ],
            ..Default::default()
        });
        assert_eq!(get_current_tools(&context.messages).unwrap(), [second]);
        let current = get_current_system_message(&context.messages)
            .unwrap()
            .unwrap();
        let Message::System(current) = current else {
            panic!("typed system")
        };
        assert_eq!(current.sections.unwrap().keys().collect::<Vec<_>>(), [&b]);
        assert_eq!(
            get_current_system_prompt(&context.messages).unwrap(),
            "second"
        );
    }

    #[test]
    fn constrained_sampling_object_order_is_observable_in_declaration_equality() {
        let config = |keys: [&str; 2]| {
            ConstrainedSampling::Config(ConstrainedSamplingConfig::from_raw(
                keys.into_iter()
                    .map(|key| {
                        (
                            key,
                            if key == "type" {
                                JsValue::from("jsonSchema")
                            } else {
                                JsValue::Bool(true)
                            },
                        )
                    })
                    .collect(),
            ))
        };
        let left = Tool {
            constrained_sampling: Some(config(["type", "strict"])),
            ..tool("a")
        };
        let right = Tool {
            constrained_sampling: Some(config(["strict", "type"])),
            ..tool("a")
        };
        assert!(!declarations_equal(&left, &right));
        let changes = get_tool_state_changes(&[left], std::slice::from_ref(&right));
        assert_eq!(changes.tools_added, [to_tool_declaration(&right)]);
        assert_eq!(changes.tools_removed, [ToolReference { name: "a".into() }]);
    }

    #[test]
    fn transcript_replay_and_estimates_preserve_lone_utf16_units() {
        let lone = JsString::from_utf16(vec![0xd800]);
        let context = normalize_context(Context {
            system_prompt: Some(lone.clone()),
            messages: vec![
                SystemMessage {
                    content: SystemContent::Text("tail".into()),
                    ..Default::default()
                }
                .into(),
            ],
            ..Default::default()
        });
        let prompt = get_current_system_prompt(&context.messages).unwrap();
        assert_eq!(prompt.as_utf16(), [0xd800, 10, 10, 116, 97, 105, 108]);
        let leading = get_current_system_message(&context.messages)
            .unwrap()
            .unwrap();
        assert_eq!(estimate_message_tokens(&leading).unwrap(), 2.0);
        let encoded = stringify(&to_js_value(&leading).unwrap());
        assert!(encoded.contains(r"\ud800\n\ntail"));
        let changed = Tool {
            description: lone,
            ..tool("a")
        };
        assert!(declarations_equal(&changed, &to_tool_declaration(&changed)));
    }
}

mod raw_message_contracts {
    use super::*;
    use pi_ai::utils::estimate::{estimate_context_tokens, estimate_message_tokens};
    use pi_ai::utils::js_value::to_js_value;

    #[test]
    fn raw_declarations_replay_without_reading_or_rewriting_missing_content() {
        let raw = message(
            json!({"role":"system","toolsAdded":[to_tool_declaration(&tool("read"))],"opaque":{"keep":true}}),
        );
        let before = to_js_value(&raw).unwrap();
        let messages = vec![raw.clone()];
        assert!(std::ptr::eq(
            get_initial_system_message(&messages).unwrap(),
            &messages[0]
        ));
        assert!(without_initial_system_message(&messages).is_empty());
        assert_eq!(
            get_current_tools(&messages).unwrap(),
            [to_tool_declaration(&tool("read"))]
        );
        assert_eq!(
            get_declared_tools(&messages).unwrap(),
            [to_tool_declaration(&tool("read"))]
        );
        assert!(!has_non_additive_tool_changes(&messages).unwrap());
        assert_eq!(
            resolve_transcript_tools(&messages, true)
                .unwrap()
                .request_tools,
            [to_tool_declaration(&tool("read"))]
        );
        assert_eq!(
            get_current_system_message(&messages).unwrap_err(),
            "Cannot read properties of undefined (reading 'filter')"
        );
        assert_eq!(
            estimate_message_tokens(&raw).unwrap_err(),
            "Cannot read properties of undefined (reading 'filter')"
        );
        assert_eq!(to_js_value(&raw).unwrap(), before);
        let Message::Raw(original) = &raw else {
            panic!("raw system declaration")
        };
        let Message::Raw(alias) = &messages[0] else {
            panic!("raw alias")
        };
        assert!(original.ptr_eq(alias));
        original.update(|object| {
            object.insert(
                "toolsRemoved",
                JsValue::from_json_with_js_numbers(json!([{"name":"read"}])),
            );
            object.remove("toolsAdded");
        });
        assert!(get_current_tools(&messages).unwrap().is_empty());
        assert!(has_non_additive_tool_changes(&messages).unwrap());
    }

    #[test]
    fn raw_system_replay_retains_null_versus_missing_timestamp() {
        let missing = message(json!({"role":"system","content":"base"}));
        assert!(
            get_current_system_message(std::slice::from_ref(&missing))
                .unwrap()
                .is_none()
        );
        assert_eq!(estimate_message_tokens(&missing).unwrap(), 1.0);
        let null = message(json!({"role":"system","content":"base","timestamp":null}));
        assert_eq!(
            get_current_system_message(&[null])
                .unwrap()
                .unwrap()
                .timestamp(),
            0.0
        );
        let tools = message(
            json!({"role":"system","content":"base","toolsAdded":[to_tool_declaration(&tool("read"))]}),
        );
        let current = get_current_system_message(&[tools]).unwrap().unwrap();
        assert_eq!(current.timestamp(), 0.0);
        assert_eq!(get_message_system_text(&current).unwrap(), "base");
        for (content, expected) in [
            (
                Some(Value::Null),
                "Cannot read properties of null (reading 'filter')",
            ),
            (
                None,
                "Cannot read properties of undefined (reading 'filter')",
            ),
        ] {
            let mut value = json!({"role":"system","timestamp":3});
            if let Some(content) = content {
                value["content"] = content;
            }
            let raw = message(value);
            assert_eq!(
                get_current_system_message(std::slice::from_ref(&raw)).unwrap_err(),
                expected
            );
            assert_eq!(estimate_message_tokens(&raw).unwrap_err(), expected);
        }
    }

    #[test]
    fn raw_system_sections_retain_object_entries_and_interpolation_semantics() {
        for sections in [json!(42), json!(true), Value::Null] {
            let raw = message(
                json!({"role":"system","content":"base","sections":sections,"timestamp":1}),
            );
            let current = get_current_system_message(&[raw]).unwrap().unwrap();
            assert_eq!(
                serde_json::to_value(&current).unwrap(),
                json!({"role":"system","content":"base","timestamp":1.0})
            );
        }
        let raw = message(
            json!({"role":"system","content":"base","sections":{"number":42,"list":["a","b"],"gone":null},"timestamp":"legacy"}),
        );
        let current = get_current_system_message(std::slice::from_ref(&raw))
            .unwrap()
            .unwrap();
        assert!(matches!(current, Message::Raw(_)));
        assert_eq!(
            serde_json::to_value(&current).unwrap(),
            json!({"role":"system","content":"base","sections":{"number":42.0,"list":["a","b"]},"timestamp":"legacy"})
        );
        assert_eq!(get_message_system_text(&current).unwrap(), "base\n\na,b");
        assert_eq!(
            render_message_system_update(&raw).unwrap(),
            "base\n\nUpdated system prompt section \"number\":\n\n42\n\nUpdated system prompt section \"list\":\n\na,b\n\nRemoved system prompt section \"gone\"."
        );
        let raw = message(json!({"role":"system","content":"","sections":"ab","timestamp":1}));
        assert_eq!(get_current_system_prompt(&[raw]).unwrap(), "a\n\nb");
    }

    #[test]
    fn raw_estimation_reads_content_and_usage_without_requiring_other_metadata() {
        for raw in [
            json!({"role":"user","content":"abcd"}),
            json!({"role":"assistant","content":[{"type":"text","text":"abcd"}]}),
        ] {
            let raw = message(raw);
            assert_eq!(estimate_message_tokens(&raw).unwrap(), 1.0);
            assert_eq!(estimate_context_tokens([raw]).unwrap().tokens, 1.0);
        }
        assert_eq!(
            estimate_message_tokens(&message(
                json!({"role":"user","content":null,"timestamp":3})
            ))
            .unwrap_err(),
            "content is not iterable"
        );
        assert_eq!(
            estimate_context_tokens([message(
                json!({"role":"assistant","content":[],"timestamp":1})
            )])
            .unwrap_err(),
            "Cannot read properties of undefined (reading 'totalTokens')"
        );
        let null_prefix = message(json!({"role":"user","content":"abcd","timestamp":null}));
        let anchor = message(
            json!({"role":"assistant","content":[],"usage":{"totalTokens":9},"timestamp":1}),
        );
        assert_eq!(
            estimate_context_tokens([null_prefix, anchor])
                .unwrap()
                .last_usage_index,
            Some(1)
        );
        let usage = estimate_context_tokens([message(
            json!({"role":"assistant","content":[],"usage":{"totalTokens":9},"timestamp":1}),
        )])
        .unwrap();
        assert_eq!(
            (usage.tokens, usage.usage_tokens, usage.last_usage_index),
            (9.0, 9.0, Some(0))
        );
        assert_eq!(
            estimate_message_tokens(&message(
                json!({"role":"custom","content":[],"timestamp":1})
            ))
            .unwrap(),
            0.0
        );
    }
}
