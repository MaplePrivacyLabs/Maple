use super::*;
use crate::encrypt::encrypt_with_key;
use chrono::{TimeZone, Utc};
use std::cell::{Cell, RefCell};

fn key() -> SecretKey {
    SecretKey::from_slice(&[7; 32]).unwrap()
}

async fn row(kind: &str, id: i64, content: &str, tokens: i32) -> RawThreadMessage {
    RawThreadMessage {
        message_type: kind.into(),
        id,
        uuid: Uuid::from_u128(id as u128),
        response_uuid: Some(Uuid::from_u128(1000 + id as u128)),
        content_enc: Some(encrypt_with_key(&key(), content.as_bytes()).await),
        status: Some("completed".into()),
        created_at: Utc.timestamp_opt(id, 0).unwrap(),
        model: Some("deepseek-v4-1-flash".into()),
        token_count: Some(tokens),
        tool_call_id: None,
        finish_reason: Some("stop".into()),
        tool_name: None,
    }
}

async fn user(id: i64, text: &str, tokens: i32) -> RawThreadMessage {
    row("user", id, &serde_json::to_string(text).unwrap(), tokens).await
}

fn metadata(rows: &[RawThreadMessage]) -> Vec<RawThreadMessageMetadata> {
    rows.iter()
        .map(|row| RawThreadMessageMetadata {
            message_type: row.message_type.clone(),
            id: row.id,
            uuid: row.uuid,
            created_at: row.created_at,
            token_count: row.token_count,
            tool_call_id: row.tool_call_id,
            encrypted_bytes: row
                .content_enc
                .as_ref()
                .map_or(0, |bytes| bytes.len() as i64),
        })
        .collect()
}

fn fetch_rows(rows: &[RawThreadMessage], keys: &[MessageKey]) -> Vec<RawThreadMessage> {
    // Return reverse order, like an unordered heterogeneous query can, so the
    // strategy must restore metadata-selected order across every batch.
    keys.iter()
        .rev()
        .filter_map(|key| {
            rows.iter()
                .find(|row| row.message_type == key.0 && row.id == key.1)
                .cloned()
        })
        .collect()
}

async fn rows_with_tool_pair_across_batch_boundary() -> Vec<RawThreadMessage> {
    let mut rows = Vec::new();
    for id in 1..=126 {
        rows.push(user(id, "Synthetic", 1).await);
    }
    rows.push(row("reasoning", 127, "Current reasoning", 1).await);
    let mut call = row("tool_call", 128, "{}", 1).await;
    call.tool_call_id = Some(call.uuid);
    call.tool_name = Some("web_search".into());
    let mut output = row("tool_output", 129, "Result", 1).await;
    output.tool_call_id = call.tool_call_id;
    rows.extend([call, output]);
    rows
}

fn prepare_rows(
    rows: &[RawThreadMessage],
    model: &str,
    replay: ReasoningReplay,
) -> LegacyPreparedPrompt {
    prepare_with_fetch(
        &metadata(rows),
        vec![],
        0,
        &key(),
        model,
        replay,
        &[],
        0,
        0,
        |keys| Ok(fetch_rows(rows, keys)),
    )
    .unwrap()
}

#[tokio::test]
async fn ordinary_prompt_parity_preserves_order_and_incoming_reservation() {
    let rows = vec![
        user(1, "First", 2).await,
        row("assistant", 2, "Answer", 3).await,
    ];
    let incoming = vec![json!({"role":"user","content":"Current"})];
    let result = prepare_with_fetch(
        &metadata(&rows),
        vec![json!({"role":"system","content":"Instructions"})],
        4,
        &key(),
        "deepseek-v4-1-flash",
        ReasoningReplay::None,
        &incoming,
        5,
        100,
        |keys| Ok(fetch_rows(&rows, keys)),
    )
    .unwrap();
    assert_eq!(
        result.prompt.messages,
        vec![
            json!({"role":"system","content":"Instructions"}),
            json!({"role":"user","content":"First"}),
            json!({"role":"assistant","content":"Answer"}),
            json!({"role":"user","content":"Current"}),
        ]
    );
    assert_eq!(result.prompt.estimated_tokens, 14);
    assert!(!result.stats.truncated);
    assert!(result.prompt.compactions.is_empty());
}

