use super::*;
use crate::store::{NewTask, StorePaths, TaskKind};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, Store, AccountId, AccountId, String) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(StorePaths::new(directory.path())).unwrap();
    let alice = store.upsert_account("Alice", 1).unwrap();
    let bob = store.upsert_account("Bob", 1).unwrap();
    let task = uuid::Uuid::new_v4().to_string();
    store
        .create_task(
            alice,
            &NewTask {
                uuid: task.clone(),
                project_root: directory.path().to_string_lossy().into_owned(),
                kind: TaskKind::Desktop,
                title: "Synthetic task".into(),
                provisional: false,
                model: None,
                created_ms: 1,
            },
        )
        .unwrap();
    (directory, store, alice, bob, task)
}

fn tokens(label: &str) -> CredentialTokens {
    CredentialTokens {
        access_token: SecretString::new(format!("synthetic-access-{label}")),
        refresh_token: Some(SecretString::new(format!("synthetic-refresh-{label}"))),
    }
}

fn namespace(profile: &str) -> RemoteNamespace {
    RemoteNamespace {
        api_url: "http://127.0.0.1:1234".into(),
        client_id: "synthetic-client".into(),
        profile: profile.into(),
    }
}

fn mcp(value: McpSecretUpdate) -> McpServerEdit {
    McpServerEdit {
        key: "example".into(),
        name: "Example".into(),
        description: "Synthetic server".into(),
        enabled: true,
        timeout_seconds: 30,
        target: McpTarget::Stdio {
            command: "synthetic-mcp --serve".into(),
        },
        environment: vec![McpSecretEdit {
            name: "TOKEN".into(),
            update: value,
        }],
        headers: vec![],
    }
}

fn http_mcp(environment: McpSecretUpdate, header: McpSecretUpdate) -> McpServerEdit {
    let mut definition = mcp(environment);
    definition.target = McpTarget::StreamableHttp {
        url: "https://example.invalid/mcp".into(),
    };
    definition.headers.push(McpSecretEdit {
        name: "Authorization".into(),
        update: header,
    });
    definition
}

#[test]
fn credential_rotation_and_clear_bind_account_session_and_revision() {
    let (_directory, store, alice, bob, _task) = fixture();
    let first = store
        .replace_credentials(
            None,
            alice,
            "http://127.0.0.1:1234",
            "session-a",
            0,
            &tokens("one"),
            1,
        )
        .unwrap();
    let rotated = store.rotate_credentials(&first, &tokens("two"), 2).unwrap();
    assert_eq!(rotated.revision, 1);
    assert_eq!(
        store.rotate_credentials(&first, &tokens("stale"), 3),
        Err(StoreError::Conflict)
    );
    assert!(!store.clear_credentials(&first).unwrap());
    assert_eq!(store.credentials("http://other.invalid").unwrap(), None);
    assert_eq!(
        store
            .credentials("http://127.0.0.1:1234")
            .unwrap()
            .unwrap()
            .tokens,
        tokens("two")
    );
    assert!(store.clear_credentials(&rotated).unwrap());
    assert_eq!(
        store.rotate_credentials(&rotated, &tokens("resurrect"), 4),
        Err(StoreError::Conflict)
    );
    let next = store
        .replace_credentials(
            None,
            bob,
            "http://127.0.0.1:1234",
            "session-b",
            0,
            &tokens("bob"),
            5,
        )
        .unwrap();
    assert!(!store.clear_credentials(&rotated).unwrap());
    assert_eq!(
        store.replace_credentials(
            Some(&rotated),
            alice,
            "http://127.0.0.1:1234",
            "session-c",
            2,
            &tokens("stale-login"),
            6
        ),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store
            .credentials("http://127.0.0.1:1234")
            .unwrap()
            .unwrap()
            .owner,
        next
    );
}

#[test]
fn sign_in_observes_filtered_slot_owner_without_exposing_foreign_origin_tokens() {
    let (_directory, store, alice, bob, _task) = fixture();
    let owner = store
        .replace_credentials(
            None,
            alice,
            "https://old.example",
            "session-a",
            3,
            &tokens("alice"),
            1,
        )
        .unwrap();
    assert_eq!(store.credentials("https://new.example").unwrap(), None);
    assert_eq!(
        store.credential_owner_for_sign_in().unwrap(),
        Some(owner.clone())
    );
    assert_eq!(
        store.replace_credentials(
            None,
            bob,
            "https://new.example",
            "session-b",
            1,
            &tokens("bob"),
            2
        ),
        Err(StoreError::Conflict)
    );
    let replacement = store
        .replace_credentials(
            Some(&owner),
            bob,
            "https://new.example",
            "session-b",
            1,
            &tokens("bob"),
            2,
        )
        .unwrap();
    assert_eq!(store.credentials("https://old.example").unwrap(), None);
    assert_eq!(
        store
            .credentials("https://new.example")
            .unwrap()
            .unwrap()
            .tokens,
        tokens("bob")
    );
    assert_eq!(
        store.rotate_credentials(&owner, &tokens("late-alice"), 3),
        Err(StoreError::Conflict)
    );
    assert!(!store.clear_credentials(&owner).unwrap());
    assert_eq!(
        store.credential_owner_for_sign_in().unwrap(),
        Some(replacement)
    );
}

#[test]
fn pending_deletion_reserves_physical_slot_even_for_another_origin_and_exact_owner() {
    let (_directory, store, alice, bob, _task) = fixture();
    let owner = store
        .replace_credentials(
            None,
            alice,
            "https://old.example",
            "session-a",
            3,
            &tokens("alice"),
            1,
        )
        .unwrap();
    let generation = uuid::Uuid::new_v4().to_string();
    store.begin_account_deletion(alice, &generation, 2).unwrap();
    assert_eq!(
        store.credential_owner_for_sign_in(),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.replace_credentials(
            Some(&owner),
            bob,
            "https://new.example",
            "session-b",
            1,
            &tokens("bob"),
            3
        ),
        Err(StoreError::Conflict)
    );
    let retained = store
        .pending_deletion_credentials(alice, &generation, "https://old.example")
        .unwrap()
        .unwrap();
    assert_eq!(retained.owner, owner);
    assert_eq!(retained.tokens, tokens("alice"));
    store.abort_account_deletion(alice, &generation).unwrap();
    assert_eq!(store.credential_owner_for_sign_in().unwrap(), Some(owner));
}

