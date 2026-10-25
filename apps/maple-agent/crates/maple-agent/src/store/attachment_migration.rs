//! One-way retirement of the old SQLite image payloads.
//!
//! Schema v3 fences old binaries before this code changes any source. Every
//! step commits its progress with its effect, so a crash resumes at that step.
//! Nothing exports formerly protected bytes to a plaintext attachment file.

use pi_ai::{Content, Message};
use pi_coding_agent::{
    SessionMessage,
    session::{EntryKind, SessionEntry},
};
use rusqlite::{Connection, TransactionBehavior, params};

use super::{StoreError, StoreResult, codec};

const PHASE_KEY: &str = "attachment_migration_phase";
const CURSOR_KEY: &str = "attachment_migration_cursor";
const RETIRED_IMAGE: &str = "maple-attachment-unavailable:retired";
const ROW_BATCH: i64 = 128;
const CHUNK_BATCH: i64 = 4;
const SOURCE_BATCH_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn retire_legacy_images(connection: &mut Connection) -> StoreResult<()> {
    let mut steps = 0usize;
    while !retire_step(connection)? {
        steps += 1;
        if steps.is_multiple_of(16) {
            // The ordinary checkpoint worker starts after migration. Keep a
            // large retirement from retaining its entire rewrite in the WAL.
            super::connection::checkpoint(connection, false)?;
        }
    }
    super::connection::checkpoint(connection, false)?;
    Ok(())
}

/// Each call is one short, durable SQL transaction. Source decoding is bounded
/// by the codec's existing per-entry cap, never the total history or blob size.
fn retire_step(connection: &mut Connection) -> StoreResult<bool> {
    let phase: i64 = connection.query_row(
        "SELECT value FROM store_meta WHERE key=?1",
        [PHASE_KEY],
        |row| row.get(0),
    )?;
    match phase {
        0 => retire_source_batch(connection),
        1 => invalidate_task_timelines(connection),
        2 => collect_payload_rows(
            connection,
            "timeline_rows",
            3,
            ROW_BATCH,
            SOURCE_BATCH_BYTES,
        ),
        3 => collect_rows(connection, "attachments", 4, ROW_BATCH),
        4 => collect_rows(connection, "task_blobs", 5, ROW_BATCH),
        5 => collect_payload_rows(connection, "blob_chunks", 6, CHUNK_BATCH, 1024 * 1024),
        6 => collect_rows(connection, "blobs", 7, ROW_BATCH),
        7 => Ok(true),
        _ => Err(StoreError::InvalidData),
    }
}

fn retire_source_batch(connection: &mut Connection) -> StoreResult<bool> {
    let cursor: i64 = connection.query_row(
        "SELECT value FROM store_meta WHERE key=?1",
        [CURSOR_KEY],
        |row| row.get(0),
    )?;
    if cursor < 0
        || connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE id<=0)",
            [],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Err(StoreError::InvalidData);
    }
    let mut prepared = Vec::new();
    let mut batch_bytes = 0usize;
    {
        let mut statement = connection.prepare(
            "SELECT id,entry_id,flags,codec,bytes,length(payload), \
             CASE WHEN length(payload)<=?3 THEN payload END \
             FROM entries WHERE id>?1 ORDER BY id LIMIT ?2",
        )?;
        let mut rows =
            statement.query(params![cursor, ROW_BATCH, codec::MAX_DECODED_BYTES as i64])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let entry_id: String = row.get(1)?;
            let flags: i64 = row.get(2)?;
            let encoded: i64 = row.get(3)?;
            let size: i64 = row.get(4)?;
            let actual_size: i64 = row.get(5)?;
            if flags & !1 != 0 || size < 0 || size != actual_size {
                return Err(StoreError::InvalidData);
            }
            let size = usize::try_from(size).map_err(|_| StoreError::InvalidData)?;
            if size > codec::MAX_DECODED_BYTES {
                return Err(StoreError::InvalidData);
            }
            if !prepared.is_empty() && batch_bytes.saturating_add(size) > SOURCE_BATCH_BYTES {
                break;
            }
            let payload: Vec<u8> = row.get(6)?;
            let mut entry: SessionEntry =
                serde_json::from_slice(&codec::decode(encoded, &payload)?)
                    .map_err(|_| StoreError::InvalidData)?;
            if entry.id != entry_id {
                return Err(StoreError::InvalidData);
            }
            let replacement = if retire_entry_images(&mut entry) {
                Some(codec::encode(
                    &serde_json::to_vec(&entry).map_err(|_| StoreError::InvalidData)?,
                )?)
            } else {
                None
            };
            prepared.push((id, flags, replacement));
            batch_bytes += size;
        }
    }
    let Some((last_id, _, _)) = prepared.last() else {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("UPDATE store_meta SET value=0 WHERE key=?1", [CURSOR_KEY])?;
        set_phase(&transaction, 1)?;
        transaction.commit()?;
        return Ok(false);
    };
    let last_id = *last_id;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (id, flags, replacement) in prepared {
        if let Some((encoded, payload)) = replacement {
            transaction.execute(
                "UPDATE entries SET flags=0,codec=?2,bytes=?3,payload=?4 WHERE id=?1",
                params![id, encoded, payload.len() as i64, payload],
            )?;
        } else if flags != 0 {
            transaction.execute("UPDATE entries SET flags=0 WHERE id=?1", [id])?;
        }
    }
    transaction.execute(
        "UPDATE store_meta SET value=?2 WHERE key=?1",
        params![CURSOR_KEY, last_id],
    )?;
    transaction.commit()?;
    Ok(false)
}

