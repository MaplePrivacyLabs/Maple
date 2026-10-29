//! Observer isolation through the real ACP transport and runtime broadcast.
use super::*;

#[tokio::test]
async fn more_than_a_broadcast_window_of_updates_waits_for_replay_without_starving_another_session()
{
    let harness = Harness::new().await;
    let (mut client, serving, context) = connect_with_context(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let blocked = client.new_session(harness.project.path()).await;
    let healthy = client.new_session(harness.project.path()).await;
    let blocked_operation = context.session_operations.lock().await[&blocked].clone();
    let healthy_operation = context.session_operations.lock().await[&healthy].clone();
    // Exercise the same gate owned by initial replay without needing timing
    // hooks in the SQL/source capture. Keep it held until B's frame arrives.
    let replay = Arc::clone(&blocked_operation.replay_gate)
        .lock_owned()
        .await;
    blocked_operation.replaying.store(true, Ordering::Release);
    for index in 0..400 {
        let before = context.observer_dispatches.load(Ordering::Acquire);
        harness.emit_timeline_item(
            &blocked,
            row("stream", "message", Some("assistant"), "a", "append"),
        );
        // Do not manufacture a genuine broadcast overflow by blocking this
        // single-threaded test runtime. Each event must reach the dispatcher.
        eventually(|| context.observer_dispatches.load(Ordering::Acquire) > before).await;
        if index == 128 {
            harness.emit_timeline_item(
                &healthy,
                row(
                    "healthy",
                    "message",
                    Some("assistant"),
                    "B stays live",
                    "replace",
                ),
            );
        }
    }
    client
        .wait_for_updates("B remains live while A owns replay_gate", |updates| {
            updates.iter().any(|update| {
                update["sessionId"] == healthy
                    && update["update"]["content"]["text"] == "B stays live"
            })
        })
        .await;
    assert!(!blocked_operation.cancellation.is_cancelled());
    assert_eq!(blocked_operation.failure_message(), None);
    assert!(!healthy_operation.cancellation.is_cancelled());
    assert!(!context.lifetime.is_cancelled());
    assert!(
        context
            .session_operation_is_current(&healthy, &healthy_operation)
            .await
    );
    blocked_operation.replaying.store(false, Ordering::Release);
    drop(replay);
    client
        .wait_for_updates("A resumes every queued append after replay", |updates| {
            updates
                .iter()
                .filter(|update| {
                    update["sessionId"] == blocked
                        && update["update"]["sessionUpdate"] == "agent_message_chunk"
                })
                .map(|update| {
                    update["update"]["content"]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .len()
                })
                .sum::<usize>()
                == 400
        })
        .await;
    let received: String = client
        .updates
        .iter()
        .filter(|update| {
            update["sessionId"] == blocked
                && update["update"]["sessionUpdate"] == "agent_message_chunk"
        })
        .map(|update| {
            update["update"]["content"]["text"]
                .as_str()
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(received, "a".repeat(400));
    assert!(!blocked_operation.cancellation.is_cancelled());
    client.call("session/list", json!({})).await;
    client.shutdown().await;
    finish(serving).await;
}

#[tokio::test]
async fn a_stale_observer_cannot_retire_a_reloaded_session_with_the_same_id() {
    let harness = Harness::new().await;
    let (mut client, serving, context) = connect_with_context(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let task = client.new_session(harness.project.path()).await;
    let stale = context.session_operations.lock().await[&task].clone();
    // Install a replacement generation while retaining the old worker's Arc.
    // Its late cleanup must not remove the new maps, lease, or session count.
    let replacement = AcpSessionOperation::new(&context.lifetime, stale.history_epoch);
    context
        .session_operations
        .lock()
        .await
        .insert(task.clone(), replacement.clone());
    let active = context.stats.active_sessions.load(Ordering::Acquire);
    context.retire_session_if_same(&task, &stale).await;
    assert!(stale.cancellation.is_cancelled());
    assert!(!replacement.cancellation.is_cancelled());
    assert!(
        context
            .session_operation_is_current(&task, &replacement)
            .await
    );
    assert!(context.sessions.lock().await.contains_key(&task));
    assert_eq!(
        context.stats.active_sessions.load(Ordering::Acquire),
        active
    );
    client.shutdown().await;
    finish(serving).await;
}

#[tokio::test]
async fn delayed_history_replacements_at_or_before_the_registered_epoch_are_ignored() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    append_test_messages(
        &harness.handle,
        &task,
        harness.project.path(),
        &["Saved history".to_owned()],
    );
    let (store, account) = harness.handle.test_persistence().unwrap();
    store.retire_task_timeline_for_test(account, &task).unwrap();
    harness
        .handle
        .ensure_task_history_ready(&task)
        .await
        .unwrap();
    let (mut client, serving, context) = connect_with_context(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    client
        .call(
            "session/load",
            json!({"sessionId":task,"cwd":harness.project.path(),"mcpServers":[]}),
        )
        .await;
    let operation = context.session_operations.lock().await[&task].clone();
    assert!(operation.history_epoch > 0);
    assert_eq!(
        operation.history_epoch,
        store.task_history_stamp(account, &task).unwrap().rows_epoch
    );
    let replay = Arc::clone(&operation.replay_gate).lock_owned().await;
    operation.replaying.store(true, Ordering::Release);
    // Deliberately dispatch after registration, while replay owns its gate.
    // These are the notifications a completed preparation can leave queued.
    for rows_epoch in [operation.history_epoch - 1, operation.history_epoch] {
        let before = context.observer_dispatches.load(Ordering::Acquire);
        harness.emit_service_event(crate::agent::AgentServiceEvent::HistoryReplaced {
            user_id: context.agent.user_id().to_owned(),
            session_id: task.clone(),
            rows_epoch,
        });
        eventually(|| context.observer_dispatches.load(Ordering::Acquire) > before).await;
        assert!(!operation.cancellation.is_cancelled());
        assert_eq!(operation.failure_message(), None);
        assert!(
            context
                .session_operation_is_current(&task, &operation)
                .await
        );
    }
    operation.replaying.store(false, Ordering::Release);
    drop(replay);
    harness.faux.push_text("The session remains attached.");
    assert_eq!(
        client.prompt(&task, "Continue.").await["stopReason"],
        "end_turn"
    );
    assert_eq!(
        client.texts("agent_message_chunk")[0].1,
        "The session remains attached."
    );
    assert!(!context.lifetime.is_cancelled());
    client.shutdown().await;
    finish(serving).await;
}

#[tokio::test]
async fn a_history_replacement_invalidates_only_its_own_operation() {
    let harness = Harness::new().await;
    let (mut client, serving, context) = connect_with_context(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let changed = client.new_session(harness.project.path()).await;
    let healthy = client.new_session(harness.project.path()).await;
    let changed_operation = context.session_operations.lock().await[&changed].clone();
    let healthy_operation = context.session_operations.lock().await[&healthy].clone();
    let replay = Arc::clone(&changed_operation.replay_gate)
        .lock_owned()
        .await;
    changed_operation.replaying.store(true, Ordering::Release);
    // Run HistoryReplaced is another host interface changing this task; this
    // client has not originated a prompt, so it must fail even behind replay.
    harness.emit_timeline_item(
        &changed,
        row("stream", "message", Some("assistant"), "queued", "append"),
    );
    // The account-scoped service event is routed directly, rather than being
    // stuck behind that queued timeline row and its held replay gate.
    let event = crate::agent::AgentServiceEvent::HistoryReplaced {
        user_id: context.agent.user_id().to_owned(),
        session_id: changed.clone(),
        rows_epoch: changed_operation.history_epoch + 1,
    };
    // The private fixture emitter exposes timeline events only. Publish through
    // the same host dispatcher the runtime uses for this event variant.
    harness.emit_service_event(event);
    eventually(|| changed_operation.cancellation.is_cancelled()).await;
    assert!(
        changed_operation
            .failure_message()
            .unwrap()
            .contains("replaced")
    );
    assert!(!healthy_operation.cancellation.is_cancelled());
    assert!(!context.lifetime.is_cancelled());
    drop(replay);
    client.shutdown().await;
    finish(serving).await;
}

#[tokio::test]
async fn observer_overflow_invalidates_only_the_session_that_exhausted_its_budget() {
    let harness = Harness::new().await;
    let (mut client, serving, context) = connect_with_context(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let blocked = client.new_session(harness.project.path()).await;
    let healthy = client.new_session(harness.project.path()).await;
    let blocked_operation = context.session_operations.lock().await[&blocked].clone();
    let healthy_operation = context.session_operations.lock().await[&healthy].clone();
    let replay = Arc::clone(&blocked_operation.replay_gate)
        .lock_owned()
        .await;
    blocked_operation.replaying.store(true, Ordering::Release);
    for _ in 0..=crate::acp::observer::MAX_OBSERVER_EVENTS {
        let before = context.observer_dispatches.load(Ordering::Acquire);
        harness.emit_timeline_item(
            &blocked,
            row("stream", "message", Some("assistant"), "a", "append"),
        );
        eventually(|| context.observer_dispatches.load(Ordering::Acquire) > before).await;
    }
    assert!(blocked_operation.cancellation.is_cancelled());
    assert!(
        blocked_operation
            .failure_message()
            .unwrap()
            .contains("backlog")
    );
    harness.emit_timeline_item(
        &healthy,
        row(
            "healthy",
            "message",
            Some("assistant"),
            "Only A overflowed",
            "replace",
        ),
    );
    client
        .wait_for_updates(
            "B remains attached after A exceeds its queue budget",
            |updates| {
                updates.iter().any(|update| {
                    update["sessionId"] == healthy
                        && update["update"]["content"]["text"] == "Only A overflowed"
                })
            },
        )
        .await;
    assert!(!healthy_operation.cancellation.is_cancelled());
    assert!(!context.lifetime.is_cancelled());
    assert!(
        context
            .session_operation_is_current(&healthy, &healthy_operation)
            .await
    );
    drop(replay);
    client.call("session/list", json!({})).await;
    client.shutdown().await;
    finish(serving).await;
}