#[test]
fn confirmed_deletion_releases_only_target_credentials_before_history_collection() {
    let (_directory, store, alice, bob, task) = fixture();
    let owner = store
        .replace_credentials(
            None,
            alice,
            "https://api.example",
            "session-a",
            3,
            &tokens("alice"),
            1,
        )
        .unwrap();
    let generation = uuid::Uuid::new_v4().to_string();
    store.begin_account_deletion(alice, &generation, 2).unwrap();
    store.confirm_account_deletion(alice, &generation).unwrap();
    assert_eq!(store.collect_account_step(alice), Err(StoreError::Busy));
    assert_eq!(store.credential_owner_for_sign_in().unwrap(), None);
    let replacement = store
        .replace_credentials(
            None,
            bob,
            "https://api.example",
            "session-b",
            1,
            &tokens("bob"),
            3,
        )
        .unwrap();
    store.confirm_account_deletion(alice, &generation).unwrap();
    assert_eq!(
        store.credential_owner_for_sign_in().unwrap(),
        Some(replacement.clone())
    );
    assert_eq!(
        store.rotate_credentials(&owner, &tokens("late-alice"), 4),
        Err(StoreError::Conflict)
    );
    assert!(store.collect_task_step(alice, &task).unwrap().done);
    let mut collected = false;
    for _ in 0..32 {
        if store.collect_account_step(alice).unwrap().done {
            collected = true;
            break;
        }
    }
    assert!(collected);
    assert_eq!(
        store.credential_owner_for_sign_in().unwrap(),
        Some(replacement)
    );
    assert_eq!(
        store
            .credentials("https://api.example")
            .unwrap()
            .unwrap()
            .tokens,
        tokens("bob")
    );
}

#[test]
fn deletion_confirmation_credential_failure_rolls_back_phase_and_task_tombstones() {
    let (_directory, store, alice, _bob, task) = fixture();
    let owner = store
        .replace_credentials(
            None,
            alice,
            "https://api.example",
            "session-a",
            3,
            &tokens("alice"),
            1,
        )
        .unwrap();
    let generation = uuid::Uuid::new_v4().to_string();
    store.begin_account_deletion(alice, &generation, 2).unwrap();
    store
        .writer
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_credential_delete BEFORE DELETE ON credentials \
         BEGIN SELECT RAISE(ABORT, 'synthetic credential delete failure'); END;",
        )
        .unwrap();
    assert!(store.confirm_account_deletion(alice, &generation).is_err());
    assert!(matches!(
        store.account_deletion(alice).unwrap(),
        Some(crate::store::AccountDeletion::Pending { .. })
    ));
    assert!(store.task(alice, &task).unwrap().is_some());
    assert_eq!(
        store
            .pending_deletion_credentials(alice, &generation, "https://api.example")
            .unwrap()
            .unwrap()
            .owner,
        owner
    );
    store
        .writer
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_credential_delete;")
        .unwrap();
    store.confirm_account_deletion(alice, &generation).unwrap();
    assert_eq!(store.credential_owner_for_sign_in().unwrap(), None);
    assert!(store.task(alice, &task).unwrap().is_none());
}

#[test]
fn same_revision_claim_is_idempotent_but_cannot_replace_token_values() {
    let (_directory, store, alice, _bob, _task) = fixture();
    let owner = store
        .replace_credentials(
            None,
            alice,
            "http://127.0.0.1:1234",
            "session-a",
            7,
            &tokens("one"),
            1,
        )
        .unwrap();
    assert_eq!(
        store
            .replace_credentials(
                Some(&owner),
                alice,
                "http://127.0.0.1:1234",
                "session-a",
                7,
                &tokens("one"),
                2
            )
            .unwrap(),
        owner
    );
    assert_eq!(
        store.replace_credentials(
            Some(&owner),
            alice,
            "http://127.0.0.1:1234",
            "session-a",
            7,
            &tokens("different"),
            3
        ),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.replace_credentials(
            Some(&owner),
            alice,
            "http://127.0.0.1:1234",
            "session-a",
            6,
            &tokens("older"),
            4
        ),
        Err(StoreError::Conflict)
    );
    let saved = store.credentials("http://127.0.0.1:1234").unwrap().unwrap();
    assert_eq!(saved.tokens, tokens("one"));
    assert!(!format!("{saved:?} {:?}", saved.tokens).contains("synthetic-access"));
}

#[test]
fn absent_refresh_token_round_trips_without_weakening_access_validation() {
    let (_directory, store, alice, _bob, _task) = fixture();
    let without_refresh = CredentialTokens {
        access_token: SecretString::new("synthetic-access-only".into()),
        refresh_token: None,
    };
    let owner = store
        .replace_credentials(
            None,
            alice,
            "http://127.0.0.1:1234",
            "session-a",
            0,
            &without_refresh,
            1,
        )
        .unwrap();
    assert_eq!(
        store
            .credentials("http://127.0.0.1:1234")
            .unwrap()
            .unwrap()
            .tokens,
        without_refresh
    );
    let empty_access = CredentialTokens {
        access_token: SecretString::new(String::new()),
        refresh_token: None,
    };
    assert_eq!(
        store.rotate_credentials(&owner, &empty_access, 2),
        Err(StoreError::InvalidInput)
    );
    let empty_refresh = CredentialTokens {
        access_token: SecretString::new("synthetic-access".into()),
        refresh_token: Some(SecretString::new(String::new())),
    };
    assert_eq!(
        store.rotate_credentials(&owner, &empty_refresh, 2),
        Err(StoreError::InvalidInput)
    );
}

#[test]
fn pending_deletion_credentials_are_only_available_to_exact_operation_reconciliation() {
    let (_directory, store, alice, bob, _task) = fixture();
    store
        .replace_credentials(
            None,
            alice,
            "http://127.0.0.1:1234",
            "session-a",
            0,
            &tokens("pending"),
            1,
        )
        .unwrap();
    let generation = uuid::Uuid::new_v4().to_string();
    store.begin_account_deletion(alice, &generation, 2).unwrap();
    assert_eq!(store.credentials("http://127.0.0.1:1234").unwrap(), None);
    assert_eq!(
        store
            .pending_account_deletion("http://other.invalid")
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .pending_account_deletion("http://127.0.0.1:1234")
            .unwrap(),
        Some((alice, "Alice".into(), generation.clone(), 2))
    );
    let saved = store
        .pending_deletion_credentials(alice, &generation, "http://127.0.0.1:1234")
        .unwrap()
        .unwrap();
    assert_eq!(saved.owner.account, alice);
    assert_eq!(saved.tokens, tokens("pending"));
    assert_eq!(
        store.pending_deletion_credentials(bob, &generation, "http://127.0.0.1:1234"),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.pending_deletion_credentials(
            alice,
            &uuid::Uuid::new_v4().to_string(),
            "http://127.0.0.1:1234"
        ),
        Err(StoreError::Conflict)
    );
    store.confirm_account_deletion(alice, &generation).unwrap();
    assert_eq!(
        store
            .pending_account_deletion("http://127.0.0.1:1234")
            .unwrap(),
        None
    );
    assert_eq!(
        store.pending_deletion_credentials(alice, &generation, "http://127.0.0.1:1234"),
        Err(StoreError::Conflict)
    );
}