fn invalidate_task_timelines(connection: &mut Connection) -> StoreResult<bool> {
    let cursor: i64 = connection.query_row(
        "SELECT value FROM store_meta WHERE key=?1",
        [CURSOR_KEY],
        |row| row.get(0),
    )?;
    let end: Option<i64> = connection.query_row(
        "SELECT max(id) FROM (SELECT id FROM tasks WHERE id>?1 ORDER BY id LIMIT ?2)",
        params![cursor, ROW_BATCH],
        |row| row.get(0),
    )?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(end) = end {
        // Old rows can contain cached attachment metadata. Rebuild from the
        // rewritten source while retaining source identities and tool summaries.
        transaction.execute(
            "UPDATE tasks SET timeline_format=0,rebuild_epoch=NULL,rebuild_seq=NULL,rebuild_item_index=0,context_usage=NULL \
             WHERE id>?1 AND id<=?2",
            params![cursor, end],
        )?;
        transaction.execute(
            "UPDATE store_meta SET value=?2 WHERE key=?1",
            params![CURSOR_KEY, end],
        )?;
    } else {
        set_phase(&transaction, 2)?;
    }
    transaction.commit()?;
    Ok(false)
}

fn retire_entry_images(entry: &mut SessionEntry) -> bool {
    let content = match &mut entry.kind {
        EntryKind::Message {
            message: SessionMessage::Llm(Message::User(message)),
        } => Some(&mut message.content),
        EntryKind::Message {
            message: SessionMessage::Llm(Message::ToolResult(message)),
        } => Some(&mut message.content),
        EntryKind::Message {
            message: SessionMessage::Custom(message),
        } => Some(&mut message.content),
        EntryKind::CustomMessage { content, .. } => Some(content),
        EntryKind::ContextEdit { replacement, .. } => replacement.as_mut(),
        _ => None,
    };
    let mut changed = false;
    if let Some(content) = content {
        for block in content {
            if let Content::Image(image) = block {
                changed |= image.data != RETIRED_IMAGE;
                image.data = RETIRED_IMAGE.into();
            }
        }
    }
    changed
}

fn collect_rows(
    connection: &mut Connection,
    table: &str,
    next_phase: i64,
    limit: i64,
) -> StoreResult<bool> {
    // Names are internal constants above, never caller input. WITHOUT ROWID
    // tables need their actual primary keys for bounded deletion.
    let key = match table {
        "blobs" => "id",
        "attachments" => "(task_id,public_id)",
        "task_blobs" => "(task_id,blob_id)",
        _ => return Err(StoreError::InvalidData),
    };
    let select = key.trim_start_matches('(').trim_end_matches(')');
    let sql = format!("DELETE FROM {table} WHERE {key} IN (SELECT {select} FROM {table} LIMIT ?1)");
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if transaction.execute(&sql, [limit])? == 0 {
        set_phase(&transaction, next_phase)?;
    }
    transaction.commit()?;
    Ok(false)
}

