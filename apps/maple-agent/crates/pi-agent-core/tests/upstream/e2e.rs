//! One-for-one translation of packages/agent/test/e2e.test.ts (10 cases).
//! Faux registration is scoped to an explicit handle; time and randomness use VirtualEnv.

use std::sync::Arc;

use pi_agent_core::agent::{Agent, AgentInitialState, AgentOptions};
use pi_agent_core::types::*;
use pi_ai::providers::faux::{
    FauxAssistantOptions, FauxModelDefinition, FauxProviderHandle, FauxResponseStep, FauxTokenSize,
    RegisterFauxProviderOptions, create_faux_core, faux_assistant_message, faux_text,
    faux_thinking, faux_tool_call,
};
use pi_ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Message, Schema, StopReason,
    TextContent, Tool, ToolResultMessage, UserContent, UserMessage, UserMessageContent,
};
use pi_testkit::VirtualEnv;
use serde_json::json;

const NOW: i64 = 1_767_225_600_000;

fn setup(options: RegisterFauxProviderOptions) -> (Arc<VirtualEnv>, FauxProviderHandle) {
    let env = Arc::new(VirtualEnv::new(NOW));
    let faux = create_faux_core(env.clone(), options);
    (env, faux)
}

fn text_response(env: &VirtualEnv, text: &str) -> FauxResponseStep {
    faux_assistant_message(env, text, FauxAssistantOptions::default()).into()
}

fn new_agent(
    env: Arc<VirtualEnv>,
    faux: &FauxProviderHandle,
    system_prompt: &str,
    thinking_level: Option<ThinkingLevel>,
    tools: Option<Vec<AgentTool>>,
) -> Agent {
    Agent::new(
        AgentOptions {
            stream_fn: Some(synchronous_stream(faux.stream_function())),
            initial_state: Some(AgentInitialState {
                system_prompt: Some(system_prompt.into()),
                model: Some(Shared::new(faux.get_model().clone())),
                thinking_level,
                tools: tools.map(|tools| {
                    tools
                        .into_iter()
                        .map(Shared::new)
                        .collect::<Vec<_>>()
                        .into()
                }),
                ..AgentInitialState::default()
            }),
            ..AgentOptions::default()
        },
        env,
    )
    .expect("construct agent")
}

fn get_text_content(message: &AgentMessage) -> String {
    let texts: Vec<JsString> = match message.as_llm().expect("LLM message") {
        Message::Assistant(message) => message
            .content
            .into_iter()
            .filter_map(|block| match block {
                AssistantContent::Text(block) => Some(block.text),
                _ => None,
            })
            .collect(),
        Message::ToolResult(message) => message
            .content
            .into_iter()
            .filter_map(|block| match block {
                UserContent::Text(block) => Some(block.text),
                _ => None,
            })
            .collect(),
        _ => panic!("Expected assistant or tool result message"),
    };
    texts
        .iter()
        .map(JsString::to_string_lossy)
        .collect::<Vec<_>>()
        .join("\n")
}

fn calculate_tool() -> AgentTool {
    AgentTool {
        tool: Tool {
            name: "calculate".into(),
            description: "Evaluate mathematical expressions".into(),
            parameters: Schema::typebox(json!({
                "type": "object",
                "properties": {"expression": {"type": "string", "description": "The mathematical expression to evaluate"}},
                "required": ["expression"]
            })),
            ..Tool::default()
        },
        label: "Calculator".into(),
        prepare_arguments: None,
        output_schema: None,
        execute: Arc::new(|_, args, _, _| {
            Box::pin(async move {
                let args = args.snapshot();
                let expression = args
                    .get("expression")
                    .and_then(JsValue::as_str)
                    .ok_or_else(|| AgentError::new("Expected expression string"))?;
                // The source helper evaluates JavaScript. These fixtures only need
                // numeric binary arithmetic; calculate from the actual tool args.
                let parts = expression.split_whitespace().collect::<Vec<_>>();
                let [left, operator, right] = parts.as_slice() else {
                    return Err(AgentError::new("Expected binary arithmetic expression"));
                };
                let left: f64 = left
                    .parse()
                    .map_err(|_| AgentError::new("Expected numeric operand"))?;
                let right: f64 = right
                    .parse()
                    .map_err(|_| AgentError::new("Expected numeric operand"))?;
                let result = match *operator {
                    "+" => left + right,
                    "-" => left - right,
                    "*" => left * right,
                    "/" => left / right,
                    _ => return Err(AgentError::new("Unsupported arithmetic operator")),
                };
                Ok(AgentToolResult {
                    content: Some(vec![
                        TextContent::new(format!("{expression} = {result}")).into(),
                    ]),
                    ..AgentToolResult::default()
                })
            })
        }),
        replay: None,
        execution_mode: None,
    }
}