#[test]
fn pending_deletion_refresh_is_fenced_by_generation_owner_and_api() {
    let (_directory, store, alice, bob, _task) = fixture();
    let api = "http://127.0.0.1:1234";
    let owner = store
        .replace_credentials(
            None,
            alice,
            api,
            "session-original",
            2,
            &tokens("original"),
            1,
        )
        .unwrap();
    let generation = uuid::Uuid::new_v4().to_string();
    store.begin_account_deletion(alice, &generation, 2).unwrap();
    assert_eq!(
        store.rotate_credentials(&owner, &tokens("normal-refresh"), 3),
        Err(StoreError::Conflict)
    );
    assert_eq!(store.clear_credentials(&owner), Err(StoreError::Conflict));
    let wrong_account = CredentialOwner {
        account: bob,
        ..owner.clone()
    };
    let wrong_generation = uuid::Uuid::new_v4().to_string();
    for (expected, operation, requested_api) in [
        (&wrong_account, generation.as_str(), api),
        (&owner, wrong_generation.as_str(), api),
        (&owner, generation.as_str(), "http://other.invalid"),
    ] {
        assert_eq!(
            store.replace_pending_deletion_credentials(
                expected,
                operation,
                requested_api,
                "session-restored",
                0,
                &tokens("rejected"),
                3
            ),
            Err(StoreError::Conflict)
        );
    }
    let refreshed = store
        .replace_pending_deletion_credentials(
            &owner,
            &generation,
            api,
            "session-restored",
            0,
            &tokens("refreshed"),
            4,
        )
        .unwrap();
    assert_eq!(store.credentials(api).unwrap(), None);
    let pending = store
        .pending_deletion_credentials(alice, &generation, api)
        .unwrap()
        .unwrap();
    assert_eq!(pending.owner, refreshed);
    assert_eq!(pending.tokens, tokens("refreshed"));
    assert_eq!(
        store.replace_pending_deletion_credentials(
            &owner,
            &generation,
            api,
            "session-stale",
            1,
            &tokens("stale"),
            5
        ),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.replace_pending_deletion_credentials(
            &refreshed,
            &generation,
            api,
            "session-restored",
            0,
            &tokens("different"),
            5
        ),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store
            .replace_pending_deletion_credentials(
                &refreshed,
                &generation,
                api,
                "session-restored",
                0,
                &tokens("refreshed"),
                5
            )
            .unwrap(),
        refreshed
    );
    store.confirm_account_deletion(alice, &generation).unwrap();
    assert_eq!(
        store.replace_pending_deletion_credentials(
            &refreshed,
            &generation,
            api,
            "session-restored",
            1,
            &tokens("after-confirm"),
            6
        ),
        Err(StoreError::Conflict)
    );
}

#[test]
fn failed_rotation_does_not_publish_new_owner_or_clear_saved_pair() {
    let (_directory, store, alice, _bob, _task) = fixture();
    let owner = store
        .replace_credentials(
            None,
            alice,
            "http://127.0.0.1:1234",
            "session-a",
            0,
            &tokens("saved"),
            1,
        )
        .unwrap();
    store.write(Durability::Durable,|tx| {
        tx.execute_batch("CREATE TEMP TRIGGER synthetic_rotation_failure BEFORE UPDATE ON credentials BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;")?;
        Ok(())
    }).unwrap();
    let error = store
        .rotate_credentials(&owner, &tokens("rejected"), 2)
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("synthetic"));
    let saved = store.credentials("http://127.0.0.1:1234").unwrap().unwrap();
    assert_eq!(saved.owner, owner);
    assert_eq!(saved.tokens, tokens("saved"));
}

#[test]
fn deleting_accounts_cannot_install_or_inject_privileged_records() {
    let (_directory, store, alice, _bob, _task) = fixture();
    let generation = uuid::Uuid::new_v4().to_string();
    store.begin_account_deletion(alice, &generation, 2).unwrap();
    assert_eq!(
        store.replace_credentials(
            None,
            alice,
            "http://127.0.0.1:1234",
            "session-a",
            0,
            &tokens("one"),
            3
        ),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.remote_identity(alice, &namespace("local")),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.save_mcp_definitions(alice, &[mcp(McpSecretUpdate::Keep)], McpEditOrigin::Local),
        Err(StoreError::Conflict)
    );
}

#[test]
fn receipt_reservation_is_canonical_account_bound_and_immutable() {
    let (_directory, store, alice, bob, task) = fixture();
    let id = uuid::Uuid::new_v4().to_string();
    let first = json!({"text":"synthetic prompt","sessionId":task,"nested":{"b":2,"a":1}});
    let second: Value = serde_json::from_str(&format!(
        "{{\"nested\":{{\"a\":1,\"b\":2}},\"sessionId\":\"{task}\",\"text\":\"synthetic prompt\"}}"
    ))
    .unwrap();
    assert_eq!(
        store.reserve_send(alice, &id, &task, &first, 1).unwrap(),
        None
    );
    assert_eq!(
        store.reserve_send(alice, &id, &task, &second, 2).unwrap(),
        Some(SendOutcome::Pending)
    );
    assert_eq!(
        store.reserve_send(alice, &id, &task, &json!({"different":"content"}), 3),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.reserve_send(bob, &id, &task, &first, 3),
        Err(StoreError::Conflict)
    );
    assert_eq!(store.send_outcome(bob, &id).unwrap(), SendOutcome::Missing);
    assert_eq!(
        store.finish_send(bob, &id, &Ok(json!("wrong-account"))),
        Err(StoreError::NotFound)
    );
    let result = Ok(json!("synthetic-run"));
    store.finish_send(alice, &id, &result).unwrap();
    store.finish_send(alice, &id, &result).unwrap();
    assert_eq!(
        store.finish_send(alice, &id, &Ok(json!("different-run"))),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.send_outcome(alice, &id).unwrap(),
        SendOutcome::Finished(result)
    );
}

#[test]
fn receipt_restart_keeps_uncertain_ids_reserved_and_prune_keeps_them() {
    let (directory, store, alice, _bob, task) = fixture();
    let pending = uuid::Uuid::new_v4().to_string();
    let finished = uuid::Uuid::new_v4().to_string();
    let parameters = json!({"sessionId":task,"text":"synthetic"});
    store
        .reserve_send(alice, &pending, &task, &parameters, 1)
        .unwrap();
    store
        .reserve_send(alice, &finished, &task, &parameters, 1)
        .unwrap();
    store
        .finish_send(alice, &finished, &Ok(json!("run")))
        .unwrap();
    store.close().unwrap();
    drop(store);
    let reopened = Store::open(StorePaths::new(directory.path())).unwrap();
    assert_eq!(
        reopened.send_outcome(alice, &pending).unwrap(),
        SendOutcome::Interrupted
    );
    assert_eq!(
        reopened
            .reserve_send(alice, &pending, &task, &parameters, 2)
            .unwrap(),
        Some(SendOutcome::Interrupted)
    );
    assert_eq!(
        reopened.finish_send(alice, &pending, &Ok(json!("late"))),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        reopened
            .prune_finished_send_receipts(alice, 8 * 24 * 60 * 60 * 1000)
            .unwrap(),
        1
    );
    assert_eq!(
        reopened.send_outcome(alice, &finished).unwrap(),
        SendOutcome::Missing
    );
    assert_eq!(
        reopened.send_outcome(alice, &pending).unwrap(),
        SendOutcome::Interrupted
    );
}

