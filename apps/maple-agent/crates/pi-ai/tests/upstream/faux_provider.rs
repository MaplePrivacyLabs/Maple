use std::sync::Arc;

use pi_ai::env::CancellationToken;
use pi_ai::providers::faux::*;
use pi_ai::types::*;
use pi_ai::utils::event_stream::AssistantMessageEventStream;
use pi_ai::utils::transcript::normalize_context;
use pi_testkit::VirtualEnv;
use serde_json::{Value, json};

fn setup(options: RegisterFauxProviderOptions) -> (Arc<VirtualEnv>, FauxProviderHandle) {
    let env = Arc::new(VirtualEnv::new(1_767_225_600_000));
    let faux = create_faux_core(env.clone(), options);
    (env, faux)
}
fn message(env: &VirtualEnv, text: impl Into<FauxAssistantContent>) -> AssistantMessage {
    faux_assistant_message(env, text, FauxAssistantOptions::default())
}
fn user(text: &str) -> Message {
    UserMessage {
        content: UserMessageContent::Text(text.into()),
        timestamp: 1_767_225_600_000.0,
        ..UserMessage::default()
    }
    .into()
}
fn context() -> TranscriptContext {
    normalize_context(Context {
        messages: vec![user("hi")],
        ..Context::default()
    })
}
async fn complete(
    faux: &FauxProviderHandle,
    context: TranscriptContext,
    options: Option<SimpleStreamOptions>,
) -> AssistantMessage {
    faux.stream(faux.get_model().clone(), context, options)
        .result()
        .await
}
async fn collect_events(mut stream: AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}
fn event_name(event: &AssistantMessageEvent) -> String {
    serde_json::to_value(event).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned()
}
fn arguments(value: Value) -> JsObject {
    let Value::Object(object) = value else {
        panic!("expected argument object")
    };
    object.try_into().unwrap()
}
fn factory(
    run: impl Fn(
        TranscriptContext,
        Option<SimpleStreamOptions>,
        SharedFauxProviderState,
        Model,
    ) -> BoxFuture<Result<AssistantMessage, JsString>>
    + Send
    + Sync
    + 'static,
) -> FauxResponseStep {
    FauxResponseStep::Factory(Arc::new(run))
}
fn session_options(id: &str, retention: CacheRetention) -> SimpleStreamOptions {
    SimpleStreamOptions {
        stream: StreamOptions {
            session_id: Some(id.to_owned()),
            cache_retention: Some(retention),
            ..StreamOptions::default()
        },
        ..SimpleStreamOptions::default()
    }
}
fn fixed_tokens(size: f64) -> RegisterFauxProviderOptions {
    RegisterFauxProviderOptions {
        token_size: Some(FauxTokenSize {
            min: Some(size),
            max: Some(size),
        }),
        ..RegisterFauxProviderOptions::default()
    }
}