mod agent_integration_with_faux_provider {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn handles_a_basic_text_prompt() {
        let (env, faux) = setup(RegisterFauxProviderOptions::default());
        faux.set_responses(vec![text_response(&env, "4")]);
        let agent = new_agent(
            env,
            &faux,
            "You are a helpful assistant. Keep your responses concise.",
            Some(ThinkingLevel::Off),
            Some(vec![]),
        );
        agent
            .prompt_text("What is 2+2? Answer with just the number.", None)
            .await
            .unwrap();

        let state = agent.state();
        assert!(!state.is_streaming());
        let messages = state.messages().snapshot();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role(), "system");
        assert_eq!(messages[1].role(), "user");
        assert_eq!(messages[2].role(), "assistant");
        assert!(get_text_content(&messages[2]).contains('4'));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executes_tools_and_tracks_pending_tool_calls() {
        let (env, faux) = setup(RegisterFauxProviderOptions::default());
        faux.set_responses(vec![
            faux_assistant_message(
                env.as_ref(),
                vec![
                    faux_text("Let me calculate that.").into(),
                    faux_tool_call(
                        env.as_ref(),
                        "calculate",
                        JsValue::from_json_with_js_numbers(json!({"expression": "123 * 456"})),
                        Some("calc-1".into()),
                    )
                    .into(),
                ],
                FauxAssistantOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into(),
            text_response(&env, "The result is 56088."),
        ]);
        let agent = new_agent(
            env,
            &faux,
            "You are a helpful assistant. Always use the calculator tool for math.",
            Some(ThinkingLevel::Off),
            Some(vec![calculate_tool()]),
        );
        let pending_tool_calls_during_events = Shared::new(Vec::new());
        let captured = pending_tool_calls_during_events.clone();
        let observed_state = agent.state();
        let unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            if matches!(
                event,
                AgentEvent::ToolExecutionStart { .. } | AgentEvent::ToolExecutionEnd { .. }
            ) {
                captured.push((
                    event.kind(),
                    observed_state
                        .pending_tool_calls()
                        .read(|ids| ids.iter().cloned().collect::<Vec<_>>()),
                ));
            }
            Box::pin(async { Ok(()) })
        }));
        agent
            .prompt_text("Calculate 123 * 456 using the calculator tool.", None)
            .await
            .unwrap();
        unsubscribe();

        let state = agent.state();
        assert!(!state.is_streaming());
        let messages = state.messages().snapshot();
        assert!(messages.len() >= 4);
        let tool_result = messages
            .iter()
            .find(|message| message.role() == "toolResult");
        assert!(tool_result.is_some());
        assert!(get_text_content(tool_result.unwrap()).contains("123 * 456 = 56088"));
        let final_message = messages.last().unwrap();
        assert_eq!(final_message.role(), "assistant");
        assert!(get_text_content(final_message).contains("56088"));
        assert!(state.pending_tool_calls().read(|calls| calls.is_empty()));
        assert_eq!(
            pending_tool_calls_during_events.snapshot(),
            vec![
                ("tool_execution_start", vec![JsString::from("calc-1")]),
                ("tool_execution_end", vec![]),
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handles_abort_during_streaming() {
        let (env, faux) = setup(RegisterFauxProviderOptions {
            tokens_per_second: Some(20.0),
            token_size: Some(FauxTokenSize {
                min: Some(2.0),
                max: Some(2.0),
            }),
            ..Default::default()
        });
        faux.set_responses(vec![text_response(&env,
            "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen")]);
        let agent = new_agent(
            env.clone(),
            &faux,
            "You are a helpful assistant.",
            Some(ThinkingLevel::Off),
            Some(vec![]),
        );
        let started = CancellationToken::new();
        let observed = started.clone();
        let unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            if matches!(
                event,
                AgentEvent::MessageUpdate {
                    assistant_message_event: AssistantMessageEvent::TextStart { .. },
                    ..
                }
            ) {
                observed.cancel();
            }
            Box::pin(async { Ok(()) })
        }));
        let prompt = agent.prompt_text("Count slowly from 1 to 20.", None);
        started.cancelled().await;
        // No virtual time passes while waiting for the provider-start gate.
        // Two faux tokens at 20 tokens/second schedule the first chunk at
        // 100 ms. Keep the source's 30 ms abort, then settle that timer.
        assert_eq!(env.pending_timers(), 1);
        env.advance(30).await;
        agent.abort();
        env.advance(70).await;
        prompt.await.unwrap();
        unsubscribe();

        let state = agent.state();
        assert!(!state.is_streaming());
        let messages = state.messages().snapshot();
        assert!(messages.len() >= 2);
        let last_message = messages
            .last()
            .unwrap()
            .assistant()
            .expect("Expected assistant message")
            .snapshot();
        assert_eq!(last_message.stop_reason, StopReason::Aborted);
        assert!(last_message.error_message.is_some());
        assert_eq!(state.error_message(), last_message.error_message);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emits_lifecycle_updates_while_streaming() {
        let (env, faux) = setup(RegisterFauxProviderOptions {
            token_size: Some(FauxTokenSize {
                min: Some(1.0),
                max: Some(1.0),
            }),
            ..Default::default()
        });
        faux.set_responses(vec![text_response(&env, "1 2 3 4 5")]);
        let agent = new_agent(
            env,
            &faux,
            "You are a helpful assistant.",
            Some(ThinkingLevel::Off),
            Some(vec![]),
        );
        let events = Shared::new(Vec::new());
        let captured = events.clone();
        let unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            captured.push(event.kind());
            Box::pin(async { Ok(()) })
        }));
        agent.prompt_text("Count from 1 to 5.", None).await.unwrap();
        unsubscribe();
        let events = events.snapshot();
        for expected in [
            "agent_start",
            "turn_start",
            "message_start",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
        ] {
            assert!(events.contains(&expected), "missing {expected}: {events:?}");
        }
        let first = |kind| events.iter().position(|event| *event == kind).unwrap();
        assert!(first("agent_start") < first("message_start"));
        assert!(first("message_start") < first("message_end"));
        assert!(
            first("message_end")
                < events
                    .iter()
                    .rposition(|event| *event == "agent_end")
                    .unwrap()
        );
        assert!(!agent.state().is_streaming());
        assert_eq!(agent.state().messages().len(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn maintains_context_across_multiple_turns() {
        let (env, faux) = setup(RegisterFauxProviderOptions::default());
        let response_env = env.clone();
        faux.set_responses(vec![text_response(&env, "Nice to meet you, Alice."),
            FauxResponseStep::Factory(Arc::new(move |context, _, _, _| {
                let has_alice = context.messages.iter().any(|message| match message {
                    Message::User(message) => match &message.content {
                        UserMessageContent::Text(text) => text.to_string_lossy().contains("Alice"),
                        UserMessageContent::Blocks(blocks) => blocks.iter().any(|block| matches!(block, UserContent::Text(text) if text.text.to_string_lossy().contains("Alice"))),
                    },
                    _ => false,
                });
                let response = faux_assistant_message(response_env.as_ref(),
                    if has_alice { "Your name is Alice." } else { "I do not know your name." }, Default::default());
                Box::pin(async move { Ok(response) })
            })),
        ]);
        let agent = new_agent(
            env,
            &faux,
            "You are a helpful assistant.",
            Some(ThinkingLevel::Off),
            Some(vec![]),
        );
        agent.prompt_text("My name is Alice.", None).await.unwrap();
        assert_eq!(agent.state().messages().len(), 3);
        agent.prompt_text("What is my name?", None).await.unwrap();
        assert_eq!(agent.state().messages().len(), 5);
        let last_message = agent.state().messages().get(4).unwrap();
        assert_eq!(last_message.role(), "assistant");
        assert!(
            get_text_content(&last_message)
                .to_lowercase()
                .contains("alice")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_thinking_content_blocks() {
        let (env, faux) = setup(RegisterFauxProviderOptions {
            models: Some(vec![FauxModelDefinition {
                id: "faux-reasoning".into(),
                reasoning: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        });
        faux.set_responses(vec![
            faux_assistant_message(
                env.as_ref(),
                vec![faux_thinking("step by step").into(), faux_text("4").into()],
                Default::default(),
            )
            .into(),
        ]);
        let agent = new_agent(
            env,
            &faux,
            "You are a helpful assistant.",
            Some(ThinkingLevel::Low),
            Some(vec![]),
        );
        agent.prompt_text("What is 2+2?", None).await.unwrap();
        let assistant_message = agent
            .state()
            .messages()
            .get(2)
            .expect("assistant at index 2")
            .assistant()
            .expect("Expected assistant message")
            .snapshot();
        assert_eq!(
            assistant_message.content,
            vec![faux_thinking("step by step").into(), faux_text("4").into()]
        );
    }
}

mod agent_continue_with_faux_provider {
    use super::*;

    mod validation {
        use super::*;

        #[tokio::test(flavor = "current_thread")]
        async fn throws_when_no_messages_in_context() {
            let (env, faux) = setup(Default::default());
            let agent = new_agent(env, &faux, "Test", None, None);
            assert!(
                agent
                    .continue_run()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("No messages to continue from")
            );
        }

        #[tokio::test(flavor = "current_thread")]
        async fn throws_when_last_message_is_assistant() {
            let (env, faux) = setup(Default::default());
            let agent = new_agent(env.clone(), &faux, "Test", None, None);
            let mut assistant_message =
                AssistantMessage::new(faux.get_model(), env.now_ms() as f64);
            assistant_message.content = vec![faux_text("Hello").into()];
            assistant_message.stop_reason = StopReason::Stop;
            agent
                .state()
                .set_messages(&vec![assistant_message.into()].into());
            assert!(
                agent
                    .continue_run()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("Cannot continue from message role: assistant")
            );
        }
    }

    mod continue_from_user_message {
        use super::*;

        #[tokio::test(flavor = "current_thread")]
        async fn continues_and_gets_a_response_when_last_message_is_user() {
            let (env, faux) = setup(Default::default());
            faux.set_responses(vec![text_response(&env, "HELLO WORLD")]);
            let agent = new_agent(
                env.clone(),
                &faux,
                "You are a helpful assistant. Follow instructions exactly.",
                Some(ThinkingLevel::Off),
                Some(vec![]),
            );
            let user_message = UserMessage {
                content: UserMessageContent::Blocks(vec![
                    faux_text("Say exactly: HELLO WORLD").into(),
                ]),
                timestamp: env.now_ms() as f64,
                ..Default::default()
            };
            agent
                .state()
                .set_messages(&vec![user_message.into()].into());
            agent.continue_run().await.unwrap();

            let state = agent.state();
            assert!(!state.is_streaming());
            let messages = state.messages().snapshot();
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0].role(), "user");
            assert_eq!(messages[1].role(), "assistant");
            assert!(
                get_text_content(&messages[1])
                    .to_uppercase()
                    .contains("HELLO WORLD")
            );
        }
    }

    mod continue_from_tool_result {
        use super::*;

        #[tokio::test(flavor = "current_thread")]
        async fn continues_and_processes_tool_results() {
            let (env, faux) = setup(Default::default());
            faux.set_responses(vec![text_response(&env, "The answer is 8.")]);
            let agent = new_agent(
                env.clone(),
                &faux,
                "You are a helpful assistant. After getting a calculation result, state the answer clearly.",
                Some(ThinkingLevel::Off),
                Some(vec![calculate_tool()]),
            );
            let user_message = UserMessage {
                content: UserMessageContent::Blocks(vec![faux_text("What is 5 + 3?").into()]),
                timestamp: env.now_ms() as f64,
                ..Default::default()
            };
            let mut assistant_message =
                AssistantMessage::new(faux.get_model(), env.now_ms() as f64);
            assistant_message.content = vec![
                faux_text("Let me calculate that.").into(),
                faux_tool_call(
                    env.as_ref(),
                    "calculate",
                    JsValue::from_json_with_js_numbers(json!({"expression": "5 + 3"})),
                    Some("calc-1".into()),
                )
                .into(),
            ];
            assistant_message.stop_reason = StopReason::ToolUse;
            let tool_result = ToolResultMessage {
                tool_call_id: "calc-1".into(),
                tool_name: "calculate".into(),
                content: vec![faux_text("5 + 3 = 8").into()],
                is_error: false,
                timestamp: env.now_ms() as f64,
                ..Default::default()
            };
            agent.state().set_messages(
                &vec![
                    user_message.into(),
                    assistant_message.into(),
                    tool_result.into(),
                ]
                .into(),
            );
            agent.continue_run().await.unwrap();

            let state = agent.state();
            assert!(!state.is_streaming());
            let messages = state.messages().snapshot();
            assert!(messages.len() >= 4);
            let last_message = messages.last().unwrap();
            assert_eq!(last_message.role(), "assistant");
            assert!(get_text_content(last_message).contains('8'));
        }
    }
}