#[test]
fn unavailable_store_never_answers_missing_or_accepts_a_send() {
    let (_directory, store, alice, _bob, task) = fixture();
    let paths = store.paths().clone();
    store.close().unwrap();
    let locked = Store::unavailable(paths, super::super::StoreState::Locked).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    assert_eq!(locked.send_outcome(alice, &id), Err(StoreError::Locked));
    assert_eq!(
        locked.reserve_send(alice, &id, &task, &json!({}), 1),
        Err(StoreError::Locked)
    );
}

#[test]
fn mcp_values_never_enter_definitions_and_keep_is_bound_to_target() {
    let (_directory, store, alice, bob, _task) = fixture();
    let secret = "synthetic-mcp-secret";
    let configured = mcp(McpSecretUpdate::Set(SecretString::new(secret.into())));
    store
        .save_mcp_definitions(
            alice,
            std::slice::from_ref(&configured),
            McpEditOrigin::Local,
        )
        .unwrap();
    let listed = store.mcp_definitions(alice).unwrap();
    assert!(listed[0].environment[0].has_value);
    assert!(!serde_json::to_string(&listed).unwrap().contains(secret));
    let launch = store
        .mcp_launch_secrets(alice, "example", &configured.target)
        .unwrap();
    assert_eq!(launch.environment["TOKEN"].expose(), secret);
    assert!(!format!("{launch:?}").contains(secret));
    assert_eq!(
        store.mcp_launch_secrets(bob, "example", &configured.target),
        Err(StoreError::NotFound)
    );
    store
        .save_mcp_definitions(alice, &[mcp(McpSecretUpdate::Keep)], McpEditOrigin::Local)
        .unwrap();
    assert!(store.mcp_definitions(alice).unwrap()[0].environment[0].has_value);
    let mut redirected = mcp(McpSecretUpdate::Keep);
    redirected.target = McpTarget::Stdio {
        command: "synthetic-other --capture".into(),
    };
    store
        .save_mcp_definitions(alice, &[redirected.clone()], McpEditOrigin::Local)
        .unwrap();
    assert!(!store.mcp_definitions(alice).unwrap()[0].environment[0].has_value);
    assert_eq!(
        store.mcp_launch_secrets(alice, "example", &configured.target),
        Err(StoreError::Conflict)
    );
    assert!(
        store
            .mcp_launch_secrets(alice, "example", &redirected.target)
            .unwrap()
            .environment
            .is_empty()
    );
}

#[test]
fn remote_mcp_values_can_be_created_kept_replaced_and_cleared_without_readback() {
    let (_directory, store, alice, bob, _task) = fixture();
    let configured = http_mcp(
        McpSecretUpdate::Set(SecretString::new("synthetic-env-original".into())),
        McpSecretUpdate::Set(SecretString::new("synthetic-header-original".into())),
    );
    store
        .save_mcp_definitions(
            alice,
            std::slice::from_ref(&configured),
            McpEditOrigin::Remote,
        )
        .unwrap();
    let listed = store.mcp_definitions(alice).unwrap();
    assert!(listed[0].environment[0].has_value);
    assert_eq!(listed[0].headers[0].name, "authorization");
    assert!(listed[0].headers[0].has_value);
    assert!(
        !serde_json::to_string(&listed)
            .unwrap()
            .contains("synthetic-env-original")
    );
    assert!(!format!("{listed:?}").contains("synthetic-header-original"));
    assert!(store.mcp_definitions(bob).unwrap().is_empty());
    assert_eq!(
        store.mcp_launch_secrets(bob, "example", &configured.target),
        Err(StoreError::NotFound)
    );
    let mut kept = http_mcp(McpSecretUpdate::Keep, McpSecretUpdate::Keep);
    kept.description = "Changed display label".into();
    store
        .save_mcp_definitions(alice, &[kept], McpEditOrigin::Remote)
        .unwrap();
    let launch = store
        .mcp_launch_secrets(alice, "example", &configured.target)
        .unwrap();
    assert_eq!(
        launch.environment["TOKEN"].expose(),
        "synthetic-env-original"
    );
    assert_eq!(
        launch.headers["authorization"].expose(),
        "synthetic-header-original"
    );
    assert!(!format!("{launch:?}").contains("synthetic-env-original"));
    assert!(!format!("{launch:?}").contains("synthetic-header-original"));

    let replaced = http_mcp(
        McpSecretUpdate::Set(SecretString::new("synthetic-env-replaced".into())),
        McpSecretUpdate::Set(SecretString::new("synthetic-header-replaced".into())),
    );
    store
        .save_mcp_definitions(alice, &[replaced], McpEditOrigin::Remote)
        .unwrap();
    let launch = store
        .mcp_launch_secrets(alice, "example", &configured.target)
        .unwrap();
    assert_eq!(
        launch.environment["TOKEN"].expose(),
        "synthetic-env-replaced"
    );
    assert_eq!(
        launch.headers["authorization"].expose(),
        "synthetic-header-replaced"
    );
    let listed = serde_json::to_string(&store.mcp_definitions(alice).unwrap()).unwrap();
    assert!(!listed.contains("synthetic-env-replaced"));
    assert!(!listed.contains("synthetic-header-replaced"));

    store
        .save_mcp_definitions(
            alice,
            &[http_mcp(McpSecretUpdate::Clear, McpSecretUpdate::Clear)],
            McpEditOrigin::Remote,
        )
        .unwrap();
    let listed = store.mcp_definitions(alice).unwrap();
    assert!(!listed[0].environment[0].has_value);
    assert!(!listed[0].headers[0].has_value);
    let launch = store
        .mcp_launch_secrets(alice, "example", &configured.target)
        .unwrap();
    assert!(launch.environment.is_empty());
    assert!(launch.headers.is_empty());
}

