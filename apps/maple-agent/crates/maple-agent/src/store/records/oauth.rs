//! Dedicated OAuth material never enters MCP headers, environment or IPC.

use super::*;

/// Host-only credential snapshot. Its generation and revision must both match
/// before a refresh or clear can mutate the current grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpOAuthCredentials {
    pub generation: String,
    pub revision: i64,
    pub payload: SecretString,
}

fn check_payload(payload: &SecretString) -> StoreResult<()> {
    if payload.expose().is_empty()
        || payload.expose().len() > MAX_SECRET_BYTES
        || !serde_json::from_str::<Value>(payload.expose()).is_ok_and(|value| value.is_object())
    {
        return Err(StoreError::InvalidInput);
    }
    Ok(())
}

fn bound_server(
    tx: &Transaction<'_>,
    account: AccountId,
    key: &str,
    target: &[u8; 32],
) -> StoreResult<i64> {
    tasks::writable_account(tx, account)?;
    let (id, definition) = mcp_server_tx(tx, account, key)?.ok_or(StoreError::NotFound)?;
    if mcp_target_hash(&definition.target)? != *target {
        return Err(StoreError::Conflict);
    }
    Ok(id)
}

fn credential_tx(
    tx: &Transaction<'_>,
    id: i64,
    target: &[u8; 32],
) -> StoreResult<Option<McpOAuthCredentials>> {
    let row = tx
        .query_row(
            "SELECT target,generation,revision,payload FROM mcp_oauth_credentials WHERE server_id=?1",
            [id],
            |row| Ok((row.get::<_,Vec<u8>>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,Option<Vec<u8>>>(3)?)),
        )
        .optional()?;
    row.map(|(saved_target, generation, revision, bytes)| {
        if saved_target.as_slice() != target.as_slice() || generation.len() != 32 || revision < 0 {
            return Err(StoreError::InvalidData);
        }
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let payload =
            SecretString::new(String::from_utf8(bytes).map_err(|_| StoreError::InvalidData)?);
        check_payload(&payload).map_err(|_| StoreError::InvalidData)?;
        Ok(Some(McpOAuthCredentials {
            generation,
            revision,
            payload,
        }))
    })
    .transpose()
    .map(Option::flatten)
}

