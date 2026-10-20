//! Pure source-fingerprint contracts and ignored disposable database contracts.
//!
//! Each fixture owns a unique organization and deletes its synthetic rows on drop.
//! These tests use opaque synthetic ciphertext and make no provider calls.

use super::{
    CompactionCheckpoint, CompactionError, CompactionPublishResult, CompactionSnapshot,
    CompactionSourceMetadata, NewCompactionCheckpoint,
};
use crate::models::{
    org_projects::NewOrgProject,
    orgs::{NewOrg, Org},
    responses::{
        Conversation, NewConversation, NewResponse, RawThreadMessage, Response, ResponseStatus,
    },
    schema::{
        assistant_messages, conversations, reasoning_items, tool_calls, tool_outputs, user_messages,
    },
    users::NewUser,
};
use crate::web::responses::compaction::context::{ContextItemKey, ContextItemKind};
use chrono::{DateTime, Duration, Utc};
use diesel::{
    connection::SimpleConnection, Connection, ExpressionMethods, PgConnection, QueryDsl,
    RunQueryDsl,
};
use uuid::Uuid;

struct Fixture {
    conn: PgConnection,
    org: Org,
    user: Uuid,
    other_user: Uuid,
    conversation_id: i64,
}

impl Fixture {
    fn new() -> Self {
        let database_url = std::env::var("AEAD_TAMPER_TEST_DATABASE_URL")
            .expect("requires an explicitly selected disposable migrated local database");
        let parsed = url::Url::parse(&database_url).expect("database URL must parse");
        assert!(matches!(
            parsed.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]")
        ));
        assert!(
            parsed
                .path()
                .trim_start_matches('/')
                .starts_with("opensecret_agent_test_"),
            "requires the disposable database helper's generated database name"
        );
        let mut conn = PgConnection::establish(&database_url)
            .expect("disposable migrated database must be available");
        let org = NewOrg::new(format!("compaction-db-test-{}", Uuid::new_v4()))
            .insert(&mut conn)
            .unwrap();
        let project = NewOrgProject::new(org.id, "Compaction database tests".to_string())
            .insert(&mut conn)
            .unwrap();
        let user = NewUser::new(None, None, project.id)
            .insert(&mut conn)
            .unwrap()
            .uuid;
        let other_user = NewUser::new(None, None, project.id)
            .insert(&mut conn)
            .unwrap()
            .uuid;
        let conversation = NewConversation {
            uuid: Uuid::new_v4(),
            user_id: user,
            project_id: None,
            is_pinned: false,
            metadata_enc: None,
        }
        .insert(&mut conn)
        .unwrap();
        Self {
            conn,
            org,
            user,
            other_user,
            conversation_id: conversation.id,
        }
    }

    fn snapshot(&mut self) -> CompactionSnapshot {
        CompactionSnapshot::load(&mut self.conn, self.conversation_id, self.user).unwrap()
    }

    fn response(&mut self, status: ResponseStatus) -> Response {
        NewResponse {
            uuid: Uuid::new_v4(),
            user_id: self.user,
            conversation_id: self.conversation_id,
            status,
            model: "deepseek-v4-1-flash".to_string(),
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            tool_choice: None,
            parallel_tool_calls: false,
            store: true,
            metadata_enc: None,
        }
        .insert(&mut self.conn)
        .unwrap()
    }

    fn user_item(&mut self, response: Option<i64>, at: DateTime<Utc>) -> ContextItemKey {
        let uuid = Uuid::new_v4();
        diesel::insert_into(user_messages::table)
            .values((
                user_messages::uuid.eq(uuid),
                user_messages::conversation_id.eq(self.conversation_id),
                user_messages::response_id.eq(response),
                user_messages::user_id.eq(self.user),
                user_messages::content_enc.eq(vec![1_u8, 2, 3]),
                user_messages::prompt_tokens.eq(1),
                user_messages::created_at.eq(at),
            ))
            .execute(&mut self.conn)
            .unwrap();
        ContextItemKey {
            kind: ContextItemKind::User,
            uuid,
        }
    }

    fn assistant_item(
        &mut self,
        response: Option<i64>,
        at: DateTime<Utc>,
        status: &str,
    ) -> ContextItemKey {
        let uuid = Uuid::new_v4();
        diesel::insert_into(assistant_messages::table)
            .values((
                assistant_messages::uuid.eq(uuid),
                assistant_messages::conversation_id.eq(self.conversation_id),
                assistant_messages::response_id.eq(response),
                assistant_messages::user_id.eq(self.user),
                assistant_messages::content_enc.eq(Some(vec![4_u8, 5, 6])),
                assistant_messages::completion_tokens.eq(1),
                assistant_messages::status.eq(status),
                assistant_messages::finish_reason.eq(Some("stop")),
                assistant_messages::created_at.eq(at),
            ))
            .execute(&mut self.conn)
            .unwrap();
        ContextItemKey {
            kind: ContextItemKind::Assistant,
            uuid,
        }
    }

    fn tool_pair(&mut self, at: DateTime<Utc>) -> [ContextItemKey; 2] {
        let call_uuid = Uuid::new_v4();
        let call_id = diesel::insert_into(tool_calls::table)
            .values((
                tool_calls::uuid.eq(call_uuid),
                tool_calls::conversation_id.eq(self.conversation_id),
                tool_calls::user_id.eq(self.user),
                tool_calls::name.eq("web_search"),
                tool_calls::arguments_enc.eq(Some(vec![7_u8, 8, 9])),
                tool_calls::argument_tokens.eq(1),
                tool_calls::status.eq("completed"),
                tool_calls::created_at.eq(at),
            ))
            .returning(tool_calls::id)
            .get_result::<i64>(&mut self.conn)
            .unwrap();
        let output_uuid = Uuid::new_v4();
        diesel::insert_into(tool_outputs::table)
            .values((
                tool_outputs::uuid.eq(output_uuid),
                tool_outputs::conversation_id.eq(self.conversation_id),
                tool_outputs::user_id.eq(self.user),
                tool_outputs::tool_call_fk.eq(call_id),
                tool_outputs::output_enc.eq(vec![10_u8, 11, 12]),
                tool_outputs::output_tokens.eq(1),
                tool_outputs::status.eq("completed"),
                tool_outputs::created_at.eq(at + Duration::microseconds(1)),
            ))
            .execute(&mut self.conn)
            .unwrap();
        [
            ContextItemKey {
                kind: ContextItemKind::ToolCall,
                uuid: call_uuid,
            },
            ContextItemKey {
                kind: ContextItemKind::ToolOutput,
                uuid: output_uuid,
            },
        ]
    }

    fn reasoning_item(&mut self, at: DateTime<Utc>) -> ContextItemKey {
        let uuid = Uuid::new_v4();
        diesel::insert_into(reasoning_items::table)
            .values((
                reasoning_items::uuid.eq(uuid),
                reasoning_items::conversation_id.eq(self.conversation_id),
                reasoning_items::user_id.eq(self.user),
                reasoning_items::content_enc.eq(Some(vec![13_u8, 14, 15])),
                reasoning_items::reasoning_tokens.eq(1),
                reasoning_items::status.eq("completed"),
                reasoning_items::created_at.eq(at),
            ))
            .execute(&mut self.conn)
            .unwrap();
        ContextItemKey {
            kind: ContextItemKind::Reasoning,
            uuid,
        }
    }

    fn candidate(
        &self,
        snapshot: &CompactionSnapshot,
        covered_items: Vec<ContextItemKey>,
        current_response_uuid: Option<Uuid>,
    ) -> NewCompactionCheckpoint {
        NewCompactionCheckpoint {
            uuid: Uuid::new_v4(),
            conversation_id: self.conversation_id,
            user_id: self.user,
            content_enc: vec![21, 22, 23],
            content_tokens: 1,
            expected_head_uuid: snapshot.checkpoint.as_ref().map(|head| head.uuid),
            previous_covered_items: if snapshot.checkpoint.is_some() {
                keys(snapshot)
            } else {
                Vec::new()
            },
            source_fingerprint: snapshot.coverage_fingerprint(&covered_items).unwrap(),
            covered_items,
            current_response_uuid,
            origin_response_uuid: current_response_uuid.unwrap_or_else(Uuid::new_v4),
            after_item_uuid: None,
        }
    }

    fn publish(&mut self, candidate: NewCompactionCheckpoint) -> CompactionPublishResult {
        CompactionCheckpoint::publish(&mut self.conn, candidate).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let deleted = self.org.delete(&mut self.conn);
        assert!(
            deleted.is_ok() || std::thread::panicking(),
            "disposable fixture organization cleanup must succeed"
        );
    }
}