#[test]
fn remote_target_changes_require_explicit_updates_and_roll_back_the_whole_save() {
    let (_directory, store, alice, _bob, _task) = fixture();
    let configured = http_mcp(
        McpSecretUpdate::Set(SecretString::new("synthetic-env-original".into())),
        McpSecretUpdate::Set(SecretString::new("synthetic-header-original".into())),
    );
    let mut sibling = mcp(McpSecretUpdate::Keep);
    sibling.key = "sibling".into();
    sibling.name = "Sibling".into();
    store
        .save_mcp_definitions(
            alice,
            &[sibling.clone(), configured.clone()],
            McpEditOrigin::Remote,
        )
        .unwrap();
    let before = store.mcp_definitions(alice).unwrap();
    sibling.description = "Must be rolled back".into();
    for (environment, header) in [
        (McpSecretUpdate::Keep, McpSecretUpdate::Keep),
        (McpSecretUpdate::Clear, McpSecretUpdate::Keep),
        (McpSecretUpdate::Keep, McpSecretUpdate::Clear),
    ] {
        let mut redirected = http_mcp(environment, header);
        redirected.target = McpTarget::StreamableHttp {
            url: "https://other.invalid/mcp".into(),
        };
        assert_eq!(
            store.save_mcp_definitions(
                alice,
                &[sibling.clone(), redirected],
                McpEditOrigin::Remote
            ),
            Err(StoreError::Conflict)
        );
        assert_eq!(store.mcp_definitions(alice).unwrap(), before);
    }
    let mut omitted = http_mcp(McpSecretUpdate::Clear, McpSecretUpdate::Clear);
    omitted.target = McpTarget::StreamableHttp {
        url: "https://other.invalid/mcp".into(),
    };
    omitted.headers.clear();
    assert_eq!(
        store.save_mcp_definitions(alice, &[sibling.clone(), omitted], McpEditOrigin::Remote),
        Err(StoreError::Conflict)
    );
    assert_eq!(store.mcp_definitions(alice).unwrap(), before);
    let launch = store
        .mcp_launch_secrets(alice, "example", &configured.target)
        .unwrap();
    assert_eq!(
        launch.environment["TOKEN"].expose(),
        "synthetic-env-original"
    );
    assert_eq!(
        launch.headers["authorization"].expose(),
        "synthetic-header-original"
    );

    let mut redirected = http_mcp(
        McpSecretUpdate::Set(SecretString::new("synthetic-env-new-target".into())),
        McpSecretUpdate::Clear,
    );
    redirected.target = McpTarget::StreamableHttp {
        url: "https://other.invalid/mcp".into(),
    };
    store
        .save_mcp_definitions(alice, &[sibling, redirected.clone()], McpEditOrigin::Remote)
        .unwrap();
    assert_eq!(
        store.mcp_launch_secrets(alice, "example", &configured.target),
        Err(StoreError::Conflict)
    );
    let launch = store
        .mcp_launch_secrets(alice, "example", &redirected.target)
        .unwrap();
    assert_eq!(
        launch.environment["TOKEN"].expose(),
        "synthetic-env-new-target"
    );
    assert!(launch.headers.is_empty());
}

#[test]
fn remote_http_to_stdio_transition_clears_headers_without_retaining_header_metadata() {
    let (_directory, store, alice, _bob, _task) = fixture();
    let configured = http_mcp(
        McpSecretUpdate::Set(SecretString::new("synthetic-http-env".into())),
        McpSecretUpdate::Set(SecretString::new("synthetic-http-header".into())),
    );
    store
        .save_mcp_definitions(alice, &[configured], McpEditOrigin::Remote)
        .unwrap();
    let before = store.mcp_definitions(alice).unwrap();
    let mut redirected = mcp(McpSecretUpdate::Set(SecretString::new(
        "synthetic-stdio-env".into(),
    )));
    for update in [
        McpSecretUpdate::Keep,
        McpSecretUpdate::Set(SecretString::new("synthetic-invalid-stdio-header".into())),
    ] {
        redirected.headers = vec![McpSecretEdit {
            name: "Authorization".into(),
            update,
        }];
        assert_eq!(
            store.save_mcp_definitions(
                alice,
                std::slice::from_ref(&redirected),
                McpEditOrigin::Remote
            ),
            Err(StoreError::InvalidInput)
        );
        assert_eq!(store.mcp_definitions(alice).unwrap(), before);
    }
    redirected.headers = vec![McpSecretEdit {
        name: "Authorization".into(),
        update: McpSecretUpdate::Clear,
    }];
    store
        .save_mcp_definitions(
            alice,
            std::slice::from_ref(&redirected),
            McpEditOrigin::Remote,
        )
        .unwrap();
    let listed = store.mcp_definitions(alice).unwrap();
    assert!(listed[0].headers.is_empty());
    let launch = store
        .mcp_launch_secrets(alice, "example", &redirected.target)
        .unwrap();
    assert!(launch.headers.is_empty());
    assert_eq!(launch.environment["TOKEN"].expose(), "synthetic-stdio-env");
    assert_eq!(
        store
            .read(|tx| Ok(tx.query_row(
                "SELECT count(*) FROM mcp_server_secrets WHERE kind='header'",
                [],
                |row| row.get::<_, i64>(0)
            )?))
            .unwrap(),
        0
    );
}

#[test]
fn remote_mcp_omission_and_server_removal_delete_values_only_in_the_owning_account() {
    let (_directory, store, alice, bob, _task) = fixture();
    let configured = mcp(McpSecretUpdate::Set(SecretString::new(
        "synthetic-value".into(),
    )));
    for account in [alice, bob] {
        store
            .save_mcp_definitions(
                account,
                std::slice::from_ref(&configured),
                McpEditOrigin::Remote,
            )
            .unwrap();
    }
    let mut omitted = mcp(McpSecretUpdate::Keep);
    omitted.environment.clear();
    store
        .save_mcp_definitions(alice, &[omitted], McpEditOrigin::Remote)
        .unwrap();
    assert!(
        store
            .mcp_launch_secrets(alice, "example", &configured.target)
            .unwrap()
            .environment
            .is_empty()
    );
    store
        .save_mcp_definitions(
            alice,
            std::slice::from_ref(&configured),
            McpEditOrigin::Remote,
        )
        .unwrap();
    let mut renamed = mcp(McpSecretUpdate::Keep);
    renamed.key = "renamed".into();
    renamed.name = "Renamed".into();
    store
        .save_mcp_definitions(alice, &[renamed], McpEditOrigin::Remote)
        .unwrap();
    assert!(
        store
            .mcp_launch_secrets(alice, "renamed", &configured.target)
            .unwrap()
            .environment
            .is_empty()
    );
    store
        .save_mcp_definitions(
            alice,
            std::slice::from_ref(&configured),
            McpEditOrigin::Remote,
        )
        .unwrap();
    store
        .save_mcp_definitions(alice, &[], McpEditOrigin::Remote)
        .unwrap();
    assert!(store.mcp_definitions(alice).unwrap().is_empty());
    assert_eq!(
        store.mcp_launch_secrets(alice, "example", &configured.target),
        Err(StoreError::NotFound)
    );
    assert_eq!(
        store
            .mcp_launch_secrets(bob, "example", &configured.target)
            .unwrap()
            .environment["TOKEN"]
            .expose(),
        "synthetic-value"
    );
    assert_eq!(
        store
            .read(|tx| Ok(tx
                .query_row("SELECT count(*) FROM mcp_server_secrets", [], |row| row
                    .get::<_, i64>(0))?))
            .unwrap(),
        1
    );
}