mod rust_adaptations {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn raw_content_failures_finish_the_stream_and_preserve_the_input() {
        for (raw, expected) in [
            (
                json!({"role":"system","content":null}),
                "Cannot read properties of null (reading 'filter')",
            ),
            (
                json!({"role":"system"}),
                "Cannot read properties of undefined (reading 'filter')",
            ),
            (
                json!({"role":"user","content":null}),
                "Cannot read properties of null (reading 'map')",
            ),
            (
                json!({"role":"assistant"}),
                "Cannot read properties of undefined (reading 'map')",
            ),
        ] {
            let (env, provider) = setup(fixed_tokens(1.0));
            provider.set_responses(vec![message(&env, "answer").into()]);
            let raw: Message = serde_json::from_value(raw).unwrap();
            let before = pi_ai::utils::js_value::to_js_value(&raw).unwrap();
            let result = complete(
                &provider,
                normalize_context(Context {
                    messages: vec![raw.clone()],
                    ..Context::default()
                }),
                None,
            )
            .await;
            assert_eq!(result.stop_reason, StopReason::Error);
            assert_eq!(result.error_message.as_ref().unwrap(), expected);
            assert_eq!(pi_ai::utils::js_value::to_js_value(&raw).unwrap(), before);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn raw_message_usage_reads_content_without_import_time_metadata_repair() {
        let (env, provider) = setup(fixed_tokens(1.0));
        provider.set_responses(vec![message(&env, "answer").into()]);
        let raw: Message =
            serde_json::from_value(json!({"role":"user","content":"abcd","extra":1})).unwrap();
        let before = pi_ai::utils::js_value::to_js_value(&raw).unwrap();
        let result = complete(
            &provider,
            normalize_context(Context {
                messages: vec![raw.clone()],
                ..Context::default()
            }),
            None,
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(result.usage.input, 3.0); // "user:abcd" / 4, rounded up.
        assert_eq!(pi_ai::utils::js_value::to_js_value(&raw).unwrap(), before);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn finalized_tool_arguments_share_the_original_handle_across_events() {
        let (env, provider) = setup(fixed_tokens(1.0));
        let call = faux_tool_call(
            env.as_ref(),
            "tool",
            arguments(json!({"nested":{"value":"before"}})),
            Some("id".into()),
        );
        let original = call.arguments.clone();
        provider.set_responses(vec![message(&env, call).into()]);
        let stream = provider.stream(provider.get_model().clone(), context(), None);
        let events = collect_events(stream.clone()).await;
        let (ended, partial) = events
            .iter()
            .find_map(|event| match event {
                AssistantMessageEvent::ToolcallEnd {
                    tool_call, partial, ..
                } => Some((tool_call, partial)),
                _ => None,
            })
            .expect("tool call completed");
        let final_message = stream.result().await;
        let AssistantContent::ToolCall(final_call) = &final_message.content[0] else {
            panic!("expected final tool call")
        };
        let AssistantContent::ToolCall(partial_call) = &partial.snapshot().content[0] else {
            panic!("expected partial tool call")
        };
        assert!(original.ptr_eq(&ended.arguments));
        assert!(original.ptr_eq(&partial_call.arguments));
        assert!(original.ptr_eq(&final_call.arguments));
        original.update(|value| value["nested"]["value"] = "after".into());
        assert_eq!(
            ended.arguments.snapshot()["nested"]["value"],
            json!("after")
        );
        assert_eq!(
            partial_call.arguments.snapshot()["nested"]["value"],
            json!("after")
        );
        assert_eq!(
            final_call.arguments.snapshot()["nested"]["value"],
            json!("after")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_argument_runtime_values_remain_intact_until_json_observation() {
        let (env, provider) = setup(fixed_tokens(1.0));
        for arguments in [
            JsValue::Null,
            JsValue::Array(vec![JsValue::Number(3.0), JsValue::Bool(false)]),
            JsValue::String(JsString::from_utf16(vec![0xd800])),
            JsValue::Number(f64::INFINITY),
        ] {
            let expected = pi_ai::utils::js_json::stringify(&arguments);
            let call = faux_tool_call(env.as_ref(), "tool", arguments.clone(), Some("id".into()));
            provider.set_responses(vec![message(&env, call).into()]);
            let stream = provider.stream(provider.get_model().clone(), context(), None);
            let mut joined = JsString::default();
            for event in collect_events(stream.clone()).await {
                if let AssistantMessageEvent::ToolcallDelta { delta, .. } = event {
                    joined.push(&delta);
                }
            }
            assert_eq!(joined, expected);
            let final_message = stream.result().await;
            let AssistantContent::ToolCall(call) = &final_message.content[0] else {
                panic!("expected tool call")
            };
            assert_eq!(call.arguments, arguments);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_deferred_fetches_resolve_before_either_caches_its_final_message() {
        // Verified against pinned Node 22.23.2 and Pi v1.0.4: even a ready
        // factory crosses await boundaries, so two fetches both run it.
        let (env, provider) = setup(fixed_tokens(1.0));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        provider.set_responses(vec![factory(move |_, _, _, _| {
            let call = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let message = AssistantMessage {
                response_id: Some(format!("factory:{call}").into()),
                ..message(&env, "ok")
            };
            Box::pin(async move { Ok(message) })
        })]);
        let context = normalize_context(Context {
            messages: vec![user("abcdefghijkl")],
            ..Default::default()
        });
        let submitted = complete(
            &provider,
            context,
            Some(SimpleStreamOptions {
                deferred: Some(DeferredRequest::Enabled(true)),
                stream: StreamOptions {
                    session_id: Some("isolated-oracle-session".to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await;
        let handle = submitted.deferred.unwrap();
        let first = provider.fetch_deferred(provider.get_model().clone(), handle.clone(), None);
        let second = provider.fetch_deferred(provider.get_model().clone(), handle.clone(), None);
        let (first, second) = tokio::join!(first.result(), second.result());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            first.response_id.as_ref().and_then(JsString::as_str),
            Some("factory:1")
        );
        assert_eq!(
            (
                first.usage.input,
                first.usage.output,
                first.usage.cache_read,
                first.usage.cache_write,
                first.usage.total_tokens
            ),
            (5.0, 1.0, 0.0, 5.0, 11.0)
        );
        assert_eq!(
            second.response_id.as_ref().and_then(JsString::as_str),
            Some("factory:2")
        );
        assert_eq!(
            (
                second.usage.input,
                second.usage.output,
                second.usage.cache_read,
                second.usage.cache_write,
                second.usage.total_tokens
            ),
            (0.0, 1.0, 5.0, 0.0, 6.0)
        );
        let replay = provider
            .fetch_deferred(provider.get_model().clone(), handle, None)
            .result()
            .await;
        assert_eq!(replay, second);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unobserved_producer_progresses_and_partials_preserve_shallow_copy_membership() {
        let (env, provider) = setup(fixed_tokens(1.0));
        let (started, observed) = tokio::sync::oneshot::channel();
        let started = std::sync::Mutex::new(Some(started));
        provider.set_responses(vec![factory(move |_, _, _, _| {
            started.lock().unwrap().take().unwrap().send(()).unwrap();
            let message = message(
                &env,
                vec![faux_text("abcdef").into(), faux_thinking("thought").into()],
            );
            Box::pin(async move { Ok(message) })
        })]);
        let stream = provider.stream(provider.get_model().clone(), context(), None);
        // An explicit factory gate proves progress without polling any event
        // or result future; it does not assume a scheduler-yield count.
        observed.await.unwrap();
        let events = collect_events(stream).await;
        let AssistantMessageEvent::Start { partial } = &events[0] else {
            panic!("expected start")
        };
        assert!(partial.snapshot().content.is_empty());
        let AssistantMessageEvent::TextStart { partial, .. } = &events[1] else {
            panic!("expected text start")
        };
        assert_eq!(partial.snapshot().content, [faux_text("abcdef").into()]);
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Done { .. })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consumer_block_mutation_is_shared_by_later_faux_partials() {
        let (env, provider) = setup(RegisterFauxProviderOptions {
            tokens_per_second: Some(100.0),
            ..fixed_tokens(1.0)
        });
        provider.set_responses(vec![message(&env, "abcdef").into()]);
        let mut stream = provider.stream(provider.get_model().clone(), context(), None);
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Start { .. })
        ));
        let Some(AssistantMessageEvent::TextStart { partial: early, .. }) = stream.next().await
        else {
            panic!("expected text start")
        };
        early
            .update_block(0, |block| {
                let AssistantContent::Text(block) = block else {
                    panic!("expected text")
                };
                block.text.push_str("prefix");
            })
            .unwrap();
        let Some(AssistantMessageEvent::TextDelta { partial: later, .. }) =
            next_paced(&mut stream, &env).await
        else {
            panic!("expected text delta")
        };
        assert!(
            early
                .content_block(0)
                .unwrap()
                .ptr_eq(&later.content_block(0).unwrap())
        );
        assert_eq!(later.snapshot().content, [faux_text("prefixabcd").into()]);
        while next_paced(&mut stream, &env).await.is_some() {}
        assert_eq!(early.snapshot().content, [faux_text("prefixabcdef").into()]);
        assert_eq!(stream.result().await.content, [faux_text("abcdef").into()]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn paced_text_deltas_preserve_split_surrogates_and_repair_the_partial() {
        let (env, provider) = setup(RegisterFauxProviderOptions {
            tokens_per_second: Some(100.0),
            ..fixed_tokens(1.0)
        });
        provider.set_responses(vec![message(&env, "abc😀z").into()]);
        let mut stream = provider.stream(provider.get_model().clone(), context(), None);
        assert!(matches!(
            next_paced(&mut stream, &env).await,
            Some(AssistantMessageEvent::Start { .. })
        ));
        assert!(matches!(
            next_paced(&mut stream, &env).await,
            Some(AssistantMessageEvent::TextStart {
                content_index: 0,
                ..
            })
        ));

        let Some(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: first_delta,
            partial: first_partial,
        }) = next_paced(&mut stream, &env).await
        else {
            panic!("expected first text delta")
        };
        assert_eq!(first_delta.as_utf16(), [0x61, 0x62, 0x63, 0xd83d]);
        assert_eq!(first_delta.as_str(), None);
        assert_eq!(
            serde_json::to_string(&first_delta).unwrap(),
            r#""abc\ud83d""#
        );
        // Inspect before advancing the pacing clock: the partial must retain
        // the same lone surrogate, rather than replacement text or the final pair.
        assert_eq!(
            first_partial.snapshot().content,
            [faux_text(first_delta.clone()).into()]
        );

        let Some(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: second_delta,
            partial: second_partial,
        }) = next_paced(&mut stream, &env).await
        else {
            panic!("expected second text delta")
        };
        assert_eq!(second_delta.as_utf16(), [0xde00, 0x7a]);
        assert_eq!(second_delta.as_str(), None);
        let snapshot = second_partial.snapshot();
        let AssistantContent::Text(text) = &snapshot.content[0] else {
            panic!("expected text partial")
        };
        assert_eq!(text.text.as_str(), Some("abc😀z"));
        assert_eq!(first_partial.snapshot().content, snapshot.content);

        let Some(AssistantMessageEvent::TextEnd { content, .. }) =
            next_paced(&mut stream, &env).await
        else {
            panic!("expected text end")
        };
        assert_eq!(content.as_str(), Some("abc😀z"));
        let Some(AssistantMessageEvent::Done { reason, message }) =
            next_paced(&mut stream, &env).await
        else {
            panic!("expected done")
        };
        assert_eq!(reason, DoneReason::Stop);
        assert_eq!(message.content, [faux_text("abc😀z").into()]);
        assert!(next_paced(&mut stream, &env).await.is_none());
        assert_eq!(stream.result().await, message);
    }

    #[test]
    fn shallow_copies_share_arrays_until_array_replacement_and_keep_block_identity() {
        let original = SharedAssistantMessage::new(AssistantMessage::default());
        original.append_content_copy(faux_text("a").into());
        let shallow = original.shallow_clone();
        original.push_content(faux_text("b").into());
        assert_eq!(shallow.snapshot().content.len(), 2);
        original.append_content_copy(faux_text("c").into());
        assert_eq!(shallow.snapshot().content.len(), 2);
        assert_eq!(original.snapshot().content.len(), 3);
        assert!(
            original
                .content_block(0)
                .unwrap()
                .ptr_eq(&shallow.content_block(0).unwrap())
        );
        shallow
            .update_block(0, |block| *block = faux_text("changed").into())
            .unwrap();
        assert_eq!(original.snapshot().content[0], faux_text("changed").into());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deferred_fetch_reuses_final_response_and_removes_submission_only_options() {
        let (env, provider) = setup(RegisterFauxProviderOptions {
            deferred: Some(FauxDeferredOptions {
                pending_fetches: Some(1.0),
                poll_after_ms: Some(50.0),
            }),
            ..fixed_tokens(1.0)
        });
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_in_factory = calls.clone();
        provider.set_responses(vec![factory(move |_, options, _, _| {
            calls_in_factory.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let options = options.unwrap();
            assert!(options.deferred.is_none());
            assert!(options.signal.is_none());
            assert!(options.on_response.is_none());
            let message = message(&env, "ready");
            Box::pin(async move { Ok(message) })
        })]);
        let signal = CancellationToken::new();
        let submitted = complete(
            &provider,
            context(),
            Some(SimpleStreamOptions {
                deferred: Some(DeferredRequest::Enabled(true)),
                stream: StreamOptions {
                    request: ProviderRequestOptions {
                        signal: Some(signal.clone()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(submitted.stop_reason, StopReason::Deferred);
        let handle = submitted.deferred.unwrap();
        assert_eq!(handle.poll_after_ms, Some(50.0));
        signal.cancel();
        let pending = provider
            .fetch_deferred(provider.get_model().clone(), handle.clone(), None)
            .result()
            .await;
        assert_eq!(pending.stop_reason, StopReason::Deferred);
        let ready = provider
            .fetch_deferred(provider.get_model().clone(), handle.clone(), None)
            .result()
            .await;
        assert_eq!(ready.content, [faux_text("ready").into()]);
        let again = provider
            .fetch_deferred(provider.get_model().clone(), handle, None)
            .result()
            .await;
        assert_eq!(again, ready);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(provider.state.snapshot().deferred_fetch_count, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deferred_cancellation_records_eagerly_and_rejects_later_fetch() {
        let (env, provider) = setup(Default::default());
        provider.set_responses(vec![message(&env, "unused").into()]);
        let submitted = complete(
            &provider,
            context(),
            Some(SimpleStreamOptions {
                deferred: Some(DeferredRequest::Enabled(true)),
                ..Default::default()
            }),
        )
        .await;
        let handle = submitted.deferred.unwrap();
        let hook_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_counter = hook_calls.clone();
        let cancel = provider.cancel_deferred(
            provider.get_model().clone(),
            handle.clone(),
            Some(ProviderRequestOptions {
                on_response: Some(Arc::new(move |response, _| {
                    assert_eq!(response.status, 200);
                    hook_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                })),
                ..Default::default()
            }),
        );
        assert_eq!(
            provider.state.snapshot().cancelled_deferred.as_slice(),
            std::slice::from_ref(&handle)
        );
        assert_eq!(hook_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        cancel.await.unwrap();
        let rejected = provider
            .fetch_deferred(provider.get_model().clone(), handle.clone(), None)
            .result()
            .await;
        assert_eq!(
            rejected.error_message,
            Some(
                format!(
                    "Faux deferred response was cancelled: {}",
                    handle.id.as_str().unwrap()
                )
                .into()
            )
        );
    }
}
async fn next_paced(
    stream: &mut AssistantMessageEventStream,
    env: &VirtualEnv,
) -> Option<AssistantMessageEvent> {
    let next = stream.next();
    tokio::pin!(next);
    for _ in 0..10_000 {
        if let std::task::Poll::Ready(event) = futures_util::poll!(&mut next) {
            return event;
        }
        env.advance(10).await;
        tokio::task::yield_now().await;
    }
    panic!("paced faux stream failed to settle");
}

#[allow(clippy::module_inception)] // Preserve the upstream describe path.
mod faux_provider {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn registers_a_custom_provider_and_estimates_usage() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![message(&env, "hello world").into()]);
        let context = normalize_context(Context {
            system_prompt: Some("Be concise.".into()),
            messages: vec![user("hi there")],
            ..Context::default()
        });
        let response = complete(&registration, context, None).await;
        assert_eq!(response.content, [faux_text("hello world").into()]);
        assert!(response.usage.input > 0.0);
        assert!(response.usage.output > 0.0);
        assert_eq!(
            response.usage.total_tokens,
            response.usage.input + response.usage.output
        );
        assert_eq!(registration.state.snapshot().call_count, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supports_helper_blocks_for_text_thinking_and_tool_calls() {
        let (env, registration) = setup(Default::default());
        let call = faux_tool_call(env.as_ref(), "echo", arguments(json!({"text":"hi"})), None);
        registration.set_responses(vec![
            faux_assistant_message(
                env.as_ref(),
                vec![
                    faux_thinking("think").into(),
                    call.clone().into(),
                    faux_text("done").into(),
                ],
                FauxAssistantOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    ..FauxAssistantOptions::default()
                },
            )
            .into(),
        ]);
        let response = complete(&registration, context(), None).await;
        assert!(!call.id.is_empty());
        assert_eq!(
            response.content,
            [
                faux_thinking("think").into(),
                call.into(),
                faux_text("done").into()
            ]
        );
        assert_eq!(response.stop_reason, StopReason::ToolUse);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supports_multiple_models_with_per_model_reasoning_and_model_aware_factories() {
        let (env, registration) = setup(RegisterFauxProviderOptions {
            models: Some(vec![
                FauxModelDefinition {
                    id: "faux-fast".to_owned(),
                    name: Some("Faux Fast".to_owned()),
                    reasoning: Some(false),
                    ..Default::default()
                },
                FauxModelDefinition {
                    id: "faux-thinker".to_owned(),
                    name: Some("Faux Thinker".to_owned()),
                    reasoning: Some(true),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        });
        let response = factory(move |_, _, _, model| {
            let message = message(&env, format!("{}:{}", model.id, model.reasoning));
            Box::pin(async move { Ok(message) })
        });
        registration.set_responses(vec![response.clone(), response]);
        assert_eq!(
            registration
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["faux-fast", "faux-thinker"]
        );
        assert!(std::ptr::eq(
            registration.get_model(),
            &registration.models[0]
        ));
        assert!(!registration.get_model_by_id("faux-fast").unwrap().reasoning);
        assert!(
            registration
                .get_model_by_id("faux-thinker")
                .unwrap()
                .reasoning
        );
        let fast = registration
            .stream(
                registration.get_model_by_id("faux-fast").unwrap().clone(),
                context(),
                None,
            )
            .result()
            .await;
        let thinker = registration
            .stream(
                registration
                    .get_model_by_id("faux-thinker")
                    .unwrap()
                    .clone(),
                context(),
                None,
            )
            .result()
            .await;
        assert_eq!(fast.content, [faux_text("faux-fast:false").into()]);
        assert_eq!(thinker.content, [faux_text("faux-thinker:true").into()]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rewrites_api_provider_and_model_on_returned_messages() {
        let (env, registration) = setup(RegisterFauxProviderOptions {
            api: Some("faux:test".to_owned()),
            provider: Some("faux-provider".to_owned()),
            models: Some(vec![FauxModelDefinition {
                id: "faux-model".to_owned(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        registration.set_responses(vec![message(&env, "hello").into()]);
        let response = complete(&registration, context(), None).await;
        assert_eq!(response.api, "faux:test");
        assert_eq!(response.provider, "faux-provider");
        assert_eq!(response.model, "faux-model");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consumes_queued_responses_in_order_and_errors_when_exhausted() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![
            message(&env, "first").into(),
            message(&env, "second").into(),
        ]);
        let first = complete(&registration, context(), None).await;
        let second = complete(&registration, context(), None).await;
        let exhausted = complete(&registration, context(), None).await;
        assert_eq!(first.content, [faux_text("first").into()]);
        assert_eq!(second.content, [faux_text("second").into()]);
        assert_eq!(exhausted.stop_reason, StopReason::Error);
        assert_eq!(
            exhausted.error_message.as_ref().and_then(JsString::as_str),
            Some("No more faux responses queued")
        );
        assert_eq!(registration.get_pending_response_count(), 0);
        assert_eq!(registration.state.snapshot().call_count, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn can_replace_and_append_queued_responses() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![message(&env, "first").into()]);
        assert_eq!(
            complete(&registration, context(), None).await.content,
            [faux_text("first").into()]
        );
        assert_eq!(registration.get_pending_response_count(), 0);
        registration.set_responses(vec![message(&env, "second").into()]);
        assert_eq!(registration.get_pending_response_count(), 1);
        assert_eq!(
            complete(&registration, context(), None).await.content,
            [faux_text("second").into()]
        );
        registration.append_responses(vec![
            message(&env, "third").into(),
            message(&env, "fourth").into(),
        ]);
        assert_eq!(registration.get_pending_response_count(), 2);
        assert_eq!(
            complete(&registration, context(), None).await.content,
            [faux_text("third").into()]
        );
        assert_eq!(
            complete(&registration, context(), None).await.content,
            [faux_text("fourth").into()]
        );
        assert_eq!(registration.get_pending_response_count(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supports_async_response_factories() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![factory(move |context, _, state, _| {
            let env = env.clone();
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok(message(
                    &env,
                    format!("{}:{}", context.messages.len(), state.snapshot().call_count),
                ))
            })
        })]);
        assert_eq!(
            complete(&registration, context(), None).await.content,
            [faux_text("1:1").into()]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emits_an_error_when_a_response_factory_throws() {
        let (_, registration) = setup(Default::default());
        registration.set_responses(vec![factory(|_, _, _, _| {
            Box::pin(async { Err("boom".into()) })
        })]);
        let events =
            collect_events(registration.stream(registration.get_model().clone(), context(), None))
                .await;
        assert_eq!(events.len(), 1);
        let AssistantMessageEvent::Error { error, .. } = &events[0] else {
            panic!("expected error")
        };
        assert_eq!(error.stop_reason, StopReason::Error);
        assert_eq!(
            error.error_message.as_ref().and_then(JsString::as_str),
            Some("boom")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_a_queued_response_without_a_terminal_stop_reason() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![
            faux_assistant_message(
                env.as_ref(),
                "partial",
                FauxAssistantOptions {
                    stop_reason: Some(StopReason::Pending),
                    ..Default::default()
                },
            )
            .into(),
        ]);
        let events =
            collect_events(registration.stream(registration.get_model().clone(), context(), None))
                .await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AssistantMessageEvent::Done { .. }))
        );
        let AssistantMessageEvent::Error { error, .. } = events.last().unwrap() else {
            panic!("expected error")
        };
        assert_eq!(error.stop_reason, StopReason::Error);
        assert_eq!(
            error.error_message.as_ref().and_then(JsString::as_str),
            Some("Faux response ended without a stop reason")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn estimates_prompt_and_output_tokens_from_serialized_context() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![message(&env, "done").into()]);
        let tool = Tool {
            name: "echo".into(),
            description: "Echo back text".into(),
            parameters: Schema::typebox(
                json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
            ),
            ..Default::default()
        };
        let context = normalize_context(Context {
            system_prompt: Some("sys".into()),
            messages: vec![
                UserMessage {
                    content: UserMessageContent::Blocks(vec![
                        faux_text("hello").into(),
                        ImageContent::new("abcd", "image/png").into(),
                    ]),
                    timestamp: 1.0,
                    ..Default::default()
                }
                .into(),
                message(&env, "prior").into(),
                ToolResultMessage {
                    tool_call_id: "tool-1".into(),
                    tool_name: "echo".into(),
                    content: vec![faux_text("tool out").into()],
                    is_error: false,
                    timestamp: 2.0,
                    ..Default::default()
                }
                .into(),
            ],
            tools: Some(vec![tool.clone()]),
        });
        let response = complete(&registration, context, None).await;
        let prompt_text = [
            "system:sys".to_owned(),
            "user:hello\n[image:image/png:4]".to_owned(),
            "assistant:prior".to_owned(),
            "toolResult:echo\ntool out".to_owned(),
            format!("tools:{}", pi_ai::utils::js_json::stringify(&json!([tool]))),
        ]
        .join("\n\n");
        let expected_prompt_tokens = (prompt_text.encode_utf16().count() as f64 / 4.0).ceil();
        let expected_output_tokens = ("done".len() as f64 / 4.0).ceil();
        assert_eq!(response.usage.input, expected_prompt_tokens);
        assert_eq!(response.usage.output, expected_output_tokens);
        assert_eq!(response.usage.cache_read, 0.0);
        assert_eq!(response.usage.cache_write, 0.0);
        assert_eq!(
            response.usage.total_tokens,
            expected_prompt_tokens + expected_output_tokens
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn does_not_share_cache_across_sessions_or_requests_without_session_id() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![
            message(&env, "first").into(),
            message(&env, "second").into(),
            message(&env, "third").into(),
        ]);
        let mut context = normalize_context(Context {
            messages: vec![user("hello")],
            ..Default::default()
        });
        let first = complete(
            &registration,
            context.clone(),
            Some(session_options("session-1", CacheRetention::Short)),
        )
        .await;
        assert!(first.usage.cache_write > 0.0);
        context.messages.push(first.into());
        context.messages.push(user("follow up"));
        let second = complete(
            &registration,
            context.clone(),
            Some(session_options("session-2", CacheRetention::Short)),
        )
        .await;
        assert_eq!(second.usage.cache_read, 0.0);
        assert!(second.usage.cache_write > 0.0);
        let third = complete(&registration, context, None).await;
        assert_eq!(third.usage.cache_read, 0.0);
        assert_eq!(third.usage.cache_write, 0.0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn simulates_prompt_caching_per_session_id() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![
            message(&env, "first").into(),
            message(&env, "second").into(),
        ]);
        let mut context = normalize_context(Context {
            system_prompt: Some("Be concise.".into()),
            messages: vec![user("hello")],
            ..Default::default()
        });
        let first = complete(
            &registration,
            context.clone(),
            Some(session_options("session-1", CacheRetention::Short)),
        )
        .await;
        assert_eq!(first.usage.cache_read, 0.0);
        assert!(first.usage.cache_write > 0.0);
        context.messages.push(first.into());
        context.messages.push(user("follow up"));
        let second = complete(
            &registration,
            context,
            Some(session_options("session-1", CacheRetention::Short)),
        )
        .await;
        assert!(second.usage.cache_read > 0.0);
        assert!(second.usage.input + second.usage.cache_read > second.usage.input);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn does_not_simulate_caching_when_cache_retention_is_none() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![
            message(&env, "first").into(),
            message(&env, "second").into(),
        ]);
        let mut context = normalize_context(Context {
            messages: vec![user("hello")],
            ..Default::default()
        });
        complete(
            &registration,
            context.clone(),
            Some(session_options("session-1", CacheRetention::None)),
        )
        .await;
        context.messages.push(message(&env, "first").into());
        context.messages.push(user("follow up"));
        let second = complete(
            &registration,
            context,
            Some(session_options("session-1", CacheRetention::None)),
        )
        .await;
        assert_eq!(second.usage.cache_read, 0.0);
        assert_eq!(second.usage.cache_write, 0.0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streams_thinking_text_and_partial_tool_call_deltas() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![
            faux_assistant_message(
                env.as_ref(),
                vec![
                    faux_thinking("thinking text").into(),
                    faux_text("answer text").into(),
                    faux_tool_call(
                        env.as_ref(),
                        "echo",
                        arguments(json!({"text":"hi","count":12})),
                        Some("tool-1".into()),
                    )
                    .into(),
                ],
                FauxAssistantOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into(),
        ]);
        let events =
            collect_events(registration.stream(registration.get_model().clone(), context(), None))
                .await;
        let names = events.iter().map(event_name).collect::<Vec<_>>();
        for expected in [
            "thinking_start",
            "thinking_delta",
            "text_start",
            "text_delta",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
        ] {
            assert!(names.iter().any(|name| name == expected));
        }
        let deltas = events
            .iter()
            .filter_map(|event| {
                if let AssistantMessageEvent::ToolcallDelta { delta, .. } = event {
                    Some(delta.as_str().unwrap())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert!(deltas.len() > 1);
        assert_eq!(
            serde_json::from_str::<Value>(&deltas.join("")).unwrap(),
            json!({"text":"hi","count":12})
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streams_an_exact_event_order_for_fixed_size_chunks() {
        let (env, registration) = setup(fixed_tokens(1.0));
        registration.set_responses(vec![
            faux_assistant_message(
                env.as_ref(),
                vec![
                    faux_thinking("go").into(),
                    faux_text("ok").into(),
                    faux_tool_call(
                        env.as_ref(),
                        "echo",
                        JsonObject::new(),
                        Some("tool-1".into()),
                    )
                    .into(),
                ],
                FauxAssistantOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into(),
        ]);
        let events =
            collect_events(registration.stream(registration.get_model().clone(), context(), None))
                .await;
        let AssistantMessageEvent::Start { partial } = &events[0] else {
            panic!("expected start")
        };
        assert_eq!(partial.snapshot().stop_reason, StopReason::Pending);
        assert_eq!(
            events.iter().map(event_name).collect::<Vec<_>>(),
            [
                "start",
                "thinking_start",
                "thinking_delta",
                "thinking_end",
                "text_start",
                "text_delta",
                "text_end",
                "toolcall_start",
                "toolcall_delta",
                "toolcall_end",
                "done"
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streams_multiple_tool_calls_in_one_message() {
        let (env, registration) = setup(Default::default());
        registration.set_responses(vec![
            faux_assistant_message(
                env.as_ref(),
                vec![
                    faux_tool_call(
                        env.as_ref(),
                        "echo",
                        arguments(json!({"text":"one"})),
                        Some("tool-1".into()),
                    )
                    .into(),
                    faux_tool_call(
                        env.as_ref(),
                        "echo",
                        arguments(json!({"text":"two"})),
                        Some("tool-2".into()),
                    )
                    .into(),
                ],
                FauxAssistantOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into(),
        ]);
        let events =
            collect_events(registration.stream(registration.get_model().clone(), context(), None))
                .await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::ToolcallStart { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::ToolcallEnd { .. }))
                .count(),
            2
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streams_an_explicit_assistant_error_message_as_a_terminal_error() {
        let (env, registration) = setup(fixed_tokens(2.0));
        registration.set_responses(vec![
            AssistantMessage {
                stop_reason: StopReason::Error,
                error_message: Some("upstream failed".into()),
                ..message(&env, "partial")
            }
            .into(),
        ]);
        let events =
            collect_events(registration.stream(registration.get_model().clone(), context(), None))
                .await;
        assert_eq!(
            events.iter().map(event_name).collect::<Vec<_>>(),
            ["start", "text_start", "text_delta", "text_end", "error"]
        );
        let AssistantMessageEvent::Error { reason, error } = events.last().unwrap() else {
            panic!("expected error")
        };
        assert_eq!(*reason, ErrorReason::Error);
        assert_eq!(error.stop_reason, StopReason::Error);
        assert_eq!(
            error.error_message.as_ref().and_then(JsString::as_str),
            Some("upstream failed")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streams_an_explicit_assistant_aborted_message_as_a_terminal_error() {
        let (env, registration) = setup(fixed_tokens(2.0));
        registration.set_responses(vec![
            AssistantMessage {
                stop_reason: StopReason::Aborted,
                error_message: Some("Request was aborted".into()),
                ..message(&env, "partial")
            }
            .into(),
        ]);
        let events =
            collect_events(registration.stream(registration.get_model().clone(), context(), None))
                .await;
        assert_eq!(
            events.iter().map(event_name).collect::<Vec<_>>(),
            ["start", "text_start", "text_delta", "text_end", "error"]
        );
        let AssistantMessageEvent::Error { reason, error } = events.last().unwrap() else {
            panic!("expected error")
        };
        assert_eq!(*reason, ErrorReason::Aborted);
        assert_eq!(error.stop_reason, StopReason::Aborted);
        assert_eq!(
            error.error_message.as_ref().and_then(JsString::as_str),
            Some("Request was aborted")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supports_aborting_before_the_first_chunk() {
        let (env, registration) = setup(RegisterFauxProviderOptions {
            tokens_per_second: Some(50.0),
            ..fixed_tokens(3.0)
        });
        registration.set_responses(vec![message(&env, "abcdefghijklmnopqrstuvwxyz").into()]);
        let controller = CancellationToken::new();
        controller.cancel();
        let events = collect_events(registration.stream(
            registration.get_model().clone(),
            context(),
            Some(SimpleStreamOptions {
                stream: StreamOptions {
                    request: ProviderRequestOptions {
                        signal: Some(controller),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            }),
        ))
        .await;
        assert_eq!(events.len(), 1);
        let AssistantMessageEvent::Error { reason, error } = &events[0] else {
            panic!("expected error")
        };
        assert_eq!(*reason, ErrorReason::Aborted);
        assert_eq!(error.stop_reason, StopReason::Aborted);
    }

    async fn abort_mid_stream(
        block: AssistantContent,
        delta_name: &str,
        start_name: &str,
        end_name: &str,
    ) {
        let (env, registration) = setup(RegisterFauxProviderOptions {
            tokens_per_second: Some(100.0),
            ..fixed_tokens(3.0)
        });
        registration.set_responses(vec![
            faux_assistant_message(
                env.as_ref(),
                vec![block],
                FauxAssistantOptions {
                    stop_reason: Some(if delta_name == "toolcall_delta" {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    }),
                    ..Default::default()
                },
            )
            .into(),
        ]);
        let controller = CancellationToken::new();
        let mut stream = registration.stream(
            registration.get_model().clone(),
            context(),
            Some(SimpleStreamOptions {
                stream: StreamOptions {
                    request: ProviderRequestOptions {
                        signal: Some(controller.clone()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            }),
        );
        let mut names = Vec::new();
        let mut delta_count = 0;
        while let Some(event) = next_paced(&mut stream, &env).await {
            let name = event_name(&event);
            if name == delta_name {
                delta_count += 1;
                controller.cancel();
            }
            names.push(name);
        }
        assert_eq!(delta_count, 1);
        assert!(names.iter().any(|name| name == start_name));
        assert!(names.iter().any(|name| name == delta_name));
        assert!(names.iter().any(|name| name == "error"));
        assert!(!names.iter().any(|name| name == end_name));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supports_aborting_mid_text_stream_when_paced() {
        abort_mid_stream(
            faux_text("abcdefghijklmnopqrstuvwxyz").into(),
            "text_delta",
            "text_start",
            "text_end",
        )
        .await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn supports_aborting_mid_thinking_stream_when_paced() {
        abort_mid_stream(
            faux_thinking("abcdefghijklmnopqrstuvwxyz").into(),
            "thinking_delta",
            "thinking_start",
            "thinking_end",
        )
        .await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn supports_aborting_mid_toolcall_stream_when_paced() {
        abort_mid_stream(
            ToolCall::new(
                "tool-1",
                "echo",
                arguments(json!({"text":"abcdefghijklmnopqrstuvwxyz","count":123456789})),
            )
            .into(),
            "toolcall_delta",
            "toolcall_start",
            "toolcall_end",
        )
        .await;
    }
}