#[tokio::test]
async fn legacy_window_keeps_first_user_and_recent_user_tail() {
    let mut rows = Vec::new();
    for id in 1..=6 {
        rows.push(if id % 2 == 1 {
            user(id, &format!("user-{id}"), 30_000).await
        } else {
            row("assistant", id, &format!("assistant-{id}"), 30_000).await
        });
    }
    let requested = RefCell::new(Vec::new());
    let incoming = vec![json!({"role":"user","content":"current"})];
    let result = prepare_with_fetch(
        &metadata(&rows),
        vec![json!({"role":"system","content":"Instructions"})],
        10,
        &key(),
        "gpt-oss-120b",
        ReasoningReplay::None,
        &incoming,
        5000,
        8192,
        |keys| {
            requested.borrow_mut().extend(keys.iter().map(|key| key.1));
            Ok(fetch_rows(&rows, keys))
        },
    )
    .unwrap();
    assert_eq!(*requested.borrow(), vec![1, 5, 6]);
    assert_eq!(
        result.prompt.messages,
        vec![
            json!({"role":"system","content":"Instructions"}),
            json!({"role":"user","content":"user-1"}),
            json!({"role":"assistant","content":TRUNCATION_MARKER}),
            json!({"role":"user","content":"user-5"}),
            json!({"role":"assistant","content":"assistant-6"}),
            json!({"role":"user","content":"current"}),
        ]
    );
    assert!(result.stats.truncated);
    assert!(result.prompt.estimated_tokens <= prompt_token_budget("gpt-oss-120b") - 8192);
}