fn at(index: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + Duration::microseconds(index)
}

fn keys(snapshot: &CompactionSnapshot) -> Vec<ContextItemKey> {
    snapshot.items.iter().map(|item| item.key).collect()
}

fn raw_keys(items: &[RawThreadMessage]) -> Vec<ContextItemKey> {
    items
        .iter()
        .map(|item| ContextItemKey {
            kind: ContextItemKind::from_db_type(&item.message_type).unwrap(),
            uuid: item.uuid,
        })
        .collect()
}

fn published(result: CompactionPublishResult) -> CompactionCheckpoint {
    match result {
        CompactionPublishResult::Published(checkpoint) => checkpoint,
        CompactionPublishResult::Conflict => {
            panic!("unchanged source and expected head must publish")
        }
    }
}

fn synthetic_snapshot() -> CompactionSnapshot {
    let call_uuid = Uuid::from_u128(102);
    let items = [
        ContextItemKind::User,
        ContextItemKind::ToolCall,
        ContextItemKind::ToolOutput,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, kind)| CompactionSourceMetadata {
        key: ContextItemKey {
            kind,
            uuid: Uuid::from_u128(101 + index as u128),
        },
        id: index as i64 + 1,
        created_at: at(index as i64),
        updated_at: at(index as i64),
        token_count: Some(1),
        item_status: Some("completed".to_string()),
        response_uuid: Some(Uuid::from_u128(200)),
        response_status: Some("in_progress".to_string()),
        tool_call_id: match kind {
            ContextItemKind::ToolCall | ContextItemKind::ToolOutput => Some(call_uuid),
            _ => None,
        },
        tool_name: match kind {
            ContextItemKind::ToolCall | ContextItemKind::ToolOutput => {
                Some("web_search".to_string())
            }
            _ => None,
        },
        model: Some("deepseek-v4-1-flash".to_string()),
        finish_reason: None,
        encrypted_bytes: 32,
        content_fingerprint: format!("{:064x}", index + 1),
    })
    .collect();
    CompactionSnapshot {
        conversation_id: 1,
        conversation_uuid: Uuid::from_u128(300),
        user_id: Uuid::from_u128(400),
        items,
        active_response_uuids: vec![Uuid::from_u128(200)],
        fingerprint: [0; 32],
        checkpoint: None,
    }
}