fn collect_payload_rows(
    connection: &mut Connection,
    table: &str,
    next_phase: i64,
    limit: i64,
    byte_limit: usize,
) -> StoreResult<bool> {
    // Inspect only primary keys and actual BLOB lengths, never payload bytes or
    // the cached `bytes` field. One oversized historical row is retired alone
    // so malformed old chunk sizes cannot inflate the rest of a batch.
    let (select, delete) = match table {
        "timeline_rows" => (
            "SELECT id,0,length(item) FROM timeline_rows ORDER BY id LIMIT ?1",
            "DELETE FROM timeline_rows WHERE id=?1 AND ?2=0",
        ),
        "blob_chunks" => (
            "SELECT blob_id,idx,length(data) FROM blob_chunks ORDER BY blob_id,idx LIMIT ?1",
            "DELETE FROM blob_chunks WHERE blob_id=?1 AND idx=?2",
        ),
        _ => return Err(StoreError::InvalidData),
    };
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut keys = Vec::new();
    let mut bytes = 0usize;
    {
        let mut statement = transaction.prepare(select)?;
        let mut rows = statement.query([limit])?;
        while let Some(row) = rows.next()? {
            let size =
                usize::try_from(row.get::<_, i64>(2)?).map_err(|_| StoreError::InvalidData)?;
            if !keys.is_empty() && bytes.saturating_add(size) > byte_limit {
                break;
            }
            keys.push((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?));
            bytes = bytes.saturating_add(size);
            if bytes >= byte_limit {
                break;
            }
        }
    }
    if keys.is_empty() {
        set_phase(&transaction, next_phase)?;
    } else {
        for key in keys {
            transaction.execute(delete, params![key.0, key.1])?;
        }
    }
    transaction.commit()?;
    Ok(false)
}