#[test]
fn http_header_names_are_canonical_and_target_change_with_new_value_is_local() {
    let (_directory, store, alice, _bob, _task) = fixture();
    let mut definition = mcp(McpSecretUpdate::Keep);
    definition.target = McpTarget::StreamableHttp {
        url: "HTTP://EXAMPLE.INVALID:80/mcp".into(),
    };
    definition.environment.clear();
    definition.headers = vec![McpSecretEdit {
        name: "Authorization".into(),
        update: McpSecretUpdate::Set(SecretString::new("synthetic-token".into())),
    }];
    store
        .save_mcp_definitions(alice, &[definition], McpEditOrigin::Local)
        .unwrap();
    let listed = store.mcp_definitions(alice).unwrap();
    assert_eq!(
        listed[0].headers[0],
        McpSecretPresence {
            name: "authorization".into(),
            has_value: true
        }
    );
    assert_eq!(
        store
            .mcp_launch_secrets(alice, "example", &listed[0].target)
            .unwrap()
            .headers["authorization"]
            .expose(),
        "synthetic-token"
    );
    let mut duplicate = mcp(McpSecretUpdate::Keep);
    duplicate.environment.push(McpSecretEdit {
        name: "TOKEN".into(),
        update: McpSecretUpdate::Keep,
    });
    assert_eq!(
        store.save_mcp_definitions(alice, &[duplicate], McpEditOrigin::Local),
        Err(StoreError::InvalidInput)
    );
    assert_eq!(store.mcp_definitions(alice).unwrap(), listed);
}

#[test]
fn remote_identity_is_stable_scoped_and_survives_credential_clear() {
    let (directory, store, alice, bob, _task) = fixture();
    let ns = namespace("local");
    assert_eq!(store.saved_remote_identity(alice, &ns).unwrap(), None);
    let identity = store.remote_identity(alice, &ns).unwrap();
    assert!(!identity.enabled);
    assert_eq!(store.remote_identity(alice, &ns).unwrap(), identity);
    assert_ne!(
        store.remote_identity(bob, &ns).unwrap().secret,
        identity.secret
    );
    assert_ne!(
        store
            .remote_identity(alice, &namespace("dev"))
            .unwrap()
            .secret,
        identity.secret
    );
    let owner = store
        .replace_credentials(
            None,
            alice,
            "http://127.0.0.1:1234",
            "session-a",
            0,
            &tokens("one"),
            1,
        )
        .unwrap();
    store.clear_credentials(&owner).unwrap();
    store.close().unwrap();
    drop(store);
    let reopened = Store::open(StorePaths::new(directory.path())).unwrap();
    assert_eq!(
        reopened.saved_remote_identity(alice, &ns).unwrap(),
        Some(identity)
    );
}

#[test]
fn pairing_confirmation_attempts_and_discovery_preserve_authority() {
    let (directory, store, alice, _bob, _task) = fixture();
    let ns = namespace("local");
    let key = "11".repeat(32);
    let unknown = "22".repeat(32);
    store.remote_identity(alice, &ns).unwrap();
    store
        .reserve_remote_pairing_attempt(alice, &ns, &key, [1; 16])
        .unwrap();
    assert_eq!(
        store.reserve_remote_pairing_attempt(alice, &ns, &key, [1; 16]),
        Err(StoreError::Conflict)
    );
    store
        .remember_remote_host_proof(alice, &ns, &key, [2; 16], "Synthetic peer")
        .unwrap();
    assert_eq!(
        store.confirm_remote_host(alice, &ns, &key, [3; 16]),
        Err(StoreError::Conflict)
    );
    assert!(
        !store
            .saved_remote_identity(alice, &ns)
            .unwrap()
            .unwrap()
            .peers[&key]
            .can_connect
    );
    store
        .confirm_remote_host(alice, &ns, &key, [2; 16])
        .unwrap();
    store
        .grant_remote_peer(
            alice,
            &ns,
            &key,
            RemotePeer {
                name: "Synthetic peer".into(),
                grant: [4; 16],
                controls_this_computer: true,
                can_connect: false,
                host_grant: None,
                pending_confirmation: None,
            },
        )
        .unwrap();
    let before = store.saved_remote_identity(alice, &ns).unwrap().unwrap();
    assert!(before.peers[&key].can_connect && before.peers[&key].controls_this_computer);
    assert_eq!(before.peers[&key].host_grant, Some([2; 16]));
    store
        .refresh_remote_peer_names(
            alice,
            &ns,
            &BTreeMap::from([
                (key.clone(), "New label".into()),
                (unknown, "Unknown".into()),
            ]),
        )
        .unwrap();
    let after = store.saved_remote_identity(alice, &ns).unwrap().unwrap();
    assert_eq!(after.peers.len(), 1);
    assert_eq!(after.peers[&key].grant, before.peers[&key].grant);
    assert_eq!(after.peers[&key].host_grant, before.peers[&key].host_grant);
    assert_eq!(after.attempted_windows, before.attempted_windows);
    assert!(!after.enabled);
    store.close().unwrap();
    drop(store);
    let reopened = Store::open(StorePaths::new(directory.path())).unwrap();
    assert_eq!(
        reopened
            .saved_remote_identity(alice, &ns)
            .unwrap()
            .unwrap()
            .peers[&key]
            .host_grant,
        Some([2; 16])
    );
    assert_eq!(
        reopened.reserve_remote_pairing_attempt(alice, &ns, &key, [1; 16]),
        Err(StoreError::Conflict)
    );
    reopened.revoke_remote_peer(alice, &ns, &key).unwrap();
    let saved = reopened.saved_remote_identity(alice, &ns).unwrap().unwrap();
    assert!(saved.peers.is_empty());
    assert_eq!(saved.attempted_windows[&key], vec![[1; 16]]);
}

fn incoming_remote_peer(grant: [u8; 16]) -> RemotePeer {
    RemotePeer {
        name: "Synthetic peer".into(),
        grant,
        controls_this_computer: true,
        can_connect: false,
        host_grant: None,
        pending_confirmation: None,
    }
}

#[test]
fn remote_host_confirmation_is_durable_and_pending_proof_is_not_authority() {
    let (directory, store, alice, _bob, _task) = fixture();
    let ns = namespace("local");
    let key = "11".repeat(32);
    let window = [1; 16];
    store.remote_identity(alice, &ns).unwrap();
    store
        .reserve_remote_pairing_attempt(alice, &ns, &key, window)
        .unwrap();
    store
        .remember_remote_host_proof(alice, &ns, &key, window, "Synthetic peer")
        .unwrap();
    store.close().unwrap();
    drop(store);

    let store = Store::open(StorePaths::new(directory.path())).unwrap();
    let pending = store.saved_remote_identity(alice, &ns).unwrap().unwrap();
    assert_eq!(pending.peers[&key].pending_confirmation, Some(window));
    assert_eq!(pending.peers[&key].host_grant, None);
    assert!(!pending.peers[&key].can_connect);
    assert_eq!(
        store.confirm_remote_host(alice, &ns, &key, [2; 16]),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.saved_remote_identity(alice, &ns).unwrap(),
        Some(pending)
    );
    store.confirm_remote_host(alice, &ns, &key, window).unwrap();
    store.close().unwrap();
    drop(store);

    let reopened = Store::open(StorePaths::new(directory.path())).unwrap();
    let saved = reopened.saved_remote_identity(alice, &ns).unwrap().unwrap();
    let peer = &saved.peers[&key];
    assert_eq!(peer.host_grant, Some(window));
    assert_eq!(peer.pending_confirmation, None);
    assert!(peer.can_connect);
    assert_eq!(peer.grant, [0; 16]);
    assert!(!peer.controls_this_computer);
    assert_eq!(saved.attempted_windows[&key], vec![window]);
}