#[test]
fn coverage_fingerprint_rejects_duplicate_missing_and_wrong_kind_source_keys() {
    let snapshot = synthetic_snapshot();
    let selected = keys(&snapshot);
    for invalid in [
        vec![selected[0], selected[0]],
        vec![ContextItemKey {
            kind: ContextItemKind::User,
            uuid: Uuid::from_u128(999),
        }],
        vec![ContextItemKey {
            kind: ContextItemKind::Assistant,
            uuid: selected[0].uuid,
        }],
    ] {
        assert!(matches!(
            snapshot.coverage_fingerprint(&invalid),
            Err(CompactionError::InvalidSelection)
        ));
    }
}

#[test]
fn coverage_fingerprint_ignores_response_lifecycle_and_token_annotations() {
    let mut snapshot = synthetic_snapshot();
    let selected = keys(&snapshot);
    let before = snapshot.coverage_fingerprint(&selected).unwrap();
    for item in &mut snapshot.items {
        item.updated_at += Duration::seconds(1);
        item.token_count = Some(1_000);
        item.response_status = Some("completed".to_string());
    }
    snapshot.active_response_uuids.clear();
    assert_eq!(snapshot.coverage_fingerprint(&selected).unwrap(), before);
}

#[test]
fn coverage_fingerprint_detects_same_length_ciphertext_and_render_relation_changes() {
    let snapshot = synthetic_snapshot();
    let selected = keys(&snapshot);
    let before = snapshot.coverage_fingerprint(&selected).unwrap();

    let mut changed = synthetic_snapshot();
    changed.items[0].content_fingerprint = "f".repeat(64);
    assert_eq!(
        changed.items[0].encrypted_bytes,
        snapshot.items[0].encrypted_bytes
    );
    assert_eq!(
        changed.items[0].content_fingerprint.len(),
        snapshot.items[0].content_fingerprint.len()
    );
    assert_ne!(changed.coverage_fingerprint(&selected).unwrap(), before);

    let mut changed = synthetic_snapshot();
    changed.items[2].tool_call_id = Some(Uuid::from_u128(999));
    assert_ne!(changed.coverage_fingerprint(&selected).unwrap(), before);

    let mut changed = synthetic_snapshot();
    changed.items[1].tool_name = Some("open_urls".to_string());
    assert_ne!(changed.coverage_fingerprint(&selected).unwrap(), before);

    let mut changed = synthetic_snapshot();
    changed.items[1].item_status = Some("incomplete".to_string());
    assert_ne!(changed.coverage_fingerprint(&selected).unwrap(), before);
}

