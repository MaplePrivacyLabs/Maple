use serde_json::json;

use super::*;
use crate::store::tasks::{NewTask, TaskKind, TaskRecord};
use crate::store::{Durability, StorePaths};

fn fixture() -> (tempfile::TempDir, Store, AccountId, AccountId) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(StorePaths::new(directory.path())).unwrap();
    let alice = store.upsert_account("alice", 1).unwrap();
    let bob = store.upsert_account("bob", 1).unwrap();
    (directory, store, alice, bob)
}

fn task(store: &Store, account: AccountId, number: u64) -> TaskRecord {
    store
        .create_task(
            account,
            &NewTask {
                uuid: format!("00000000-0000-7000-8000-{number:012x}"),
                project_root: if cfg!(windows) {
                    "C:/fixture"
                } else {
                    "/fixture"
                }
                .to_owned(),
                kind: TaskKind::Desktop,
                title: "fixture".to_owned(),
                provisional: false,
                model: None,
                created_ms: 1,
            },
        )
        .unwrap()
}

fn insert_entries(store: &Store, task: &TaskRecord, sizes: &[usize]) {
    store
        .write(Durability::Durable, |tx| {
            let mut statement = tx.prepare(
                "INSERT INTO entries
            (task_id,seq,entry_id,kind,created_ms,bytes,payload)
            VALUES (?1,?2,?3,'message',1,?4,zeroblob(?4))",
            )?;
            for (index, size) in sizes.iter().enumerate() {
                statement.execute(params![
                    task.key.0,
                    index as i64 + 1,
                    format!("{index:08x}"),
                    *size as i64
                ])?;
            }
            Ok(())
        })
        .unwrap();
}

