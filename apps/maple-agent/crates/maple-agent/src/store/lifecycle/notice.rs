//! Acknowledged mode-change notices, fenced by generation and exact record.

use sha2::{Digest, Sha256};

use super::*;

const NOTICE_LIMIT: i64 = 2048;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreModeNotice {
    pub store_id: String,
    pub notice_id: String,
    pub message: String,
    /// Additive presentation metadata; unknown kinds remain readable via message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum NoticeKind {
    #[serde(rename = "password_set")]
    Set,
    #[serde(rename = "password_changed")]
    Changed,
    #[serde(rename = "password_rotated")]
    Rotated,
    #[serde(rename = "password_removed")]
    Removed,
}

impl NoticeKind {
    fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "password_set" => Ok(Self::Set),
            "password_changed" => Ok(Self::Changed),
            "password_rotated" => Ok(Self::Rotated),
            "password_removed" => Ok(Self::Removed),
            _ => Err(StoreError::InvalidInput),
        }
    }

    fn message(self, at_ms: i64) -> StoreResult<String> {
        let date = chrono::DateTime::from_timestamp_millis(at_ms)
            .ok_or(StoreError::InvalidData)?
            .format("%Y-%m-%d %H:%M UTC");
        let action = match self {
            Self::Set => "An unlock password was set",
            Self::Changed => "Your unlock password was changed",
            Self::Rotated => {
                "Your unlock password was changed and your local data was re-encrypted"
            }
            Self::Removed => "Your unlock password was removed",
        };
        Ok(format!("{action} on this computer at {date}."))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoticeRecord {
    kind: NoticeKind,
    at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
}

pub(super) fn new_value(kind: &str) -> LifecycleResult<String> {
    let record = NoticeRecord {
        kind: NoticeKind::parse(kind)?,
        at_ms: super::super::now_ms(),
        operation_id: Some(random_id()?),
    };
    serde_json::to_string(&record).map_err(|_| StoreError::InvalidData.into())
}

impl Store {
    /// Read this one non-content notice only while authenticated store access is
    /// Open. General settings and raw JSON are never returned by this API.
    pub fn mode_change_notice(&self) -> StoreResult<Option<StoreModeNotice>> {
        self.read(|transaction| {
            let generation = notice_generation(transaction)?;
            let Some(value) = stored_value(transaction)? else {
                return Ok(None);
            };
            make_notice(&generation, &value).map(Some)
        })
    }

    /// An old UI acknowledgement cannot clear a newer operation or a notice in
    /// a replacement store. Comparison and deletion use one FULL transaction.
    pub fn acknowledge_mode_change_notice(
        &self,
        store_id: &str,
        notice_id: &str,
    ) -> StoreResult<bool> {
        if decode_id(store_id).is_none()
            || notice_id.len() != 64
            || !notice_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(StoreError::InvalidInput);
        }
        self.write(super::super::Durability::Durable, |transaction| {
            let generation = notice_generation(transaction)?;
            if encode_id(&generation) != store_id {
                return Ok(false);
            }
            let Some(value) = stored_value(transaction)? else {
                return Ok(false);
            };
            if make_notice(&generation, &value)?.notice_id != notice_id {
                return Ok(false);
            }
            Ok(transaction.execute(
                "DELETE FROM app_settings WHERE key='store.mode_notice' AND value=?1",
                [&value],
            )? == 1)
        })
    }
}

fn notice_generation(connection: &Connection) -> StoreResult<[u8; 16]> {
    let value: Vec<u8> = connection.query_row(
        "SELECT value FROM store_meta WHERE key='store_id'",
        [],
        |row| row.get(0),
    )?;
    value.try_into().map_err(|_| StoreError::InvalidData)
}

fn stored_value(connection: &Connection) -> StoreResult<Option<String>> {
    connection.query_row(
        "SELECT value,length(CAST(value AS BLOB)) FROM app_settings WHERE key='store.mode_notice'", [],
        |row| {
            let bytes: i64 = row.get(1)?;
            if !(1..=NOTICE_LIMIT).contains(&bytes) {return Err(rusqlite::Error::InvalidQuery);}
            row.get(0)
        },
    ).optional().map_err(Into::into)
}