fn set_phase(connection: &Connection, phase: i64) -> StoreResult<()> {
    if connection.execute(
        "UPDATE store_meta SET value=?2 WHERE key=?1",
        params![PHASE_KEY, phase],
    )? != 1
    {
        return Err(StoreError::InvalidData);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{engine, keys, schema};
    use pi_ai::{ImageContent, ToolResultMessage, UserMessage};
    use pi_coding_agent::messages::CustomMessage;

    fn connection(path: &std::path::Path, keyed: bool) -> Connection {
        let connection = Connection::open(path).unwrap();
        if keyed {
            engine::key_connection(
                &connection,
                &keys::LockedKey::from_bytes([0xa5; 32]).unwrap(),
            )
            .unwrap();
        } else {
            engine::guard_plain(&connection).unwrap();
        }
        connection
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        connection
    }

    fn image(data: &str) -> Content {
        Content::Image(ImageContent {
            data: data.into(),
            mime_type: "image/png".into(),
        })
    }

    fn fixture(connection: &mut Connection, version: i64) -> Vec<SessionEntry> {
        schema::migrate_to_version(connection, 1, version).unwrap();
        connection.execute("INSERT INTO accounts(id,user_key,user_id,created_ms,last_seen_ms) VALUES (1,'a','A',1,1)", []).unwrap();
        connection
            .execute(
                "INSERT INTO projects(id,account_id,root,last_used_ms) VALUES (1,1,'/work',1)",
                [],
            )
            .unwrap();
        connection.execute("INSERT INTO tasks(id,uuid,account_id,project_id,kind,state,title,title_fold,created_ms,updated_ms,last_seq,timeline_format,rows_epoch,last_row_seq) VALUES (1,'session',1,1,'desktop','settled','Keep title','keep title',1,1,130,1,3,1)", []).unwrap();
        connection
            .execute("UPDATE tasks SET context_usage=1234", [])
            .unwrap();
        let mut kinds = vec![
            EntryKind::Message {
                message: SessionMessage::Llm(Message::User(UserMessage {
                    content: vec![Content::text("keep user text"), image("maple-blob:123")],
                    timestamp: 100,
                })),
            },
            EntryKind::Message {
                message: SessionMessage::Llm(Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call-1".into(),
                    tool_name: "screenshot".into(),
                    content: vec![
                        Content::text("keep OCR"),
                        image("maple-inline-image:opaque"),
                    ],
                    details: Some(serde_json::json!({"keep":"data:image/png;base64,opaque"})),
                    usage: None,
                    is_error: false,
                    timestamp: 101,
                })),
            },
            EntryKind::Message {
                message: SessionMessage::Custom(CustomMessage {
                    custom_type: "extension".into(),
                    content: vec![image("raw-base64-image")],
                    display: true,
                    details: None,
                    timestamp: 102,
                }),
            },
            EntryKind::CustomMessage {
                custom_type: "custom".into(),
                content: vec![
                    Content::text("keep custom text"),
                    image("data:image/png;base64,AA=="),
                ],
                display: true,
                details: Some(serde_json::json!({"image":"opaque extension data stays"})),
            },
            EntryKind::ContextEdit {
                target_id: "entry0".into(),
                replacement: Some(vec![
                    Content::text("keep replacement"),
                    image("maple-blob:456"),
                ]),
            },
            EntryKind::Compaction {
                summary: "keep summary".into(),
                first_kept_entry_id: "entry1".into(),
                tokens_before: 100,
                details: None,
                usage: None,
                from_extension: false,
                system_message: None,
            },
        ];
        // More than a source batch proves its cursor resumes a partial history.
        kinds.extend((6..130).map(|index| EntryKind::Label {
            target_id: "entry0".into(),
            label: Some(format!("preserved label {index}")),
        }));
        let entries = kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| SessionEntry {
                id: format!("entry{index}"),
                parent_id: index.checked_sub(1).map(|parent| format!("entry{parent}")),
                timestamp: 100 + index as i64,
                kind,
            })
            .collect::<Vec<_>>();
        for (index, entry) in entries.iter().enumerate() {
            let payload = serde_json::to_vec(entry).unwrap();
            // Exercise both codecs even for this small typed fixture.
            let (encoded, payload) = if index % 2 == 0 {
                (1, zstd::stream::encode_all(payload.as_slice(), 3).unwrap())
            } else {
                (0, payload)
            };
            connection.execute("INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,flags,codec,bytes,payload) VALUES (1,?1,?2,'message',1,?3,?4,?5,?6)", params![index as i64 + 1, entry.id, i64::from(index < 2), encoded, payload.len() as i64, payload]).unwrap();
        }
        connection.execute("INSERT INTO blobs(id,sha256,mime,size,complete,created_ms) VALUES (1,?1,'image/png',5120,1,1)", [vec![1u8;32]]).unwrap();
        for index in 0..5 {
            connection
                .execute(
                    "INSERT INTO blob_chunks(blob_id,idx,data) VALUES (1,?1,?2)",
                    params![index, vec![42u8; 1024]],
                )
                .unwrap();
        }
        connection
            .execute("INSERT INTO task_blobs(task_id,blob_id) VALUES (1,1)", [])
            .unwrap();
        connection.execute("INSERT INTO attachments(task_id,public_id,blob_id,created_ms) VALUES (1,'old-image',1,1)", []).unwrap();
        connection.execute("INSERT INTO timeline_rows(task_id,epoch,row_seq,row_key,entry_seq,item_index,update_seq,bytes,item) VALUES (1,3,1,'row',1,0,1,3,?1)", [b"old".as_slice()]).unwrap();
        connection.execute("INSERT INTO tool_summaries(task_id,entry_seq,item_index,bytes,summary) VALUES (1,2,0,12,'keep summary')", []).unwrap();
        entries
    }

    #[test]
    fn plain_and_keyed_retirement_resumes_preserves_source_and_removes_sql_bytes() {
        for keyed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("migration.db");
            let mut database = connection(&path, keyed);
            let original = fixture(&mut database, 3);
            // Stop after one source batch: schema v3 and the cursor both survive
            // reopening. Released v2 readers reject this user_version first.
            assert!(!retire_step(&mut database).unwrap());
            assert_eq!(
                database
                    .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                    .unwrap(),
                3
            );
            database.close().unwrap();
            let mut database = connection(&path, keyed);
            assert_eq!(schema::preflight(&database, false).unwrap(), 3);
            assert_eq!(
                database
                    .query_row::<i64, _, _>(
                        "SELECT value FROM store_meta WHERE key=?1",
                        [CURSOR_KEY],
                        |row| row.get(0)
                    )
                    .unwrap(),
                128
            );
            // Step until the bounded chunk phase, then prove a step retires at
            // most four chunks and resumes the remainder after another reopen.
            while database
                .query_row::<i64, _, _>(
                    "SELECT value FROM store_meta WHERE key=?1",
                    [PHASE_KEY],
                    |row| row.get(0),
                )
                .unwrap()
                != 5
            {
                assert!(!retire_step(&mut database).unwrap());
            }
            assert!(!retire_step(&mut database).unwrap());
            assert_eq!(
                database
                    .query_row::<i64, _, _>("SELECT count(*) FROM blob_chunks", [], |row| row
                        .get(0))
                    .unwrap(),
                1
            );
            database.close().unwrap();
            let mut database = connection(&path, keyed);
            schema::migrate(&mut database, 200).unwrap();
            schema::validate(&database).unwrap();
            let mut statement = database
                .prepare("SELECT flags,codec,bytes,payload FROM entries ORDER BY seq")
                .unwrap();
            let stored = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            for (mut expected, (flags, encoded, bytes, payload)) in original.into_iter().zip(stored)
            {
                retire_entry_images(&mut expected);
                assert_eq!(flags, 0);
                assert_eq!(bytes as usize, payload.len());
                let actual: SessionEntry =
                    serde_json::from_slice(&codec::decode(encoded, &payload).unwrap()).unwrap();
                assert_eq!(actual, expected);
            }
            for table in ["blobs", "blob_chunks", "task_blobs", "attachments"] {
                assert_eq!(
                    database
                        .query_row::<i64, _, _>(
                            "SELECT count(*) FROM sqlite_schema WHERE name=?1",
                            [table],
                            |row| row.get(0)
                        )
                        .unwrap(),
                    0
                );
            }
            assert_eq!(
                database
                    .query_row::<String, _, _>("SELECT summary FROM tool_summaries", [], |row| row
                        .get(0))
                    .unwrap(),
                "keep summary"
            );
            assert_eq!(
                database
                    .query_row::<i64, _, _>("SELECT count(*) FROM timeline_rows", [], |row| row
                        .get(0))
                    .unwrap(),
                0
            );
            assert_eq!(
                database
                    .query_row::<(String, i64, i64), _, _>(
                        "SELECT title,last_seq,timeline_format FROM tasks",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    )
                    .unwrap(),
                ("Keep title".into(), 130, 0)
            );
            assert_eq!(
                database
                    .query_row::<Option<i64>, _, _>("SELECT context_usage FROM tasks", [], |row| {
                        row.get(0)
                    })
                    .unwrap(),
                None
            );
            assert!(!temp.path().join("attachments").exists());
        }
    }

    #[test]
    fn malformed_source_does_not_become_an_unavailable_image() {
        for corruption in [
            "flags",
            "size",
            "codec",
            "source",
            "entry_id",
            "later_source",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let mut database = connection(&temp.path().join("migration.db"), false);
            // Enter from the shipped schema: even an invalid first source row
            // must commit the version fence before any retirement is attempted.
            fixture(&mut database, 2);
            let sql = match corruption {
                "flags" => "UPDATE entries SET flags=2 WHERE id=1",
                "size" => "UPDATE entries SET bytes=bytes+1 WHERE id=1",
                "codec" => "UPDATE entries SET codec=2 WHERE id=1",
                "source" => "UPDATE entries SET codec=0,bytes=1,payload=x'7b' WHERE id=1",
                "entry_id" => "UPDATE entries SET entry_id='wrong' WHERE id=1",
                "later_source" => "UPDATE entries SET codec=2 WHERE id=2",
                _ => unreachable!(),
            };
            database.execute(sql, []).unwrap();
            assert_eq!(
                schema::migrate(&mut database, 200),
                Err(StoreError::InvalidData),
                "{corruption}"
            );
            assert_eq!(
                database
                    .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                    .unwrap(),
                3
            );
            assert_eq!(
                database
                    .query_row::<i64, _, _>("SELECT count(*) FROM blob_chunks", [], |row| row
                        .get(0))
                    .unwrap(),
                5
            );
            assert_eq!(
                database
                    .query_row::<i64, _, _>(
                        "SELECT value FROM store_meta WHERE key=?1",
                        [CURSOR_KEY],
                        |row| row.get(0)
                    )
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn collection_uses_actual_payload_sizes_and_retires_oversized_rows_alone() {
        for keyed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut database = connection(&temp.path().join("migration.db"), keyed);
            fixture(&mut database, 3);
            database.execute("DELETE FROM timeline_rows", []).unwrap();
            for (id, size) in [(1, 3 * 1024 * 1024), (2, 5 * 1024 * 1024), (3, 1024 * 1024)] {
                // Intentionally wrong cached bytes: retirement must use length(item).
                database.execute("INSERT INTO timeline_rows(id,task_id,epoch,row_seq,row_key,entry_seq,item_index,update_seq,bytes,item) VALUES (?1,1,3,?1,'row',1,0,1,1,zeroblob(?2))", params![id, size]).unwrap();
            }
            set_phase(&database, 2).unwrap();
            for expected in [2, 1, 0] {
                assert!(!retire_step(&mut database).unwrap());
                assert_eq!(
                    database
                        .query_row::<i64, _, _>("SELECT count(*) FROM timeline_rows", [], |row| row
                            .get(0))
                        .unwrap(),
                    expected
                );
            }
            database.execute("DELETE FROM blob_chunks", []).unwrap();
            for (index, size) in [(0, 2 * 1024 * 1024), (1, 256 * 1024), (2, 256 * 1024)] {
                database
                    .execute(
                        "INSERT INTO blob_chunks(blob_id,idx,data) VALUES (1,?1,zeroblob(?2))",
                        params![index, size],
                    )
                    .unwrap();
            }
            set_phase(&database, 5).unwrap();
            assert!(!retire_step(&mut database).unwrap());
            assert_eq!(
                database
                    .query_row::<i64, _, _>("SELECT count(*) FROM blob_chunks", [], |row| row
                        .get(0))
                    .unwrap(),
                2
            );
            assert!(!retire_step(&mut database).unwrap());
            assert_eq!(
                database
                    .query_row::<i64, _, _>("SELECT count(*) FROM blob_chunks", [], |row| row
                        .get(0))
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn retired_plain_and_keyed_blob_pages_are_reclaimed_by_bounded_idle_vacuum() {
        for keyed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("migration.db");
            let mut database = connection(&path, keyed);
            database
                .pragma_update(None, "auto_vacuum", "INCREMENTAL")
                .unwrap();
            database.pragma_update(None, "journal_mode", "WAL").unwrap();
            database.pragma_update(None, "synchronous", "FULL").unwrap();
            fixture(&mut database, 2);
            {
                let transaction = database
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .unwrap();
                for index in 5..17 {
                    transaction.execute("INSERT INTO blob_chunks(blob_id,idx,data) VALUES (1,?1,zeroblob(262144))", [index]).unwrap();
                }
                transaction
                    .execute("UPDATE blobs SET size=3150848", [])
                    .unwrap();
                transaction.commit().unwrap();
            }
            super::super::connection::checkpoint(&database, true).unwrap();
            let before = std::fs::metadata(&path).unwrap().len();
            schema::migrate(&mut database, 200).unwrap();
            let mut pages: i64 = database
                .pragma_query_value(None, "freelist_count", |row| row.get(0))
                .unwrap();
            assert!(pages > 64);
            for _ in 0..64 {
                if pages == 0 {
                    break;
                }
                {
                    let transaction = database
                        .transaction_with_behavior(TransactionBehavior::Immediate)
                        .unwrap();
                    let mut statement = transaction
                        .prepare("PRAGMA incremental_vacuum(64)")
                        .unwrap();
                    let mut rows = statement.query([]).unwrap();
                    while rows.next().unwrap().is_some() {}
                    drop(rows);
                    drop(statement);
                    transaction.commit().unwrap();
                }
                let remaining: i64 = database
                    .pragma_query_value(None, "freelist_count", |row| row.get(0))
                    .unwrap();
                assert!((1..=64).contains(&(pages - remaining)));
                pages = remaining;
                super::super::connection::checkpoint(&database, false).unwrap();
            }
            assert_eq!(pages, 0);
            super::super::connection::checkpoint(&database, true).unwrap();
            assert!(std::fs::metadata(&path).unwrap().len() < before);
            schema::validate(&database).unwrap();
            assert_eq!(
                database
                    .query_row::<i64, _, _>("SELECT count(*) FROM entries", [], |row| row.get(0))
                    .unwrap(),
                130
            );
        }
    }
}
