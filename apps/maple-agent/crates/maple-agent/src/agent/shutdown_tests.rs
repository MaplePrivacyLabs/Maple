//! Retained shutdown history must not masquerade as a runnable runtime.
use super::*;
use tests::{Harness, finished};

#[derive(Clone, Copy)]
enum ShutdownCaller {
    Host,
    Account,
    Runtime,
}

#[tokio::test]
async fn failed_shutdown_retains_source_writes_and_retry_starts_a_fresh_runtime() {
    for caller in [
        ShutdownCaller::Host,
        ShutdownCaller::Account,
        ShutdownCaller::Runtime,
    ] {
        let harness = Harness::new().await;
        let old_runtime = harness.runtime().await;
        let user = old_runtime.user_id.clone();
        let task = harness.create_task().await;
        harness.faux.push_text("Saved before shutdown.");
        let mut run = harness.send(&task, "Before shutdown").await;
        assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
        let session = old_runtime.loaded_session(&task).await.unwrap();
        let store = harness.service.host.store.as_ref().unwrap();

        // Use the real SQLite adapter: an unavailable store leaves an actual
        // Pi source entry queued in memory, rather than a fake service error.
        store.close_for_shutdown().unwrap();
        session.set_session_name("Retained shutdown name").await;
        assert!(session.has_unpersisted());
        store.record_persistence_failure(&crate::store::StoreError::DiskFull);
        let result = match caller {
            ShutdownCaller::Host => harness.service.shutdown().await,
            ShutdownCaller::Account => harness.service.drain_account(&user).await,
            ShutdownCaller::Runtime => harness.handle.stop().await.map(|_| ()),
        };
        assert!(result.is_err());
        assert!(old_runtime.lifetime.is_cancelled());
        assert!(harness.service.state.runtime.lock().await.is_none());
        assert!(
            harness
                .service
                .state
                .shutdown_pending
                .read()
                .await
                .is_some()
        );
        assert!(old_runtime.loaded_session(&task).await.is_some());
        assert!(session.has_unpersisted());

        // Account drain revokes the previous handle even when saving failed.
        let handle = harness.service.handle_for_user(&user).await.unwrap();
        assert!(!handle.status().await.unwrap().running);
        harness.service.resume_admission();
        assert_eq!(
            handle
                .start(crate::maple_api::test_maple_api_session(&user), None)
                .await
                .unwrap_err(),
            RUNTIME_SAVE_PENDING_ERROR
        );
        assert!(harness.service.flush_persistence().await.is_err());
        assert!(
            harness
                .service
                .state
                .shutdown_pending
                .read()
                .await
                .is_some()
        );

        store.initialize_plain().unwrap();
        harness.service.flush_persistence().await.unwrap();
        assert!(store.persistence_failure().is_none());
        assert!(
            harness
                .service
                .state
                .shutdown_pending
                .read()
                .await
                .is_none()
        );
        let replacement = harness.runtime().await;
        assert!(!Arc::ptr_eq(&old_runtime, &replacement));
        assert!(!replacement.lifetime.is_cancelled());
        assert!(handle.status().await.unwrap().running);
        assert!(!session.has_unpersisted());
        assert!(old_runtime.loaded_session(&task).await.is_none());
        assert_eq!(
            harness.faux.requests().len(),
            1,
            "retry never replays a prompt"
        );

        replacement.use_stream_fn(Arc::new(tests::SplitStream {
            task: harness.faux.clone(),
            side: pi_ai::faux::FauxProvider::new(),
        }));
        harness.faux.push_text("A fresh runtime can continue.");
        let mut run = handle
            .send_message(AgentSendMessageRequest {
                session_id: task.clone(),
                text: "Continue after saving".into(),
                model: Some("glm-5-3".into()),
                context_limit: Some(200_000),
                vision_capable: false,
                steer: false,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
        let reloaded = replacement.loaded_session(&task).await.unwrap();
        assert_eq!(
            reloaded.session_name().as_deref(),
            Some("Retained shutdown name")
        );
        harness.service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn cancelling_a_shutdown_request_keeps_the_drain_and_retained_sessions_owned() {
    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    let task = harness.create_task().await;
    harness.faux.push_text("A saved response.");
    let mut run = harness.send(&task, "A saved request").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let session = runtime.loaded_session(&task).await.unwrap();
    let store = harness.service.host.store.as_ref().unwrap();
    store.close_for_shutdown().unwrap();
    session
        .set_session_name("Saved after caller cancellation")
        .await;

    // Hold the real session registry through the drain's snapshot await, so
    // cancellation happens while the child task still owns unfinished drain.
    let stopped = runtime.hold_loaded_sessions_for_test().await;
    let service = harness.service.clone();
    let mut shutdown = tokio::spawn(async move { service.shutdown().await });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if harness
                .service
                .state
                .shutdown_pending
                .read()
                .await
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    shutdown.abort();
    let _ = (&mut shutdown).await;
    drop(stopped);
    assert!(harness.service.state.runtime.lock().await.is_none());
    let pending = harness
        .service
        .state
        .shutdown_pending
        .read()
        .await
        .clone()
        .unwrap();
    assert!(pending.wait().await.unwrap().is_err());
    assert!(session.has_unpersisted());
    store.initialize_plain().unwrap();
    harness.service.resume_admission();
    harness.service.flush_persistence().await.unwrap();
    assert!(harness.handle.status().await.unwrap().running);
    assert!(!session.has_unpersisted());
    harness.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_deletion_waits_for_retained_shutdown_history_even_after_store_is_writable() {
    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    let task = harness.create_task().await;
    harness.faux.push_text("Committed response.");
    let mut run = harness.send(&task, "Keep until saving succeeds").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let session = runtime.loaded_session(&task).await.unwrap();
    let store = harness.service.host.store.as_ref().unwrap();
    let account = runtime.store.account();
    store.close_for_shutdown().unwrap();
    session
        .set_session_name("Must survive attempted deletion")
        .await;
    assert!(harness.service.shutdown().await.is_err());
    assert!(session.has_unpersisted());

    // Restore actual SQLite writes first: refusing because the store is still
    // unavailable would not cover the tombstone/NotFound regression.
    store.initialize_plain().unwrap();
    harness.service.resume_admission();
    store
        .set_app_setting("writable-before-retry", &serde_json::json!(true), 1)
        .unwrap();
    assert_eq!(
        harness
            .handle
            .delete_session(task.clone())
            .await
            .unwrap_err(),
        RUNTIME_SAVE_PENDING_ERROR
    );
    assert!(store.task(account, &task).unwrap().is_some());
    assert!(store.deleted_tasks(10).unwrap().is_empty());
    assert!(session.has_unpersisted());

    harness.service.flush_persistence().await.unwrap();
    assert!(!session.has_unpersisted());
    let saved = store
        .load_session(account, &task, crate::store::SessionLoad::Full)
        .unwrap()
        .into_manager(&harness.project.path().to_string_lossy());
    assert_eq!(
        saved.session_name(),
        Some("Must survive attempted deletion")
    );
    harness.handle.delete_session(task.clone()).await.unwrap();
    assert!(store.task(account, &task).unwrap().is_none());
    harness.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn confirmed_reset_retains_unsaved_source_until_the_store_reset_succeeds() {
    use crate::store::lifecycle::{ERASE_CONFIRMATION, START_FRESH_CONFIRMATION};

    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    let task = harness.create_task().await;
    harness.faux.push_text("Committed before the reset.");
    let mut run = harness.send(&task, "Before the reset").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let session = runtime.loaded_session(&task).await.unwrap();
    let store = harness.service.host.store.as_ref().unwrap();
    let old_generation = store.store_id().unwrap();
    let account = runtime.store.account();
    store.close_for_shutdown().unwrap();
    session
        .set_session_name("Unsaved until reset confirmation")
        .await;
    assert!(session.has_unpersisted());

    // Ordinary Quit remains strict. Reset preparation stops the same runtime
    // without dropping the only in-memory source, even when saving still fails.
    assert!(harness.service.shutdown().await.is_err());
    harness.service.prepare_store_reset().await.unwrap();
    assert!(harness.service.state.runtime.lock().await.is_none());
    assert!(
        harness
            .service
            .state
            .shutdown_pending
            .read()
            .await
            .is_some()
    );
    assert!(runtime.loaded_session(&task).await.is_some());
    assert!(session.has_unpersisted());

    // Invalid confirmation and a valid request refused before reset commitment
    // both preserve history. The host must never call finish for either error.
    assert!(store.erase_local_data("wrong confirmation").is_err());
    store.initialize_plain().unwrap();
    assert!(store.start_fresh(START_FRESH_CONFIRMATION).is_err());
    assert_eq!(store.store_id().unwrap(), old_generation);
    assert!(store.task(account, &task).unwrap().is_some());
    assert!(runtime.loaded_session(&task).await.is_some());
    assert!(session.has_unpersisted());

    store.erase_local_data(ERASE_CONFIRMATION).unwrap();
    assert_ne!(store.store_id().unwrap(), old_generation);
    harness.service.finish_store_reset().await.unwrap();
    harness.service.invalidate_store_session();
    assert!(
        harness
            .service
            .state
            .shutdown_pending
            .read()
            .await
            .is_none()
    );
    assert!(runtime.loaded_session(&task).await.is_none());
    assert!(harness.handle.verify_generation().await.is_err());
    harness.service.resume_admission();

    let fresh = harness
        .service
        .handle_for_user(&runtime.user_id)
        .await
        .unwrap();
    assert!(fresh.list_sessions(None).await.unwrap().is_empty());
    fresh
        .start(
            crate::maple_api::test_maple_api_session(&runtime.user_id),
            Some(AgentStartRequest {
                project_root: Some(harness.project.path().to_string_lossy().into_owned()),
                model: Some("glm-5-3".into()),
            }),
        )
        .await
        .unwrap();
    assert!(fresh.status().await.unwrap().running);
    assert_eq!(
        harness.faux.requests().len(),
        1,
        "reset never replays a prompt"
    );
    harness.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn confirmed_reset_intent_failure_can_stop_and_recover_without_replaying_retired_source() {
    use crate::store::lifecycle::{ERASE_CONFIRMATION, LifecycleError, StoreMode};
    use crate::store::{Store, StoreState};

    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    let task = harness.create_task().await;
    harness.faux.push_text("Committed before the reset intent.");
    let mut run = harness.send(&task, "Before the reset intent").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let session = runtime.loaded_session(&task).await.unwrap();
    let store = harness.service.host.store.as_ref().unwrap().clone();
    let paths = store.paths().clone();
    let old_generation = store.lock_status().unwrap().store_id.unwrap();
    let original_metadata = std::fs::read(paths.generation()).unwrap();
    store.close_for_shutdown().unwrap();
    session
        .set_session_name("Retired only by confirmed reset intent")
        .await;
    assert!(session.has_unpersisted());
    assert!(harness.service.shutdown().await.is_err());
    harness.service.prepare_store_reset().await.unwrap();

    // Rejected confirmation publishes no intent and cannot authorize discarding
    // the actual pending source retained by the failed ordinary shutdown.
    assert!(matches!(
        store.erase_with_failed_intent_sync_for_test("incorrect confirmation"),
        Err(LifecycleError::Confirmation)
    ));
    assert!(!paths.setup().exists());
    assert!(runtime.loaded_session(&task).await.is_some());
    assert!(
        harness
            .service
            .state
            .shutdown_pending
            .read()
            .await
            .is_some()
    );

    // The real journal write and rename succeed; only its final directory sync
    // fails. RecoveryRequired is typed proof of the accepted reset boundary.
    let result = store.erase_with_failed_intent_sync_for_test(ERASE_CONFIRMATION);
    assert!(matches!(
        result,
        Err(LifecycleError::RecoveryRequired {
            mode: StoreMode::Plain
        })
    ));
    assert_eq!(store.state(), StoreState::Maintenance);
    let journal_bytes = std::fs::read(paths.setup()).unwrap();
    let journal: serde_json::Value = serde_json::from_slice(&journal_bytes).unwrap();
    assert_eq!(journal["after"], "erase");
    assert_eq!(journal["phase"], "retire_old");
    assert_eq!(journal["old_store_id"], old_generation);
    let expected_generation = journal["new_store_id"].as_str().unwrap().to_owned();
    assert_ne!(expected_generation, old_generation);
    assert_eq!(
        std::fs::read(paths.generation()).unwrap(),
        original_metadata
    );
    assert!(session.has_unpersisted());

    // The host may finish only because this exact confirmed reset returned the
    // typed recovery outcome. Ordinary Stop itself never gains a discard path.
    harness.service.finish_store_reset().await.unwrap();
    harness.service.invalidate_store_session();
    assert!(
        harness
            .service
            .state
            .shutdown_pending
            .read()
            .await
            .is_none()
    );
    assert!(runtime.loaded_session(&task).await.is_none());
    assert!(harness.handle.verify_generation().await.is_err());
    harness.service.shutdown().await.unwrap();
    store.close_for_shutdown().unwrap();
    assert_eq!(std::fs::read(paths.setup()).unwrap(), journal_bytes);

    // Reopen a fresh store handle after every old connection is closed. The
    // actual startup recovery installs the journal's reserved generation.
    let reopened = Store::unavailable(paths.clone(), StoreState::Maintenance).unwrap();
    reopened.initialize_recovered().unwrap();
    assert_eq!(reopened.state(), StoreState::Open);
    assert_eq!(
        reopened.lock_status().unwrap().store_id.as_deref(),
        Some(expected_generation.as_str())
    );
    assert!(reopened.account_id(&runtime.user_id).unwrap().is_none());
    assert!(!paths.setup().exists());
    assert_eq!(
        harness.faux.requests().len(),
        1,
        "reset recovery never replays a prompt"
    );
    assert!(harness.service.state.runtime.lock().await.is_none());
    reopened.close_for_shutdown().unwrap();
}