fn make_notice(generation: &[u8; 16], value: &str) -> StoreResult<StoreModeNotice> {
    let record: NoticeRecord = serde_json::from_str(value).map_err(|_| StoreError::InvalidData)?;
    if record
        .operation_id
        .as_ref()
        .is_some_and(|id| decode_id(id).is_none())
    {
        return Err(StoreError::InvalidData);
    }
    // Hash every stored byte, including legacy records without operation IDs.
    // A changed payload cannot retain an old acknowledgement token.
    let mut digest = Sha256::new();
    digest.update(b"maple-store-mode-notice/1\0");
    digest.update(generation);
    digest.update(value.as_bytes());
    let notice_id = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(StoreModeNotice {
        store_id: encode_id(generation),
        notice_id,
        message: record.kind.message(record.at_ms)?,
        kind: Some(
            match record.kind {
                NoticeKind::Set => "password_set",
                NoticeKind::Changed => "password_changed",
                NoticeKind::Rotated => "password_rotated",
                NoticeKind::Removed => "password_removed",
            }
            .into(),
        ),
        at_ms: Some(record.at_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(path: &Path) -> Store {
        let store = Store::unavailable(StorePaths::new(path), StoreState::Maintenance).unwrap();
        store.initialize_recovered().unwrap();
        store
    }

    fn install(store: &Store, value: &str) {
        store.write(super::super::super::Durability::Durable, |tx| {
            tx.execute("INSERT INTO app_settings(key,value,updated_ms) VALUES ('store.mode_notice',?1,1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [value])?;
            Ok(())
        }).unwrap();
    }

    #[test]
    fn semantic_metadata_preserves_legacy_record_bytes_and_ack_identity() {
        let directory = tempfile::tempdir().unwrap();
        let store = opened(directory.path());
        let record = r#"{"kind":"password_rotated","at_ms":0}"#;
        install(&store, record);
        let notice = store.mode_change_notice().unwrap().unwrap();
        assert_eq!(notice.kind.as_deref(), Some("password_rotated"));
        assert_eq!(notice.at_ms, Some(0));
        store
            .read(|tx| {
                assert_eq!(stored_value(tx)?.as_deref(), Some(record));
                let mut hash = Sha256::new();
                hash.update(b"maple-store-mode-notice/1\0");
                hash.update(notice_generation(tx)?);
                hash.update(record.as_bytes());
                assert_eq!(notice.notice_id, format!("{:x}", hash.finalize()));
                Ok(())
            })
            .unwrap();
        let legacy: StoreModeNotice = serde_json::from_value(serde_json::json!({
            "store_id":notice.store_id, "notice_id":notice.notice_id, "message":notice.message
        }))
        .unwrap();
        assert!(legacy.kind.is_none());
        assert!(legacy.at_ms.is_none());
        assert!(
            store
                .acknowledge_mode_change_notice(&legacy.store_id, &legacy.notice_id)
                .unwrap()
        );
        assert!(store.mode_change_notice().unwrap().is_none());
    }

    #[test]
    fn notice_and_explicit_ack_survive_restarts_without_exposing_raw_settings() {
        let directory = tempfile::tempdir().unwrap();
        let store = opened(directory.path());
        store.write_notice_locked("password_removed").unwrap();
        let notice = store.mode_change_notice().unwrap().unwrap();
        assert!(
            notice
                .message
                .starts_with("Your unlock password was removed on this computer at ")
        );
        store.close().unwrap();
        drop(store);
        let store = opened(directory.path());
        assert_eq!(store.mode_change_notice().unwrap(), Some(notice.clone()));
        assert!(
            !store
                .acknowledge_mode_change_notice(&encode_id(&[0; 16]), &notice.notice_id)
                .unwrap()
        );
        assert!(
            !store
                .acknowledge_mode_change_notice(&notice.store_id, &"0".repeat(64))
                .unwrap()
        );
        assert!(
            store
                .acknowledge_mode_change_notice(&notice.store_id, &notice.notice_id)
                .unwrap()
        );
        assert!(
            !store
                .acknowledge_mode_change_notice(&notice.store_id, &notice.notice_id)
                .unwrap()
        );
        store.close().unwrap();
        drop(store);
        assert!(
            opened(directory.path())
                .mode_change_notice()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_new_operation_and_an_edited_legacy_record_both_reject_stale_acknowledgements() {
        let directory = tempfile::tempdir().unwrap();
        let store = opened(directory.path());
        let legacy = r#"{"kind":"password_set","at_ms":1}"#;
        install(&store, legacy);
        let first = store.mode_change_notice().unwrap().unwrap();
        assert_eq!(first, store.mode_change_notice().unwrap().unwrap());
        install(&store, r#"{"kind":"password_set", "at_ms":1}"#);
        let edited = store.mode_change_notice().unwrap().unwrap();
        assert_ne!(first.notice_id, edited.notice_id);
        assert!(
            !store
                .acknowledge_mode_change_notice(&first.store_id, &first.notice_id)
                .unwrap()
        );
        store.write_notice_locked("password_set").unwrap();
        let next = store.mode_change_notice().unwrap().unwrap();
        assert_ne!(next.notice_id, edited.notice_id);
        assert!(
            !store
                .acknowledge_mode_change_notice(&edited.store_id, &edited.notice_id)
                .unwrap()
        );
        assert!(
            store
                .acknowledge_mode_change_notice(&next.store_id, &next.notice_id)
                .unwrap()
        );
    }

    #[test]
    fn invalid_or_oversized_notice_records_are_bounded_and_sanitized() {
        let directory = tempfile::tempdir().unwrap();
        let store = opened(directory.path());
        for value in [
            r#"{"kind":"PRIVATE-CANARY","at_ms":1}"#.to_owned(),
            format!(
                r#"{{"kind":"password_set","at_ms":1,"extra":"{}"}}"#,
                "x".repeat(4096)
            ),
        ] {
            install(&store, &value);
            let error = store.mode_change_notice().unwrap_err();
            assert_eq!(error, StoreError::InvalidData);
            assert!(!format!("{error:?} {error}").contains("PRIVATE-CANARY"));
        }
        assert!(
            store
                .acknowledge_mode_change_notice("../db", &"0".repeat(64))
                .is_err()
        );
    }
}