impl Store {
    /// A cleared grant retains its managed marker so explicit reconnect can
    /// distinguish it from an unrelated custom server using the same name.
    pub fn mcp_oauth_managed(
        &self,
        account: AccountId,
        key: &str,
        expected_target: &McpTarget,
    ) -> StoreResult<bool> {
        let target = mcp_target_hash(expected_target)?;
        self.read(|tx| {
            let id = bound_server(tx, account, key, &target)?;
            Ok(tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM mcp_oauth_credentials WHERE server_id=?1 AND target=?2)",
                params![id,target.as_slice()],
                |row|row.get(0),
            )?)
        })
    }

    /// Read only after revalidating account and canonical MCP destination.
    pub fn mcp_oauth_credentials(
        &self,
        account: AccountId,
        key: &str,
        expected_target: &McpTarget,
    ) -> StoreResult<Option<McpOAuthCredentials>> {
        let target = mcp_target_hash(expected_target)?;
        self.read(|tx| {
            let id = bound_server(tx, account, key, &target)?;
            credential_tx(tx, id, &target)
        })
    }

    /// Install only a newly authorized definition or reconnect a definition
    /// already managed by OAuth. A concurrent edit or unrelated named server
    /// cannot be overwritten. Other definitions are untouched.
    pub fn install_mcp_oauth(
        &self,
        account: AccountId,
        expected_existing: Option<&McpDefinition>,
        edit: &McpServerEdit,
        payload: SecretString,
    ) -> StoreResult<McpOAuthCredentials> {
        check_payload(&payload)?;
        let definition = mcp_definition(edit)?;
        if definition.enabled
            || !definition.environment.is_empty()
            || !definition.headers.is_empty()
            || !matches!(definition.target, McpTarget::StreamableHttp { .. })
        {
            return Err(StoreError::InvalidInput);
        }
        let target = mcp_target_hash(&definition.target)?;
        let next = McpOAuthCredentials {
            generation: uuid::Uuid::new_v4().simple().to_string(),
            revision: 0,
            payload,
        };
        self.write(Durability::Durable, |tx| {
            tasks::writable_account(tx, account)?;
            let current = mcp_server_tx(tx, account, &edit.key)?;
            match (&current, expected_existing) {
                (None, None) => {
                    let count: i64 = tx.query_row("SELECT count(*) FROM mcp_servers WHERE account_id=?1", [account.0], |row| row.get(0))?;
                    if count >= MAX_MCP_SERVERS as i64 { return Err(StoreError::InvalidInput); }
                }
                (Some((id, current)), Some(expected)) if current == expected => {
                    let old_target = mcp_target_hash(&current.target)?;
                    let managed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM mcp_oauth_credentials WHERE server_id=?1 AND target=?2)",params![id,old_target.as_slice()],|row|row.get(0))?;
                    if !managed { return Err(StoreError::Conflict); }
                }
                _ => return Err(StoreError::Conflict),
            }
            let json = serde_json::to_string(&definition).map_err(|_| StoreError::InvalidInput)?;
            let id: i64 = if let Some((id, _)) = current {
                tx.execute("UPDATE mcp_servers SET definition=?1 WHERE id=?2 AND account_id=?3", params![json,id,account.0])?;
                tx.execute("DELETE FROM mcp_server_secrets WHERE server_id=?1", [id])?;
                id
            } else {
                tx.query_row(
                    "INSERT INTO mcp_servers(account_id,key,position,definition) SELECT ?1,?2,coalesce(max(position),-1)+1,?3 FROM mcp_servers WHERE account_id=?1 RETURNING id",
                    params![account.0,edit.key,json], |row| row.get(0),
                )?
            };
            tx.execute(
                "INSERT INTO mcp_oauth_credentials(server_id,target,generation,revision,payload) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(server_id) DO UPDATE SET target=excluded.target,generation=excluded.generation,revision=excluded.revision,payload=excluded.payload",
                params![id,target.as_slice(),next.generation,next.revision,next.payload.expose().as_bytes()],
            )?;
            Ok(next.clone())
        })
    }

    /// Compare-and-swap prevents stale refresh work from replacing a new grant.
    pub fn rotate_mcp_oauth_credentials(
        &self,
        account: AccountId,
        key: &str,
        expected_target: &McpTarget,
        expected: &McpOAuthCredentials,
        payload: SecretString,
    ) -> StoreResult<McpOAuthCredentials> {
        check_payload(&payload)?;
        let target = mcp_target_hash(expected_target)?;
        let revision = expected
            .revision
            .checked_add(1)
            .ok_or(StoreError::Conflict)?;
        self.write(Durability::Durable, |tx| {
            let id = bound_server(tx, account, key, &target)?;
            let updated = tx.execute(
                "UPDATE mcp_oauth_credentials SET revision=?1,payload=?2 WHERE server_id=?3 AND target=?4 AND generation=?5 AND revision=?6 AND payload IS NOT NULL",
                params![revision,payload.expose().as_bytes(),id,target.as_slice(),expected.generation,expected.revision],
            )?;
            if updated != 1 { return Err(StoreError::Conflict); }
            Ok(McpOAuthCredentials { generation:expected.generation.clone(), revision,payload:payload.clone() })
        })
    }

    /// Clear only the exact grant/rotation the caller observed.
    pub fn clear_mcp_oauth_credentials(
        &self,
        account: AccountId,
        key: &str,
        expected_target: &McpTarget,
        expected: &McpOAuthCredentials,
    ) -> StoreResult<bool> {
        let target = mcp_target_hash(expected_target)?;
        self.write(Durability::Durable, |tx| {
            let id = bound_server(tx, account, key, &target)?;
            Ok(tx.execute(
                "UPDATE mcp_oauth_credentials SET payload=NULL,revision=revision+1 WHERE server_id=?1 AND target=?2 AND generation=?3 AND revision=?4 AND payload IS NOT NULL",
                params![id,target.as_slice(),expected.generation,expected.revision],
            )? == 1)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{StorePaths, StoreState, engine, keys};

    fn fixture(keyed: bool) -> (tempfile::TempDir, Store, AccountId, AccountId) {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Store::unavailable(StorePaths::new(dir.path()), StoreState::Maintenance).unwrap();
        if keyed {
            store
                .initialize_key_material(
                    keys::LockedKey::from_bytes([0xa1; 32]).unwrap(),
                    engine::key_connection,
                )
                .unwrap();
        } else {
            store.initialize_plain().unwrap();
        }
        let alice = store.upsert_account("Alice", 1).unwrap();
        let bob = store.upsert_account("Bob", 1).unwrap();
        (dir, store, alice, bob)
    }

    fn edit(port: u16) -> McpServerEdit {
        McpServerEdit {
            key: "vana".into(),
            name: "Vana".into(),
            description: "Local personal context".into(),
            enabled: false,
            timeout_seconds: 30,
            target: McpTarget::StreamableHttp {
                url: format!("http://127.0.0.1:{port}/mcp"),
            },
            environment: vec![],
            headers: vec![],
        }
    }

    fn payload(label: &str) -> SecretString {
        SecretString::new(serde_json::json!({"syntheticRefreshToken":label}).to_string())
    }

    #[test]
    fn oauth_is_private_account_target_bound_and_durable_in_plain_and_keyed_stores() {
        for keyed in [false, true] {
            let (_dir, store, alice, bob) = fixture(keyed);
            let edit = edit(47321);
            let saved = store
                .install_mcp_oauth(alice, None, &edit, payload("never-in-definitions"))
                .unwrap();
            assert_eq!(
                store
                    .mcp_oauth_credentials(alice, "vana", &edit.target)
                    .unwrap(),
                Some(saved.clone())
            );
            assert_eq!(
                store.mcp_oauth_credentials(bob, "vana", &edit.target),
                Err(StoreError::NotFound)
            );
            assert_eq!(
                store.mcp_oauth_credentials(alice, "vana", &self::edit(47322).target),
                Err(StoreError::Conflict)
            );
            let definitions =
                serde_json::to_string(&store.mcp_definitions(alice).unwrap()).unwrap();
            assert!(!definitions.contains("never-in-definitions"));
            assert!(!definitions.contains("syntheticRefreshToken"));
            assert!(!format!("{saved:?}").contains("never-in-definitions"));
            let launch = store
                .mcp_launch_secrets(alice, "vana", &edit.target)
                .unwrap();
            assert!(launch.environment.is_empty() && launch.headers.is_empty());
            let paths = store.paths().clone();
            store.close().unwrap();
            drop(store);
            let reopened = Store::unavailable(paths, StoreState::Maintenance).unwrap();
            if keyed {
                reopened
                    .initialize_key_material(
                        keys::LockedKey::from_bytes([0xa1; 32]).unwrap(),
                        engine::key_connection,
                    )
                    .unwrap();
            } else {
                reopened.initialize_plain().unwrap();
            }
            assert_eq!(
                reopened
                    .mcp_oauth_credentials(alice, "vana", &edit.target)
                    .unwrap(),
                Some(saved)
            );
        }
    }

    #[test]
    fn oauth_rotations_and_reconnect_generations_refuse_late_callbacks() {
        for keyed in [false, true] {
            let (_dir, store, alice, _bob) = fixture(keyed);
            let edit = edit(47321);
            let first = store
                .install_mcp_oauth(alice, None, &edit, payload("first"))
                .unwrap();
            let rotated = store
                .rotate_mcp_oauth_credentials(
                    alice,
                    "vana",
                    &edit.target,
                    &first,
                    payload("rotated"),
                )
                .unwrap();
            assert_eq!(rotated.revision, 1);
            assert_eq!(
                store.rotate_mcp_oauth_credentials(
                    alice,
                    "vana",
                    &edit.target,
                    &first,
                    payload("stale")
                ),
                Err(StoreError::Conflict)
            );
            assert!(
                !store
                    .clear_mcp_oauth_credentials(alice, "vana", &edit.target, &first)
                    .unwrap()
            );
            let expected = store.mcp_definitions(alice).unwrap().remove(0);
            let reconnected = store
                .install_mcp_oauth(alice, Some(&expected), &edit, payload("reconnected"))
                .unwrap();
            assert_ne!(reconnected.generation, rotated.generation);
            assert_eq!(reconnected.revision, 0);
            assert_eq!(
                store.rotate_mcp_oauth_credentials(
                    alice,
                    "vana",
                    &edit.target,
                    &rotated,
                    payload("resurrect")
                ),
                Err(StoreError::Conflict)
            );
            assert!(
                !store
                    .clear_mcp_oauth_credentials(alice, "vana", &edit.target, &rotated)
                    .unwrap()
            );
            assert!(
                store
                    .clear_mcp_oauth_credentials(alice, "vana", &edit.target, &reconnected)
                    .unwrap()
            );
            assert_eq!(
                store.rotate_mcp_oauth_credentials(
                    alice,
                    "vana",
                    &edit.target,
                    &reconnected,
                    payload("after-clear")
                ),
                Err(StoreError::Conflict)
            );
            assert_eq!(
                store
                    .mcp_oauth_credentials(alice, "vana", &edit.target)
                    .unwrap(),
                None
            );
            let replacement = store
                .install_mcp_oauth(alice, Some(&expected), &edit, payload("after-revocation"))
                .unwrap();
            assert_ne!(replacement.generation, reconnected.generation);
        }
    }

    #[test]
    fn oauth_install_preserves_siblings_and_rejects_changed_or_custom_definitions() {
        let (_dir, store, alice, _bob) = fixture(false);
        let mut sibling = edit(47111);
        sibling.key = "sibling".into();
        sibling.name = "Sibling".into();
        store
            .save_mcp_definitions(alice, &[sibling.clone()], McpEditOrigin::Local)
            .unwrap();
        let vana = edit(47321);
        store
            .install_mcp_oauth(alice, None, &vana, payload("first"))
            .unwrap();
        let all = store.mcp_definitions(alice).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].key, "sibling");
        let expected = all[1].clone();
        let mut changed = vana.clone();
        changed.enabled = true;
        store
            .save_mcp_definitions(alice, &[sibling.clone(), changed], McpEditOrigin::Local)
            .unwrap();
        assert_eq!(
            store.install_mcp_oauth(alice, Some(&expected), &vana, payload("stale-install")),
            Err(StoreError::Conflict)
        );
        assert_eq!(
            store.install_mcp_oauth(alice, None, &sibling, payload("overwrite-unrelated")),
            Err(StoreError::Conflict)
        );
        let expected_sibling = store.mcp_definitions(alice).unwrap().remove(0);
        assert_eq!(
            store.install_mcp_oauth(
                alice,
                Some(&expected_sibling),
                &sibling,
                payload("overwrite-unrelated")
            ),
            Err(StoreError::Conflict)
        );
        assert_eq!(store.mcp_definitions(alice).unwrap().len(), 2);
    }

    #[test]
    fn generic_destination_or_credential_changes_clear_oauth_but_defaults_retain_it() {
        for keyed in [false, true] {
            let (_dir, store, alice, _bob) = fixture(keyed);
            let mut vana = edit(47321);
            let first = store
                .install_mcp_oauth(alice, None, &vana, payload("first"))
                .unwrap();
            vana.enabled = true;
            store
                .save_mcp_definitions(alice, &[vana.clone()], McpEditOrigin::Local)
                .unwrap();
            assert_eq!(
                store
                    .mcp_oauth_credentials(alice, "vana", &vana.target)
                    .unwrap(),
                Some(first)
            );
            vana.target = self::edit(47322).target;
            store
                .save_mcp_definitions(alice, &[vana.clone()], McpEditOrigin::Local)
                .unwrap();
            assert_eq!(
                store
                    .mcp_oauth_credentials(alice, "vana", &vana.target)
                    .unwrap(),
                None
            );
            store
                .save_mcp_definitions(alice, &[], McpEditOrigin::Local)
                .unwrap();
            vana.enabled = false;
            let oauth = store
                .install_mcp_oauth(alice, None, &vana, payload("second"))
                .unwrap();
            vana.headers.push(McpSecretEdit {
                name: "Authorization".into(),
                update: McpSecretUpdate::Set(SecretString::new("synthetic-header".into())),
            });
            store
                .save_mcp_definitions(alice, &[vana.clone()], McpEditOrigin::Local)
                .unwrap();
            assert_eq!(
                store
                    .mcp_oauth_credentials(alice, "vana", &vana.target)
                    .unwrap(),
                None
            );
            assert_eq!(
                store.rotate_mcp_oauth_credentials(
                    alice,
                    "vana",
                    &vana.target,
                    &oauth,
                    payload("stale")
                ),
                Err(StoreError::Conflict)
            );
        }
    }

    #[test]
    fn oauth_rows_cascade_on_definition_and_account_deletion() {
        let (_dir, store, alice, _bob) = fixture(false);
        let vana = edit(47321);
        store
            .install_mcp_oauth(alice, None, &vana, payload("first"))
            .unwrap();
        store
            .save_mcp_definitions(alice, &[], McpEditOrigin::Local)
            .unwrap();
        store
            .install_mcp_oauth(alice, None, &vana, payload("second"))
            .unwrap();
        store
            .read(|tx| {
                let count: i64 =
                    tx.query_row("SELECT count(*) FROM mcp_oauth_credentials", [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 1);
                Ok(())
            })
            .unwrap();
        store
            .write(Durability::Durable, |tx| {
                tx.execute("DELETE FROM accounts WHERE id=?1", [alice.0])?;
                let count: i64 =
                    tx.query_row("SELECT count(*) FROM mcp_oauth_credentials", [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 0);
                Ok(())
            })
            .unwrap();
    }
}