#[test]
fn mutual_pairing_keeps_both_directions_across_sequential_and_interleaved_acks() {
    for interleaved in [false, true] {
        let (a_directory, a, a_account, _bob, _task) = fixture();
        let (b_directory, b, b_account, _bob, _task) = fixture();
        let ns = namespace("local");
        let a_key = "11".repeat(32);
        let b_key = "22".repeat(32);
        let a_window = [1; 16];
        let b_window = [2; 16];
        a.remote_identity(a_account, &ns).unwrap();
        b.remote_identity(b_account, &ns).unwrap();
        a.reserve_remote_pairing_attempt(a_account, &ns, &b_key, b_window)
            .unwrap();
        a.remember_remote_host_proof(a_account, &ns, &b_key, b_window, "B")
            .unwrap();
        if interleaved {
            b.reserve_remote_pairing_attempt(b_account, &ns, &a_key, a_window)
                .unwrap();
            b.remember_remote_host_proof(b_account, &ns, &a_key, a_window, "A")
                .unwrap();
            a.grant_remote_peer(a_account, &ns, &b_key, incoming_remote_peer(a_window))
                .unwrap();
        }
        b.grant_remote_peer(b_account, &ns, &a_key, incoming_remote_peer(b_window))
            .unwrap();
        a.confirm_remote_host(a_account, &ns, &b_key, b_window)
            .unwrap();
        if !interleaved {
            b.reserve_remote_pairing_attempt(b_account, &ns, &a_key, a_window)
                .unwrap();
            b.remember_remote_host_proof(b_account, &ns, &a_key, a_window, "A")
                .unwrap();
            a.grant_remote_peer(a_account, &ns, &b_key, incoming_remote_peer(a_window))
                .unwrap();
        }
        b.confirm_remote_host(b_account, &ns, &a_key, a_window)
            .unwrap();
        a.close().unwrap();
        b.close().unwrap();
        drop(a);
        drop(b);

        let a = Store::open(StorePaths::new(a_directory.path())).unwrap();
        let b = Store::open(StorePaths::new(b_directory.path())).unwrap();
        let a_saved = a.saved_remote_identity(a_account, &ns).unwrap().unwrap();
        let b_saved = b.saved_remote_identity(b_account, &ns).unwrap().unwrap();
        let a_peer = &a_saved.peers[&b_key];
        let b_peer = &b_saved.peers[&a_key];
        assert_eq!(a_peer.grant, a_window);
        assert_eq!(b_peer.grant, b_window);
        assert_eq!(a_peer.host_grant, Some(b_peer.grant));
        assert_eq!(b_peer.host_grant, Some(a_peer.grant));
        assert!(a_peer.can_connect && a_peer.controls_this_computer);
        assert!(b_peer.can_connect && b_peer.controls_this_computer);
        assert_eq!(a_peer.pending_confirmation, None);
        assert_eq!(b_peer.pending_confirmation, None);
    }
}

#[test]
fn incoming_pairing_cannot_create_outgoing_authority_or_pending_proof() {
    let (_directory, store, alice, _bob, _task) = fixture();
    let ns = namespace("local");
    let key = "11".repeat(32);
    store.remote_identity(alice, &ns).unwrap();
    let mut supplied = incoming_remote_peer([1; 16]);
    supplied.can_connect = true;
    supplied.host_grant = Some([2; 16]);
    supplied.pending_confirmation = Some([3; 16]);
    store.grant_remote_peer(alice, &ns, &key, supplied).unwrap();
    let saved = store.saved_remote_identity(alice, &ns).unwrap().unwrap();
    assert_eq!(saved.peers[&key], incoming_remote_peer([1; 16]));
    assert_eq!(
        store.confirm_remote_host(alice, &ns, &key, [3; 16]),
        Err(StoreError::Conflict)
    );
    store
        .reserve_remote_pairing_attempt(alice, &ns, &key, [2; 16])
        .unwrap();
    let saved = store.saved_remote_identity(alice, &ns).unwrap().unwrap();
    assert_eq!(
        store.recover_remote_host_grant(
            alice,
            &ns,
            &key,
            &saved.peers[&key],
            &saved.attempted_windows[&key],
            [2; 16],
        ),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.saved_remote_identity(alice, &ns).unwrap(),
        Some(saved)
    );
}

fn legacy_remote_host(
    store: &Store,
    account: AccountId,
    ns: &RemoteNamespace,
    key: &str,
    incoming_grant: Option<[u8; 16]>,
) -> RemoteIdentity {
    store.remote_identity(account, ns).unwrap();
    store
        .reserve_remote_pairing_attempt(account, ns, key, [1; 16])
        .unwrap();
    let legacy: RemotePeer = serde_json::from_value(json!({
        "name": "Legacy peer",
        "grant": incoming_grant.unwrap_or([0; 16]),
        "controls_this_computer": incoming_grant.is_some(),
        "can_connect": true,
        "pending_confirmation": null,
    }))
    .unwrap();
    assert_eq!(legacy.host_grant, None);
    assert!(
        serde_json::to_value(&legacy)
            .unwrap()
            .get("host_grant")
            .is_none()
    );
    store
        .edit_remote(account, ns, Durability::Durable, |identity| {
            identity.peers.insert(key.to_owned(), legacy);
            Ok(())
        })
        .unwrap();
    store.saved_remote_identity(account, ns).unwrap().unwrap()
}

#[test]
fn legacy_host_grant_recovery_is_durable_and_changes_only_confirmed_outgoing_binding() {
    for incoming_grant in [None, Some([4; 16])] {
        let (directory, store, alice, _bob, _task) = fixture();
        let ns = namespace("local");
        let key = "11".repeat(32);
        let before = legacy_remote_host(&store, alice, &ns, &key, incoming_grant);
        assert!(!before.enabled);
        assert_eq!(
            store.recover_remote_host_grant(
                alice,
                &ns,
                &key,
                &before.peers[&key],
                &before.attempted_windows[&key],
                [9; 16],
            ),
            Err(StoreError::Conflict)
        );
        assert_eq!(
            store.saved_remote_identity(alice, &ns).unwrap(),
            Some(before.clone())
        );
        store
            .recover_remote_host_grant(
                alice,
                &ns,
                &key,
                &before.peers[&key],
                &before.attempted_windows[&key],
                [1; 16],
            )
            .unwrap();
        let mut expected = before.clone();
        expected.peers.get_mut(&key).unwrap().host_grant = Some([1; 16]);
        assert_eq!(
            store.saved_remote_identity(alice, &ns).unwrap(),
            Some(expected.clone())
        );
        assert_eq!(
            store.recover_remote_host_grant(
                alice,
                &ns,
                &key,
                &before.peers[&key],
                &before.attempted_windows[&key],
                [1; 16],
            ),
            Err(StoreError::Conflict)
        );
        assert_eq!(
            store.recover_remote_host_grant(
                alice,
                &ns,
                &key,
                &expected.peers[&key],
                &expected.attempted_windows[&key],
                [1; 16],
            ),
            Err(StoreError::Conflict)
        );
        store.close().unwrap();
        drop(store);
        let reopened = Store::open(StorePaths::new(directory.path())).unwrap();
        assert_eq!(
            reopened.saved_remote_identity(alice, &ns).unwrap(),
            Some(expected)
        );
    }
}