#[tokio::test]
async fn current_reasoning_and_tool_pairs_match_existing_model_formatting() {
    let mut call = row("tool_call", 6, r#"{"query":"sample"}"#, 8).await;
    call.tool_call_id = Some(call.uuid);
    call.tool_name = Some("web_search".into());
    let mut output = row("tool_output", 7, "Synthetic result", 4).await;
    output.tool_call_id = call.tool_call_id;
    let rows = vec![
        user(1, "Earlier", 2).await,
        row("reasoning", 2, "Earlier private reasoning", 3).await,
        row("assistant", 3, "Earlier answer", 4).await,
        user(4, "Current", 2).await,
        row("reasoning", 5, "Current private reasoning", 3).await,
        call,
        output,
        row("reasoning", 8, "Final private reasoning", 3).await,
        row("assistant", 9, "Done", 1).await,
    ];
    let fetched = RefCell::new(Vec::new());
    let result = prepare_with_fetch(
        &metadata(&rows),
        vec![],
        0,
        &key(),
        "kimi-k3",
        ReasoningReplay::CurrentTurn,
        &[],
        0,
        0,
        |keys| {
            fetched.borrow_mut().extend(keys.iter().map(|key| key.1));
            Ok(fetch_rows(&rows, keys))
        },
    )
    .unwrap();
    assert!(!fetched.borrow().contains(&2));
    let prompt = &result.prompt.messages;
    assert!(prompt[1].get("reasoning").is_none());
    assert_eq!(prompt[3]["reasoning"], "Current private reasoning");
    assert_eq!(
        prompt[3]["tool_calls"][0]["function"]["arguments"],
        r#"{"query":"sample"}"#
    );
    assert_eq!(prompt[3]["tool_calls"][0]["id"], "functions.web_search:0");
    assert_eq!(prompt[4]["tool_call_id"], "functions.web_search:0");
    assert_eq!(prompt[5]["reasoning"], "Final private reasoning");
    assert_eq!(prompt[5]["content"], "Done");
    assert!(result.prompt.compactions.is_empty());
}

#[tokio::test]
async fn tool_pair_and_reasoning_survive_a_payload_batch_boundary() {
    let rows = rows_with_tool_pair_across_batch_boundary().await;
    let sizes = RefCell::new(Vec::new());
    let result = prepare_with_fetch(
        &metadata(&rows),
        vec![],
        0,
        &key(),
        "kimi-k3",
        ReasoningReplay::All,
        &[],
        0,
        0,
        |keys| {
            sizes.borrow_mut().push(keys.len());
            Ok(fetch_rows(&rows, keys))
        },
    )
    .unwrap();
    assert_eq!(*sizes.borrow(), vec![128, 1]);
    let prompt = &result.prompt.messages;
    assert_eq!(prompt[126]["reasoning"], "Current reasoning");
    assert_eq!(
        prompt[126]["tool_calls"][0]["id"],
        prompt[127]["tool_call_id"]
    );
    assert_eq!(result.stats.fetch_batches, 2);
}

#[tokio::test]
async fn disappearing_tool_output_discards_partial_projection_and_refreshes_metadata_once() {
    let rows = rows_with_tool_pair_across_batch_boundary().await;
    let ciphertext_bytes = |rows: &[RawThreadMessage]| {
        rows.iter()
            .filter_map(|row| row.content_enc.as_ref())
            .map(Vec::len)
            .sum::<usize>()
    };
    let first_batch_bytes = ciphertext_bytes(&rows[..128]);
    let retained_bytes = ciphertext_bytes(&rows[..126]);
    let retained_ciphertext: Vec<_> = rows[..126]
        .iter()
        .map(|row| row.content_enc.clone())
        .collect();
    let stored = RefCell::new(rows);
    let attempts = Cell::new(0);
    let fetches = Cell::new(0);
    let incoming = vec![json!({"role":"user","content":"Current input"})];
    let result = prepare_with_source_retry(|| {
        attempts.set(attempts.get() + 1);
        let fresh_metadata = metadata(&stored.borrow());
        let attempt = prepare_with_fetch(
            &fresh_metadata,
            vec![],
            0,
            &key(),
            "kimi-k3",
            ReasoningReplay::All,
            &incoming,
            2,
            4096,
            |keys| {
                fetches.set(fetches.get() + 1);
                if fetches.get() == 2 {
                    // A source writer deletes the earlier reasoning/tool step
                    // after its call was loaded, but before its output is read.
                    stored.borrow_mut().retain(|row| row.id <= 126);
                }
                Ok(fetch_rows(&stored.borrow(), keys))
            },
        );
        if attempts.get() == 1 {
            assert!(matches!(
                &attempt,
                Err(LegacyPreparationError::SourceChanged(_))
            ));
        }
        attempt
    })
    .unwrap();
    assert_eq!(attempts.get(), 2);
    assert_eq!(fetches.get(), 3);
    assert_eq!(result.prompt.messages.len(), 127);
    assert_eq!(result.prompt.messages.last(), incoming.last());
    assert!(result.prompt.messages.iter().all(|message| {
        message["role"] == "user"
            && message.get("tool_calls").is_none()
            && message.get("tool_call_id").is_none()
    }));
    assert_eq!(result.stats.metadata_rows, 126);
    assert_eq!(result.stats.selected_rows, 126);
    assert_eq!(result.stats.fetched_rows, 128 + 126);
    assert_eq!(result.stats.fetch_batches, 3);
    assert_eq!(
        result.stats.fetched_bytes,
        first_batch_bytes + retained_bytes
    );
    assert_eq!(
        result.stats.max_batch_bytes,
        first_batch_bytes.max(retained_bytes)
    );
    assert_eq!(
        retained_ciphertext,
        stored
            .borrow()
            .iter()
            .map(|row| row.content_enc.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn repeatedly_disappearing_selected_source_returns_retryable_error_after_two_attempts() {
    let rows = vec![user(1, "A selected source row", 4).await];
    let attempts = Cell::new(0);
    let result = prepare_with_source_retry(|| {
        attempts.set(attempts.get() + 1);
        prepare_with_fetch(
            &metadata(&rows),
            vec![],
            0,
            &key(),
            "deepseek-v4-1-flash",
            ReasoningReplay::None,
            &[],
            0,
            0,
            |_| Ok(vec![]),
        )
    });
    assert!(matches!(result, Err(ApiError::ServiceUnavailable)));
    assert_eq!(attempts.get(), 2);
}

#[tokio::test]
async fn identity_and_ciphertext_failures_do_not_trigger_source_retry() {
    let rows = vec![user(1, "A selected source row", 4).await];
    for corrupt_ciphertext in [false, true] {
        let attempts = Cell::new(0);
        let result = prepare_with_source_retry(|| {
            attempts.set(attempts.get() + 1);
            prepare_with_fetch(
                &metadata(&rows),
                vec![],
                0,
                &key(),
                "deepseek-v4-1-flash",
                ReasoningReplay::None,
                &[],
                0,
                0,
                |keys| {
                    let mut raw = fetch_rows(&rows, keys);
                    if corrupt_ciphertext {
                        raw[0].content_enc.as_mut().unwrap()[0] ^= 1;
                    } else {
                        raw[0].uuid = Uuid::new_v4();
                    }
                    Ok(raw)
                },
            )
        });
        assert!(matches!(result, Err(ApiError::InternalServerError)));
        assert_eq!(attempts.get(), 1);
    }
}

#[tokio::test]
async fn legacy_window_keeps_only_recent_user_when_head_and_marker_cannot_fit() {
    let model = "gpt-oss-120b";
    let window = prompt_token_budget(model) as i32;
    let rows = vec![
        user(1, "First", window / 3).await,
        row("assistant", 2, "First reply", window / 3).await,
        user(3, "Middle", window / 3).await,
        row("assistant", 4, "Middle reply", window / 3).await,
        user(5, "Most recent", window - 1000).await,
    ];
    let result = prepare_rows(&rows, model, ReasoningReplay::None);
    assert_eq!(
        result.prompt.messages,
        vec![json!({"role":"user","content":"Most recent"})]
    );
    assert_eq!(result.prompt.estimated_tokens, (window - 1000) as usize);
    assert!(result.stats.truncated);
}

#[tokio::test]
async fn legacy_window_keeps_system_and_recent_user_when_first_user_cannot_fit() {
    let model = "gpt-oss-120b";
    let window = prompt_token_budget(model) as i32;
    let rows = vec![
        user(1, "First", window / 3).await,
        row("assistant", 2, "First reply", window / 3).await,
        user(3, "Middle", window / 3).await,
        row("assistant", 4, "Middle reply", window / 3).await,
        user(5, "Most recent", window - 1000).await,
    ];
    let result = prepare_with_fetch(
        &metadata(&rows),
        vec![json!({"role":"system","content":"Required system"})],
        50,
        &key(),
        model,
        ReasoningReplay::None,
        &[],
        0,
        0,
        |keys| Ok(fetch_rows(&rows, keys)),
    )
    .unwrap();
    assert_eq!(
        result.prompt.messages,
        vec![
            json!({"role":"system","content":"Required system"}),
            json!({"role":"user","content":"Most recent"})
        ]
    );
    assert_eq!(
        result.prompt.estimated_tokens,
        50 + (window - 1000) as usize
    );
    assert!(result.stats.truncated);
}

#[tokio::test]
async fn legacy_truncation_keeps_or_drops_the_complete_recent_tool_exchange() {
    let model = "gpt-oss-120b";
    let window = prompt_token_budget(model) as i32;
    let mut call = row("tool_call", 4, "{}", window / 8).await;
    call.tool_call_id = Some(call.uuid);
    call.tool_name = Some("web_search".into());
    let mut output = row("tool_output", 5, "Result", window / 8).await;
    output.tool_call_id = call.tool_call_id;
    let rows = vec![
        user(1, "First", window / 32).await,
        row("assistant", 2, "Large middle answer", window * 3 / 4).await,
        user(3, "Recent", window / 8).await,
        call,
        output,
    ];
    for (requested_output, keeps_exchange) in [(0, true), (window as usize * 3 / 4, false)] {
        let result = prepare_with_fetch(
            &metadata(&rows),
            vec![],
            0,
            &key(),
            model,
            ReasoningReplay::None,
            &[],
            0,
            requested_output,
            |keys| Ok(fetch_rows(&rows, keys)),
        )
        .unwrap();
        let calls: Vec<_> = result
            .prompt
            .messages
            .iter()
            .filter_map(|message| message.get("tool_calls"))
            .collect();
        let outputs: Vec<_> = result
            .prompt
            .messages
            .iter()
            .filter_map(|message| message.get("tool_call_id"))
            .collect();
        assert_eq!(calls.len(), usize::from(keeps_exchange));
        assert_eq!(outputs.len(), calls.len());
        if keeps_exchange {
            assert_eq!(calls[0][0]["id"], *outputs[0]);
        }
        assert!(result.stats.truncated);
        assert!(result.prompt.estimated_tokens <= window as usize - requested_output);
    }
}

#[tokio::test]
async fn unmatched_historical_tools_are_not_fetched_or_replayed() {
    let mut call = row("tool_call", 2, "{}", 2).await;
    call.tool_call_id = Some(call.uuid);
    call.tool_name = Some("web_search".into());
    let mut output = row("tool_output", 4, "Unmatched", 2).await;
    output.tool_call_id = Some(Uuid::from_u128(99));
    let rows = vec![
        user(1, "Question", 1).await,
        call,
        user(3, "Later", 1).await,
        output,
    ];
    let result = prepare_rows(&rows, "deepseek-v4-1-flash", ReasoningReplay::None);
    assert_eq!(result.stats.fetched_rows, 2);
    assert_eq!(
        result.prompt.messages,
        vec![
            json!({"role":"user","content":"Question"}),
            json!({"role":"user","content":"Later"})
        ]
    );
}

#[tokio::test]
async fn multiple_large_encrypted_images_are_projected_per_batch_and_canonical_rows_unchanged() {
    let image = "A".repeat(9 * 1024 * 1024);
    let content = json!([{"type":"input_text","text":"Keep surrounding text"},
        {"type":"input_image","image_url":format!("data:image/png;base64,{image}")}])
    .to_string();
    let rows = vec![
        row("user", 1, &content, 10).await,
        row("user", 2, &content, 10).await,
        row("user", 3, &content, 10).await,
    ];
    let original: Vec<_> = rows.iter().map(|row| row.content_enc.clone()).collect();
    let batches = RefCell::new(Vec::new());
    let result = prepare_with_fetch(
        &metadata(&rows),
        vec![],
        0,
        &key(),
        "deepseek-v4-1-flash",
        ReasoningReplay::None,
        &[],
        0,
        0,
        |keys| {
            batches.borrow_mut().push(keys.len());
            Ok(fetch_rows(&rows, keys))
        },
    )
    .unwrap();
    assert_eq!(*batches.borrow(), vec![1, 1, 1]);
    assert!(result.stats.max_batch_bytes > SOURCE_BATCH_BYTES_TARGET);
    assert!(result.stats.max_batch_bytes < 10 * 1024 * 1024);
    assert!(result.stats.fetched_bytes > 27 * 1024 * 1024);
    assert!(result.stats.projected_bytes < 4096);
    assert!(result.prompt.estimated_tokens < 100);
    let projected = serde_json::to_string(&result.prompt.messages).unwrap();
    assert!(projected.contains("Keep surrounding text"));
    assert!(projected.contains("attachment omitted"));
    assert!(!projected.contains("image_url"));
    assert!(!projected.contains("data:image"));
    assert_eq!(
        original,
        rows.iter()
            .map(|row| row.content_enc.clone())
            .collect::<Vec<_>>()
    );
    assert!(result.prompt.compactions.is_empty());
}

#[tokio::test]
async fn image_description_tool_exchange_is_retained_separately() {
    let image =
        json!([{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]).to_string();
    let mut call = row("tool_call", 2, r#"{"image_index":1}"#, 5).await;
    call.tool_call_id = Some(call.uuid);
    call.tool_name = Some("read_image".into());
    let mut output = row("tool_output", 3, "A blue kite in a green field.", 8).await;
    output.tool_call_id = call.tool_call_id;
    let rows = vec![row("user", 1, &image, 10000).await, call, output];
    let result = prepare_rows(&rows, "deepseek-v4-1-flash", ReasoningReplay::None);
    assert_eq!(result.prompt.messages.len(), 3);
    assert_eq!(
        result.prompt.messages[1]["tool_calls"][0]["function"]["name"],
        "read_image"
    );
    assert_eq!(
        result.prompt.messages[2]["content"],
        "A blue kite in a green field."
    );
    assert!(result.prompt.estimated_tokens < 100);
}

#[test]
fn caller_output_and_current_input_room_are_reserved_before_any_fetch() {
    let fetched = std::cell::Cell::new(false);
    let result = prepare_with_fetch(
        &[],
        vec![json!({"role":"system","content":"Required"})],
        10,
        &key(),
        "gpt-oss-120b",
        ReasoningReplay::None,
        &[json!({"role":"user","content":"Current"})],
        2048,
        130000,
        |_| {
            fetched.set(true);
            Ok(vec![])
        },
    );
    assert!(matches!(
        result,
        Err(LegacyPreparationError::Api(
            ApiError::MessageExceedsContextLimit
        ))
    ));
    assert!(!fetched.get());
}

#[tokio::test]
async fn inferred_url_authority_comes_from_legacy_visible_source_not_assistant_text() {
    let rows = vec![
        user(1, "Inspect https://example.org/user-path", 4).await,
        row(
            "assistant",
            2,
            "Invented https://example.org/assistant-path",
            4,
        )
        .await,
    ];
    let result = prepare_rows(&rows, "deepseek-v4-1-flash", ReasoningReplay::None);
    assert!(result
        .prompt
        .allowed_urls
        .contains("https://example.org/user-path"));
    assert!(!result
        .prompt
        .allowed_urls
        .contains("https://example.org/assistant-path"));
}

#[tokio::test]
async fn incorrect_fetched_identity_and_tampered_ciphertext_fail_closed() {
    let rows = vec![user(1, "Harmless", 1).await];
    let meta = metadata(&rows);
    let result = prepare_with_fetch(
        &meta,
        vec![],
        0,
        &key(),
        "deepseek-v4-1-flash",
        ReasoningReplay::None,
        &[],
        0,
        0,
        |keys| {
            let mut raw = fetch_rows(&rows, keys);
            raw[0].uuid = Uuid::new_v4();
            Ok(raw)
        },
    );
    assert!(matches!(
        result,
        Err(LegacyPreparationError::Api(ApiError::InternalServerError))
    ));
    let result = prepare_with_fetch(
        &meta,
        vec![],
        0,
        &key(),
        "deepseek-v4-1-flash",
        ReasoningReplay::None,
        &[],
        0,
        0,
        |keys| {
            let mut raw = fetch_rows(&rows, keys);
            raw[0].content_enc.as_mut().unwrap()[0] ^= 1;
            Ok(raw)
        },
    );
    assert!(matches!(
        result,
        Err(LegacyPreparationError::Api(ApiError::InternalServerError))
    ));
}

#[tokio::test]
async fn legacy_null_assistant_does_not_consume_private_reasoning_for_later_answer() {
    let mut absent = row("assistant", 3, "unused", 1).await;
    absent.content_enc = None;
    let rows = vec![
        user(1, "Current", 1).await,
        row("reasoning", 2, "Private", 1).await,
        absent,
        row("assistant", 4, "Later answer", 2).await,
    ];
    let result = prepare_rows(&rows, "glm-5-3", ReasoningReplay::All);
    assert_eq!(result.prompt.messages.len(), 2);
    assert!(result.prompt.messages[1].get("reasoning").is_none());
}

#[test]
fn second_pass_preserves_required_system_when_stored_estimates_understate_payload() {
    let messages = vec![
        ProjectedMessage {
            role: ROLE_SYSTEM,
            prompt: json!({"role":"system","content":"Required"}),
            tok: 20,
        },
        ProjectedMessage {
            role: ROLE_USER,
            prompt: json!({"role":"user","content":"Huge"}),
            tok: 200,
        },
    ];
    assert!(matches!(
        fit_projected_window(messages, 100),
        Err(ApiError::MessageExceedsContextLimit)
    ));
}