fn children(store: &Store, task: &TaskRecord) -> (i64, i64, i64) {
    store
        .read(|tx| {
            Ok(tx.query_row(
                "SELECT
            (SELECT count(*) FROM timeline_rows WHERE task_id=?1),
            (SELECT count(*) FROM entries WHERE task_id=?1),
            (SELECT count(*) FROM tool_summaries WHERE task_id=?1)",
                [task.key.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        })
        .unwrap()
}

fn task_exists(store: &Store, task: &TaskRecord) -> bool {
    store
        .read(|tx| {
            Ok(tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
                [task.key.0],
                |row| row.get(0),
            )?)
        })
        .unwrap()
}

fn assert_bounded(progress: &CollectionProgress) {
    assert!(progress.rows <= ROW_CAP);
    if progress.oversized_row {
        assert_eq!(progress.rows, 1);
        assert!(progress.bytes > BYTE_CAP);
    } else {
        assert!(progress.bytes <= BYTE_CAP);
    }
}

#[test]
fn source_over_one_mib_gets_an_isolated_step_and_all_children_are_precollected() {
    let (_directory, store, alice, _bob) = fixture();
    let task = task(&store, alice, 1);
    let large = BYTE_CAP as usize + 4096;
    insert_entries(&store, &task, &[1024, large, 2048]);
    store
        .write(Durability::Durable, |tx| {
            for row in 1..=3 {
                tx.execute(
                    "INSERT INTO timeline_rows
                (task_id,epoch,row_seq,row_key,entry_seq,item_index,update_seq,bytes,item)
                VALUES (?1,0,?2,?3,1,?2,1,600000,zeroblob(600000))",
                    params![task.key.0, row, format!("u{row}")],
                )?;
                tx.execute(
                    "INSERT INTO tool_summaries(task_id,entry_seq,item_index,bytes,summary)
                VALUES (?1,1,?2,700000,?3)",
                    params![task.key.0, row, "x".repeat(700_000)],
                )?;
            }
            Ok(())
        })
        .unwrap();
    store.mark_task_deleted(alice, &task.uuid, 2).unwrap();

    let expected = [
        (CollectionStage::Timeline, 600_000, (2, 3, 3)),
        (CollectionStage::Timeline, 600_000, (1, 3, 3)),
        (CollectionStage::Timeline, 600_000, (0, 3, 3)),
        (CollectionStage::Entries, 1024, (0, 2, 3)),
        (CollectionStage::Entries, large as u64, (0, 1, 3)),
        (CollectionStage::Entries, 2048, (0, 0, 3)),
        (CollectionStage::Summaries, 700_000, (0, 0, 2)),
        (CollectionStage::Summaries, 700_000, (0, 0, 1)),
        (CollectionStage::Summaries, 700_000, (0, 0, 0)),
    ];
    for (stage, bytes, remaining) in expected {
        let progress = store.collect_task_step(alice, &task.uuid).unwrap();
        assert_bounded(&progress);
        assert_eq!(progress.stage, stage);
        assert_eq!(progress.rows, 1);
        assert_eq!(progress.bytes, bytes);
        assert_eq!(progress.oversized_row, bytes > BYTE_CAP);
        assert!(!progress.done);
        assert!(task_exists(&store, &task));
        assert_eq!(children(&store, &task), remaining);
    }
    let final_step = store.collect_task_step(alice, &task.uuid).unwrap();
    assert_eq!(final_step.stage, CollectionStage::TaskMetadata);
    assert_eq!(final_step.bytes, 0);
    assert!(final_step.done);
    assert!(!task_exists(&store, &task));
    assert!(store.deleted_tasks(10).unwrap().is_empty());
}

#[test]
fn tiny_payloads_still_obey_row_cap_before_task_metadata_is_removed() {
    let (_directory, store, alice, _bob) = fixture();
    let task = task(&store, alice, 1);
    insert_entries(&store, &task, &vec![1; ROW_CAP + 1]);
    store
        .write(Durability::Durable, |tx| {
            let mut statement = tx.prepare(
                "INSERT INTO tool_summaries
            (task_id,entry_seq,item_index,bytes,summary) VALUES (?1,1,?2,1,'x')",
            )?;
            for index in 0..=ROW_CAP {
                statement.execute(params![task.key.0, index as i64])?;
            }
            Ok(())
        })
        .unwrap();
    store.mark_task_deleted(alice, &task.uuid, 2).unwrap();

    for (stage, rows) in [
        (CollectionStage::Entries, ROW_CAP),
        (CollectionStage::Entries, 1),
        (CollectionStage::Summaries, ROW_CAP),
        (CollectionStage::Summaries, 1),
    ] {
        let progress = store.collect_task_step(alice, &task.uuid).unwrap();
        assert_bounded(&progress);
        assert_eq!(progress.stage, stage);
        assert_eq!(progress.rows, rows);
        assert_eq!(progress.bytes, rows as u64);
        assert!(task_exists(&store, &task));
    }
    assert_eq!(children(&store, &task), (0, 0, 0));
    assert!(store.collect_task_step(alice, &task.uuid).unwrap().done);
}

#[test]
fn pending_account_deletion_never_collects_even_preexisting_tombstones() {
    let (_directory, store, alice, bob) = fixture();
    let deleted = task(&store, alice, 1);
    let live = task(&store, alice, 2);
    let foreign = task(&store, bob, 3);
    insert_entries(&store, &deleted, &[1024]);
    store.mark_task_deleted(alice, &deleted.uuid, 2).unwrap();
    store.mark_task_deleted(bob, &foreign.uuid, 2).unwrap();
    store
        .begin_account_deletion(alice, "pending-attempt", 3)
        .unwrap();

    assert_eq!(
        store.deleted_tasks(10).unwrap(),
        vec![DeletedTask {
            account: bob,
            uuid: foreign.uuid.clone(),
        }]
    );
    assert_eq!(
        store.collect_task_step(alice, &deleted.uuid),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.collect_task_step(alice, &live.uuid),
        Err(StoreError::Conflict)
    );
    assert_eq!(store.collect_account_step(alice), Err(StoreError::Conflict));
    assert_eq!(
        store.collect_task_step(bob, &deleted.uuid),
        Err(StoreError::NotFound)
    );
    assert_eq!(children(&store, &deleted), (0, 1, 0));
    assert!(task_exists(&store, &deleted));
    assert!(task_exists(&store, &live));

    store
        .abort_account_deletion(alice, "pending-attempt")
        .unwrap();
    assert_eq!(store.deleted_tasks(10).unwrap().len(), 2);
    assert_eq!(
        store.collect_task_step(alice, &deleted.uuid).unwrap().stage,
        CollectionStage::Entries
    );
    assert!(store.task(alice, &live.uuid).unwrap().is_some());
}

fn account_children(store: &Store, account: AccountId) -> i64 {
    store.read(|tx| {
        Ok(tx.query_row("SELECT
            (SELECT count(*) FROM account_settings WHERE account_id=?1) +
            (SELECT count(*) FROM projects WHERE account_id=?1) +
            (SELECT count(*) FROM mcp_servers WHERE account_id=?1) +
            (SELECT count(*) FROM mcp_server_secrets s JOIN mcp_servers m ON m.id=s.server_id WHERE m.account_id=?1) +
            (SELECT count(*) FROM mcp_oauth_credentials c JOIN mcp_servers m ON m.id=c.server_id WHERE m.account_id=?1) +
            (SELECT count(*) FROM credentials WHERE account_id=?1) +
            (SELECT count(*) FROM send_receipts WHERE account_id=?1) +
            (SELECT count(*) FROM remote_identities WHERE account_id=?1)",
            [account.0], |row| row.get(0))?)
    }).unwrap()
}

#[test]
fn confirmed_account_collection_precollects_metadata_and_preserves_other_account() {
    let (_directory, store, alice, bob) = fixture();
    let alice_task = task(&store, alice, 1);
    let bob_task = task(&store, bob, 2);
    insert_entries(&store, &alice_task, &[100]);
    insert_entries(&store, &bob_task, &[200]);
    for number in 0..5 {
        store
            .set_account_setting(
                alice,
                &format!("large-{number}"),
                &json!("x".repeat(256_000)),
                1,
            )
            .unwrap();
    }
    store
        .set_account_setting(bob, "agent.defaults", &json!({"keep": true}), 1)
        .unwrap();
    store
        .set_app_setting("ui", &json!({"keep": true}), 1)
        .unwrap();
    store.write(Durability::Durable, |tx| {
        tx.execute("INSERT INTO mcp_servers(account_id,key,position,definition)
            VALUES (?1,'fixture',0,'{}')", [alice.0])?;
        let server = tx.last_insert_rowid();
        for name in ["one", "two"] {
            tx.execute("INSERT INTO mcp_server_secrets(server_id,kind,name,target,value)
                VALUES (?1,'env',?2,zeroblob(32),zeroblob(600000))", params![server, name])?;
        }
        tx.execute("INSERT INTO credentials(slot,account_id,api_url,revision,access_token,refresh_token,updated_ms)
            VALUES (1,?1,'https://fixture.invalid',1,x'0102',x'0304',1)", [alice.0])?;
        tx.execute("INSERT INTO send_receipts(id,account_id,fingerprint,task_uuid,result,created_ms)
            VALUES ('receipt',?1,zeroblob(32),?2,'{}',1)", params![alice.0, alice_task.uuid])?;
        tx.execute("INSERT INTO remote_identities(scope,account_id,secret,record,updated_ms)
            VALUES ('fixture',?1,zeroblob(32),'{}',1)", [alice.0])?;
        Ok(())
    }).unwrap();
    store
        .begin_account_deletion(alice, "confirmed-attempt", 10)
        .unwrap();
    store
        .confirm_account_deletion(alice, "confirmed-attempt")
        .unwrap();
    assert_eq!(store.collect_account_step(alice), Err(StoreError::Busy));

    let payload = store.collect_task_step(alice, &alice_task.uuid).unwrap();
    assert_eq!(payload.stage, CollectionStage::Entries);
    assert!(
        store
            .collect_task_step(alice, &alice_task.uuid)
            .unwrap()
            .done
    );
    assert!(store.task(bob, &bob_task.uuid).unwrap().is_some());
    let mut completed = false;
    let mut metadata_steps = 0;
    for _ in 0..32 {
        let before = account_children(&store, alice);
        let progress = store.collect_account_step(alice).unwrap();
        assert_bounded(&progress);
        if progress.done {
            assert_eq!(
                before, 0,
                "final account deletion must have no variable-sized children"
            );
            assert_eq!(progress.stage, CollectionStage::Done);
            completed = true;
            break;
        }
        metadata_steps += 1;
        assert_eq!(progress.stage, CollectionStage::AccountMetadata);
        assert!(store.account_id("alice").unwrap().is_some());
        assert!(account_children(&store, alice) < before);
    }
    assert!(completed);
    assert!(
        metadata_steps >= 8,
        "large settings and MCP secret payloads require separate steps"
    );
    assert_eq!(store.account_id("alice").unwrap(), None);
    assert_eq!(store.account_id("bob").unwrap(), Some(bob));
    assert_eq!(children(&store, &bob_task), (0, 1, 0));
    assert_eq!(
        store.account_setting(bob, "agent.defaults").unwrap(),
        Some(json!({"keep": true}))
    );
    assert_eq!(
        store.app_setting("ui").unwrap(),
        Some(json!({"keep": true}))
    );
    assert!(store.confirmed_account_deletions(10).unwrap().is_empty());
}

#[test]
fn confirmed_account_collection_precollects_oauth_with_byte_and_row_caps_in_plain_and_keyed_stores()
{
    use crate::store::keys::{KdfParams, Password};

    const PAYLOAD_BYTES: usize = 65_536;
    const FOREIGN_PAYLOAD: &[u8] = b"GC-OAUTH-FOREIGN-CANARY";
    const GENERATION: &str = "0123456789abcdef0123456789abcdef";
    for encrypted in [false, true] {
        for (active, revoked) in [(32, 1), (0, ROW_CAP + 1)] {
            let (_directory, store, alice, bob) = fixture();
            store.write(Durability::Durable, |tx| {
                for position in 0..active + revoked {
                    tx.execute("INSERT INTO mcp_servers(account_id,key,position,definition) VALUES (?1,?2,?3,'{}')",
                        params![alice.0,format!("oauth-{position}"),position as i64])?;
                    let server = tx.last_insert_rowid();
                    let payload = (position < active).then(|| vec![0_u8; PAYLOAD_BYTES]);
                    tx.execute("INSERT INTO mcp_oauth_credentials(server_id,target,generation,revision,payload) VALUES (?1,zeroblob(32),?2,0,?3)",
                        params![server,GENERATION,payload])?;
                }
                tx.execute("INSERT INTO mcp_servers(account_id,key,position,definition) VALUES (?1,'foreign-oauth',0,'{}')", [bob.0])?;
                let server = tx.last_insert_rowid();
                tx.execute("INSERT INTO mcp_oauth_credentials(server_id,target,generation,revision,payload) VALUES (?1,zeroblob(32),?2,7,?3)",
                    params![server,GENERATION,FOREIGN_PAYLOAD])?;
                Ok(())
            }).unwrap();
            if encrypted {
                let password = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
                store
                    .set_store_password(&password, &[], KdfParams::floor())
                    .unwrap();
            }
            let oauth_state = |account: AccountId| {
                store.read(|tx| {
                    Ok(tx.query_row("SELECT count(*),coalesce(sum(16+length(c.target)+length(CAST(c.generation AS BLOB))+coalesce(length(c.payload),0)),0)
                        FROM mcp_oauth_credentials c JOIN mcp_servers m ON m.id=c.server_id WHERE m.account_id=?1",
                        [account.0], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?)))?)
                }).unwrap()
            };
            let initial = oauth_state(alice);
            assert_eq!(initial.0, (active + revoked) as i64);
            let foreign = oauth_state(bob);
            assert_eq!(store.collect_account_step(alice), Err(StoreError::Conflict));
            store
                .begin_account_deletion(alice, "oauth-delete", 2)
                .unwrap();
            assert_eq!(store.collect_account_step(alice), Err(StoreError::Conflict));
            assert_eq!(oauth_state(alice), initial);
            store
                .confirm_account_deletion(alice, "oauth-delete")
                .unwrap();

            let mut batches = 0;
            let mut deleted_bytes = 0;
            while oauth_state(alice).0 != 0 {
                let before = oauth_state(alice);
                let progress = store.collect_account_step(alice).unwrap();
                let after = oauth_state(alice);
                assert_bounded(&progress);
                assert_eq!(progress.stage, CollectionStage::AccountMetadata);
                assert!(!progress.done);
                assert_eq!(progress.rows as i64, before.0 - after.0);
                assert_eq!(progress.bytes, u64::try_from(before.1 - after.1).unwrap());
                if batches == 0 {
                    let row_bytes = 80 + if active == 0 { 0 } else { PAYLOAD_BYTES };
                    assert_eq!(progress.rows, ROW_CAP.min(BYTE_CAP as usize / row_bytes),);
                }
                let servers = store
                    .read(|tx| {
                        Ok(tx.query_row(
                            "SELECT count(*) FROM mcp_servers WHERE account_id=?1",
                            [alice.0],
                            |row| row.get::<_, i64>(0),
                        )?)
                    })
                    .unwrap();
                assert_eq!(servers, (active + revoked) as i64);
                assert_eq!(oauth_state(bob), foreign);
                batches += 1;
                deleted_bytes += progress.bytes;
            }
            assert!(batches >= 2);
            assert_eq!(deleted_bytes, u64::try_from(initial.1).unwrap());
            let mut completed = false;
            for _ in 0..16 {
                let before = account_children(&store, alice);
                let progress = store.collect_account_step(alice).unwrap();
                assert_bounded(&progress);
                if progress.done {
                    assert_eq!(before, 0);
                    completed = true;
                    break;
                }
            }
            assert!(completed);
            assert_eq!(store.account_id("alice").unwrap(), None);
            assert_eq!(store.account_id("bob").unwrap(), Some(bob));
            assert_eq!(oauth_state(bob), foreign);
            store.read(|tx| {
                assert_eq!(tx.query_row::<Vec<u8>,_,_>("SELECT c.payload FROM mcp_oauth_credentials c JOIN mcp_servers m ON m.id=c.server_id WHERE m.account_id=?1",
                    [bob.0], |row| row.get(0))?, FOREIGN_PAYLOAD);
                Ok(())
            }).unwrap();
        }
    }
}