#[test]
fn coverage_fingerprint_uses_canonical_source_order_instead_of_requested_key_order() {
    let snapshot = synthetic_snapshot();
    let canonical = keys(&snapshot);
    let before = snapshot.coverage_fingerprint(&canonical).unwrap();
    let mut requested = canonical;
    requested.reverse();
    assert_eq!(snapshot.coverage_fingerprint(&requested).unwrap(), before);
    requested.rotate_left(1);
    assert_eq!(snapshot.coverage_fingerprint(&requested).unwrap(), before);
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_ownership_isolation_covers_reads_publication_and_invalidation() {
    let mut fixture = Fixture::new();
    let item = fixture.user_item(None, at(0));
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![item], None);
    let checkpoint = published(fixture.publish(candidate));

    assert!(CompactionSnapshot::load(
        &mut fixture.conn,
        fixture.conversation_id,
        fixture.other_user,
    )
    .is_err());
    assert!(CompactionSnapshot::load_items(
        &mut fixture.conn,
        fixture.conversation_id,
        fixture.other_user,
        &[item],
    )
    .is_err());
    assert!(CompactionCheckpoint::invalidate(
        &mut fixture.conn,
        fixture.conversation_id,
        fixture.other_user,
    )
    .is_err());
    let snapshot = fixture.snapshot();
    let mut unauthorized = fixture.candidate(&snapshot, vec![item], None);
    unauthorized.user_id = fixture.other_user;
    assert!(CompactionCheckpoint::publish(&mut fixture.conn, unauthorized).is_err());
    assert_eq!(fixture.snapshot().checkpoint.unwrap().uuid, checkpoint.uuid);
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_snapshot_keeps_more_than_one_thousand_items_and_exact_requested_order() {
    let mut fixture = Fixture::new();
    let mut expected = Vec::new();
    for index in 0..1005 {
        expected.push(fixture.user_item(None, at(index)));
    }
    expected.push(fixture.assistant_item(None, at(1005), "completed"));
    expected.extend(fixture.tool_pair(at(1006)));
    expected.push(fixture.reasoning_item(at(1008)));
    let snapshot = fixture.snapshot();
    assert_eq!(snapshot.items.len(), 1009);
    assert_eq!(keys(&snapshot), expected);
    expected.reverse();
    let raw = CompactionSnapshot::load_items(
        &mut fixture.conn,
        fixture.conversation_id,
        fixture.user,
        &expected,
    )
    .unwrap();
    assert_eq!(raw_keys(&raw), expected);
    assert_eq!(raw.len(), 1009);
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_publication_cas_rejects_stale_head_and_preserves_canonical_rows() {
    let mut fixture = Fixture::new();
    let item = fixture.user_item(None, at(0));
    let snapshot = fixture.snapshot();
    let first = fixture.candidate(&snapshot, vec![item], None);
    let stale = fixture.candidate(&snapshot, vec![item], None);
    let checkpoint = published(fixture.publish(first));
    assert!(matches!(
        fixture.publish(stale),
        CompactionPublishResult::Conflict
    ));
    let next_snapshot = fixture.snapshot();
    assert_eq!(keys(&next_snapshot), vec![item]);
    assert_eq!(
        next_snapshot.checkpoint.as_ref().unwrap().uuid,
        checkpoint.uuid
    );
    let successor = fixture.candidate(&next_snapshot, vec![item], None);
    let successor = published(fixture.publish(successor));
    assert_eq!(successor.generation, checkpoint.generation + 1);
    assert_eq!(fixture.snapshot().checkpoint.unwrap().uuid, successor.uuid);
    let raw = CompactionSnapshot::load_items(
        &mut fixture.conn,
        fixture.conversation_id,
        fixture.user,
        &[item],
    )
    .unwrap();
    assert_eq!(raw[0].content_enc.as_deref(), Some([1_u8, 2, 3].as_slice()));
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_selected_same_length_ciphertext_mutation_conflicts() {
    let mut fixture = Fixture::new();
    let item = fixture.user_item(None, at(0));
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![item], None);
    diesel::update(user_messages::table.filter(user_messages::uuid.eq(item.uuid)))
        .set(user_messages::content_enc.eq(vec![9_u8, 9, 9]))
        .execute(&mut fixture.conn)
        .unwrap();
    assert!(matches!(
        fixture.publish(candidate),
        CompactionPublishResult::Conflict
    ));
    assert!(fixture.snapshot().checkpoint.is_none());
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_selected_source_deletion_conflicts() {
    let mut fixture = Fixture::new();
    let item = fixture.user_item(None, at(0));
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![item], None);
    diesel::delete(user_messages::table.filter(user_messages::uuid.eq(item.uuid)))
        .execute(&mut fixture.conn)
        .unwrap();
    assert!(matches!(
        fixture.publish(candidate),
        CompactionPublishResult::Conflict
    ));
    assert!(fixture.snapshot().checkpoint.is_none());
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_uncovered_append_does_not_invalidate_selected_source() {
    let mut fixture = Fixture::new();
    let covered = fixture.user_item(None, at(0));
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![covered], None);
    let appended = fixture.user_item(None, at(1));
    let checkpoint = published(fixture.publish(candidate));
    let after = fixture.snapshot();
    assert_eq!(after.checkpoint.unwrap().uuid, checkpoint.uuid);
    assert_eq!(keys(&fixture.snapshot()), vec![covered, appended]);
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_covering_an_unrelated_active_response_refuses_publication() {
    let mut fixture = Fixture::new();
    let current = fixture.response(ResponseStatus::InProgress);
    let unrelated = fixture.response(ResponseStatus::InProgress);
    let user = fixture.user_item(Some(unrelated.id), at(0));
    let assistant = fixture.assistant_item(Some(unrelated.id), at(1), "completed");
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![user, assistant], Some(current.uuid));
    assert!(matches!(
        fixture.publish(candidate),
        CompactionPublishResult::Conflict
    ));
    assert!(fixture.snapshot().checkpoint.is_none());
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_unrelated_active_uncovered_items_remain_raw_without_blocking_publication() {
    let mut fixture = Fixture::new();
    let covered = fixture.user_item(None, at(0));
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![covered], None);
    let unrelated = fixture.response(ResponseStatus::InProgress);
    let appended = fixture.user_item(Some(unrelated.id), at(1));
    published(fixture.publish(candidate));
    assert_eq!(keys(&fixture.snapshot()), vec![covered, appended]);
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_current_active_response_may_publish_its_completed_steps() {
    let mut fixture = Fixture::new();
    let current = fixture.response(ResponseStatus::InProgress);
    let user = fixture.user_item(Some(current.id), at(0));
    let assistant = fixture.assistant_item(Some(current.id), at(1), "completed");
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![user, assistant], Some(current.uuid));
    published(fixture.publish(candidate));
    assert_eq!(keys(&fixture.snapshot()), vec![user, assistant]);
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_successor_retains_prior_active_coverage_but_cannot_cover_new_unrelated_steps() {
    let mut fixture = Fixture::new();
    let response_a = fixture.response(ResponseStatus::InProgress);
    let a_user = fixture.user_item(Some(response_a.id), at(0));
    let a_assistant = fixture.assistant_item(Some(response_a.id), at(1), "completed");
    let a_covered = vec![a_user, a_assistant];
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, a_covered.clone(), Some(response_a.uuid));
    let first = published(fixture.publish(candidate));

    let response_b = fixture.response(ResponseStatus::InProgress);
    let b_user = fixture.user_item(Some(response_b.id), at(2));
    let b_assistant = fixture.assistant_item(Some(response_b.id), at(3), "completed");
    let mut both_covered = a_covered.clone();
    both_covered.extend([b_user, b_assistant]);
    let snapshot = fixture.snapshot();
    assert!(snapshot.active_response_uuids.contains(&response_a.uuid));
    let mut candidate = fixture.candidate(&snapshot, both_covered.clone(), Some(response_b.uuid));
    candidate.previous_covered_items = a_covered;
    let successor = published(fixture.publish(candidate));
    assert_eq!(successor.generation, first.generation + 1);

    let a_later = fixture.assistant_item(Some(response_a.id), at(4), "completed");
    let snapshot = fixture.snapshot();
    let mut attempted_coverage = both_covered.clone();
    attempted_coverage.push(a_later);
    let mut candidate = fixture.candidate(&snapshot, attempted_coverage, Some(response_b.uuid));
    candidate.previous_covered_items = both_covered.clone();
    assert!(matches!(
        fixture.publish(candidate),
        CompactionPublishResult::Conflict
    ));
    let after = fixture.snapshot();
    assert_eq!(after.checkpoint.as_ref().unwrap().uuid, successor.uuid);
    both_covered.push(a_later);
    assert_eq!(keys(&after), both_covered);
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_current_response_incomplete_item_cannot_be_checkpointed() {
    let mut fixture = Fixture::new();
    let current = fixture.response(ResponseStatus::InProgress);
    let user = fixture.user_item(Some(current.id), at(0));
    let assistant = fixture.assistant_item(Some(current.id), at(1), "in_progress");
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![user, assistant], Some(current.uuid));
    assert!(matches!(
        fixture.publish(candidate),
        CompactionPublishResult::Conflict
    ));
    assert!(fixture.snapshot().checkpoint.is_none());
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_response_deletion_invalidates_checkpoint_without_deleting_other_history() {
    let mut fixture = Fixture::new();
    let response = fixture.response(ResponseStatus::Completed);
    let covered = fixture.user_item(Some(response.id), at(0));
    let retained = fixture.user_item(None, at(1));
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![covered], None);
    let checkpoint = published(fixture.publish(candidate));
    assert!(CompactionCheckpoint::delete_response(
        &mut fixture.conn,
        response.uuid,
        fixture.other_user,
    )
    .is_err());
    assert_eq!(fixture.snapshot().checkpoint.unwrap().uuid, checkpoint.uuid);
    CompactionCheckpoint::delete_response(&mut fixture.conn, response.uuid, fixture.user).unwrap();
    let snapshot = fixture.snapshot();
    assert!(snapshot.checkpoint.is_none());
    assert_eq!(keys(&snapshot), vec![retained]);
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_user_message_deletion_invalidates_checkpoint_and_is_owner_scoped() {
    let mut fixture = Fixture::new();
    let covered = fixture.user_item(None, at(0));
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![covered], None);
    published(fixture.publish(candidate));
    assert!(CompactionCheckpoint::delete_user_message(
        &mut fixture.conn,
        covered.uuid,
        fixture.other_user,
    )
    .is_err());
    assert!(fixture.snapshot().checkpoint.is_some());
    CompactionCheckpoint::delete_user_message(&mut fixture.conn, covered.uuid, fixture.user)
        .unwrap();
    let snapshot = fixture.snapshot();
    assert!(snapshot.checkpoint.is_none());
    assert!(snapshot.items.is_empty());
}

#[test]
#[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
fn db_compaction_publication_waiting_on_parent_delete_does_not_hold_cascaded_children() {
    #[derive(diesel::QueryableByName)]
    struct BackendPid {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        pid: i32,
    }
    #[derive(diesel::QueryableByName)]
    struct BlockedByThisConnection {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        blocked: bool,
    }

    let mut fixture = Fixture::new();
    let item = fixture.user_item(None, at(0));
    let snapshot = fixture.snapshot();
    let candidate = fixture.candidate(&snapshot, vec![item], None);
    let conversation_id = fixture.conversation_id;
    let user_id = fixture.user;
    let database_url = std::env::var("AEAD_TAMPER_TEST_DATABASE_URL").unwrap();
    let (pid_sender, pid_receiver) = std::sync::mpsc::sync_channel(1);

    let publisher = fixture
        .conn
        .transaction::<_, CompactionError, _>(|conn| {
            conn.batch_execute("SET LOCAL statement_timeout = '8s'")?;
            conn.batch_execute("SET LOCAL lock_timeout = '6s'")?;
            conversations::table
                .filter(conversations::id.eq(conversation_id))
                .select(conversations::id)
                .for_update()
                .first::<i64>(conn)?;
            let publisher = std::thread::spawn(move || {
                let mut conn = PgConnection::establish(&database_url).unwrap();
                conn.batch_execute("SET statement_timeout = '8s'").unwrap();
                conn.batch_execute("SET lock_timeout = '6s'").unwrap();
                let pid = diesel::sql_query("SELECT pg_backend_pid() AS pid")
                    .get_result::<BackendPid>(&mut conn)
                    .unwrap()
                    .pid;
                pid_sender.send(pid).unwrap();
                CompactionCheckpoint::publish(&mut conn, candidate)
            });
            let publisher_pid = pid_receiver
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("publisher must establish its dedicated connection");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let blocked = diesel::sql_query(
                    "SELECT pg_backend_pid() = ANY(pg_blocking_pids($1)) AS blocked",
                )
                .bind::<diesel::sql_types::Integer, _>(publisher_pid)
                .get_result::<BlockedByThisConnection>(conn)?
                .blocked;
                if blocked {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "publisher must wait for this transaction's conversation lock"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            // If publication acquired a child lock before waiting on the parent,
            // this cascade creates a real lock cycle and one statement must fail.
            Conversation::delete_by_id_and_user(conn, conversation_id, user_id)?;
            Ok(publisher)
        })
        .expect("parent deletion must complete without a publication deadlock");
    let publication = publisher.join().expect("publisher thread must finish");
    assert!(matches!(
        publication,
        Err(CompactionError::ConversationNotFound)
    ));
}