#[test]
fn host_grant_recovery_preserves_a_new_discovery_name() {
    let (_directory, store, alice, _bob, _task) = fixture();
    let ns = namespace("local");
    let key = "11".repeat(32);
    let before = legacy_remote_host(&store, alice, &ns, &key, Some([4; 16]));
    store
        .refresh_remote_peer_names(
            alice,
            &ns,
            &BTreeMap::from([(key.clone(), "Changed peer".into())]),
        )
        .unwrap();
    let mut expected = store.saved_remote_identity(alice, &ns).unwrap().unwrap();
    expected.peers.get_mut(&key).unwrap().host_grant = Some([1; 16]);
    store
        .recover_remote_host_grant(
            alice,
            &ns,
            &key,
            &before.peers[&key],
            &before.attempted_windows[&key],
            [1; 16],
        )
        .unwrap();
    let saved = store.saved_remote_identity(alice, &ns).unwrap().unwrap();
    assert_eq!(saved, expected);
    assert_eq!(saved.peers[&key].name, "Changed peer");
}

#[test]
fn host_grant_recovery_rejects_changed_authority_attempts_pending_proof_and_revocation() {
    for changed in ["authority", "attempts", "pending", "revoked"] {
        let (_directory, store, alice, _bob, _task) = fixture();
        let ns = namespace("local");
        let key = "11".repeat(32);
        let before = legacy_remote_host(&store, alice, &ns, &key, Some([4; 16]));
        match changed {
            "authority" => store
                .grant_remote_peer(alice, &ns, &key, incoming_remote_peer([5; 16]))
                .unwrap(),
            "attempts" => store
                .reserve_remote_pairing_attempt(alice, &ns, &key, [2; 16])
                .unwrap(),
            "pending" => store
                .remember_remote_host_proof(alice, &ns, &key, [2; 16], "Legacy peer")
                .unwrap(),
            "revoked" => store.revoke_remote_peer(alice, &ns, &key).unwrap(),
            _ => unreachable!(),
        }
        let changed_state = store.saved_remote_identity(alice, &ns).unwrap().unwrap();
        assert_eq!(
            store.recover_remote_host_grant(
                alice,
                &ns,
                &key,
                &before.peers[&key],
                &before.attempted_windows[&key],
                [1; 16],
            ),
            Err(StoreError::Conflict)
        );
        if changed == "pending" {
            assert_eq!(
                store.recover_remote_host_grant(
                    alice,
                    &ns,
                    &key,
                    &changed_state.peers[&key],
                    &changed_state.attempted_windows[&key],
                    [1; 16],
                ),
                Err(StoreError::Conflict)
            );
        }
        assert_eq!(
            store.saved_remote_identity(alice, &ns).unwrap(),
            Some(changed_state)
        );
    }
}

#[test]
fn zero_host_acknowledgements_never_become_confirmed_authority() {
    let (directory, store, alice, _bob, _task) = fixture();
    let ns = namespace("local");
    let key = "11".repeat(32);
    store.remote_identity(alice, &ns).unwrap();
    store
        .reserve_remote_pairing_attempt(alice, &ns, &key, [0; 16])
        .unwrap();
    store
        .remember_remote_host_proof(alice, &ns, &key, [0; 16], "Synthetic peer")
        .unwrap();
    let pending = store.saved_remote_identity(alice, &ns).unwrap().unwrap();
    assert_eq!(pending.peers[&key].host_grant, None);
    assert_eq!(
        store.confirm_remote_host(alice, &ns, &key, [0; 16]),
        Err(StoreError::InvalidInput)
    );
    assert_eq!(
        store.saved_remote_identity(alice, &ns).unwrap(),
        Some(pending)
    );
    let legacy = legacy_remote_host(&store, alice, &ns, &key, None);
    assert!(legacy.attempted_windows[&key].contains(&[0; 16]));
    assert_eq!(
        store.recover_remote_host_grant(
            alice,
            &ns,
            &key,
            &legacy.peers[&key],
            &legacy.attempted_windows[&key],
            [0; 16],
        ),
        Err(StoreError::InvalidInput)
    );
    store.close().unwrap();
    drop(store);
    let reopened = Store::open(StorePaths::new(directory.path())).unwrap();
    assert_eq!(
        reopened.saved_remote_identity(alice, &ns).unwrap(),
        Some(legacy)
    );
}

#[test]
fn persisted_zero_or_unpermitted_confirmed_host_grants_are_invalid() {
    for (grant, can_connect) in [([0; 16], true), ([1; 16], false)] {
        let (_directory, store, alice, _bob, _task) = fixture();
        let ns = namespace("local");
        let key = "11".repeat(32);
        store.remote_identity(alice, &ns).unwrap();
        let mut peer = incoming_remote_peer([2; 16]);
        peer.host_grant = Some(grant);
        peer.can_connect = can_connect;
        let record = serde_json::to_string(&RemoteRecord {
            version: 1,
            peers: BTreeMap::from([(key, peer)]),
            attempted_windows: BTreeMap::new(),
        })
        .unwrap();
        store
            .write(Durability::Durable, |tx| {
                tx.execute(
                    "UPDATE remote_identities SET record=?2 WHERE account_id=?1",
                    params![alice.0, record],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store.saved_remote_identity(alice, &ns),
            Err(StoreError::InvalidData)
        );
    }
}

#[test]
fn remote_record_does_not_duplicate_the_secret_and_rejects_malformed_json() {
    let (_directory, store, alice, _bob, _task) = fixture();
    let ns = namespace("local");
    store.remote_identity(alice, &ns).unwrap();
    let json = store
        .read(|tx| {
            Ok(tx.query_row(
                "SELECT record FROM remote_identities WHERE account_id=?1",
                [alice.0],
                |row| row.get::<_, String>(0),
            )?)
        })
        .unwrap();
    assert!(!json.contains("secret"));
    store
        .write(Durability::Durable, |tx| {
            tx.execute(
                "UPDATE remote_identities SET record='{}' WHERE account_id=?1",
                [alice.0],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        store.saved_remote_identity(alice, &ns),
        Err(StoreError::InvalidData)
    );
}
