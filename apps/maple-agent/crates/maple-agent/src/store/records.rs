//! Host-only credentials, send admission, remote authority and MCP secrets.
//!
//! These APIs operate exclusively on the already-open new store. Secret-bearing
//! types deliberately implement neither serde serialization nor value Debug.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{AccountId, Durability, Store, StoreError, StoreResult, tasks};

mod oauth;
pub use oauth::McpOAuthCredentials;

const MAX_RECORD_BYTES: usize = 1024 * 1024;
// Match the host's bounded 160 MiB request envelope, which admits image-heavy
// sends. The smaller terminal-record limit must not shrink send admission.
const MAX_SEND_PARAMETERS_BYTES: usize = 160 * 1024 * 1024;
const MAX_MCP_SERVERS: usize = 256;
const MAX_SECRET_BYTES: usize = 64 * 1024;
const MAX_REMOTE_PEERS: usize = 64;
const MAX_ATTEMPTS: usize = 4096;

type StoredSendReceipt = (i64, Vec<u8>, String, Option<String>, bool);

/// A host-only value. Callers must not put it in protocol responses or logs.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RemoteSecret(Zeroizing<[u8; 32]>);

impl RemoteSecret {
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for RemoteSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteSecret([REDACTED])")
    }
}

/// Account, opaque SDK session and revision all fence a credential callback.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialOwner {
    pub account: AccountId,
    pub session_id: String,
    pub revision: u64,
}

impl fmt::Debug for CredentialOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialOwner")
            .field("account", &self.account)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialTokens {
    pub access_token: SecretString,
    pub refresh_token: Option<SecretString>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct StoredCredentials {
    pub owner: CredentialOwner,
    pub user_id: String,
    pub api_url: String,
    pub tokens: CredentialTokens,
    pub updated_ms: i64,
}

impl fmt::Debug for StoredCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredCredentials")
            .field("owner", &self.owner)
            .field("updated_ms", &self.updated_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SendOutcome {
    Missing,
    Pending,
    /// Effects may exist; never authorize replay of this send identity.
    Interrupted,
    Finished(Result<Value, String>),
}

/// Definitions may cross IPC. They contain names and presence bits, never values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpSecretPresence {
    pub name: String,
    pub has_value: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTarget {
    Stdio { command: String },
    StreamableHttp { url: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpDefinition {
    pub key: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub timeout_seconds: u64,
    pub target: McpTarget,
    pub environment: Vec<McpSecretPresence>,
    pub headers: Vec<McpSecretPresence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(
    tag = "operation",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum McpSecretUpdate {
    /// Retain only if this definition's target is unchanged.
    Keep,
    Set(SecretString),
    Clear,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSecretEdit {
    pub name: String,
    pub update: McpSecretUpdate,
}

/// Host input intentionally has no Serialize implementation.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerEdit {
    pub key: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub timeout_seconds: u64,
    pub target: McpTarget,
    pub environment: Vec<McpSecretEdit>,
    pub headers: Vec<McpSecretEdit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpEditOrigin {
    Local,
    Remote,
}

/// Only the host's process/request builder receives these values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpLaunchSecrets {
    pub definition: McpDefinition,
    pub environment: BTreeMap<String, SecretString>,
    pub headers: BTreeMap<String, SecretString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteNamespace {
    pub api_url: String,
    pub client_id: String,
    pub profile: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePeer {
    pub name: String,
    /// Incoming controller authority; never used as an outgoing host grant.
    pub grant: [u8; 16],
    pub controls_this_computer: bool,
    pub can_connect: bool,
    /// Confirmed outgoing authority, independent of the incoming direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_grant: Option<[u8; 16]>,
    /// Durable CPace/lost-ack intent, not confirmed outgoing authority.
    #[serde(default)]
    pub pending_confirmation: Option<[u8; 16]>,
}

impl fmt::Debug for RemotePeer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemotePeer")
            .field("controls_this_computer", &self.controls_this_computer)
            .field("can_connect", &self.can_connect)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteRecord {
    version: u32,
    peers: BTreeMap<String, RemotePeer>,
    #[serde(default)]
    attempted_windows: BTreeMap<String, Vec<[u8; 16]>>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RemoteIdentity {
    pub secret: RemoteSecret,
    pub enabled: bool,
    pub name: Option<String>,
    pub peers: BTreeMap<String, RemotePeer>,
    pub attempted_windows: BTreeMap<String, Vec<[u8; 16]>>,
}

impl fmt::Debug for RemoteIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteIdentity")
            .field("enabled", &self.enabled)
            .field("peer_count", &self.peers.len())
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Read only the profile's current credentials for the expected API origin.
    pub fn credentials(&self, api_url: &str) -> StoreResult<Option<StoredCredentials>> {
        self.credentials_matching(api_url, None)
    }

    /// Observe the physical credential slot for an explicit validated sign-in.
    /// An API-origin filter is not proof that the slot is empty. This returns
    /// only its CAS identity, never another origin's tokens. Normal restores,
    /// rotations and persistence retries must retain their original owner.
    /// An unresolved deletion reserves the slot until authoritative resolution.
    pub fn credential_owner_for_sign_in(&self) -> StoreResult<Option<CredentialOwner>> {
        self.read(|tx| {
            ensure_credential_slot_not_pending(tx)?;
            credential_owner_tx(tx)
        })
    }

    /// Non-secret status for resuming only the current credential slot's
    /// uncertain deletion operation after restart.
    pub fn pending_account_deletion(
        &self,
        api_url: &str,
    ) -> StoreResult<Option<(AccountId, String, String, i64)>> {
        self.read(|tx| {
            tx.query_row(
                "SELECT a.id,a.user_id,a.deletion_generation,a.deleting_ms FROM accounts a \
                 JOIN credentials c ON c.account_id=a.id WHERE c.slot=1 AND c.api_url=?1 \
                 AND a.deletion_phase='pending_server' AND a.deletion_generation IS NOT NULL AND a.deleting_ms IS NOT NULL",
                [api_url], |row| Ok((AccountId(row.get(0)?),row.get(1)?,row.get(2)?,row.get(3)?)),
            ).optional().map_err(Into::into)
        })
    }

    /// Host-only reconciliation of an uncertain account-deletion request. These
    /// credentials must never be installed as a normal runtime session. The
    /// caller must resolve this exact operation against authoritative server
    /// state, then confirm or abort through the account-deletion APIs.
    pub fn pending_deletion_credentials(
        &self,
        account: AccountId,
        generation: &str,
        api_url: &str,
    ) -> StoreResult<Option<StoredCredentials>> {
        uuid::Uuid::parse_str(generation).map_err(|_| StoreError::InvalidInput)?;
        self.credentials_matching(api_url, Some((account, generation)))
    }

    fn credentials_matching(
        &self,
        api_url: &str,
        pending: Option<(AccountId, &str)>,
    ) -> StoreResult<Option<StoredCredentials>> {
        self.read(|tx| {
            if let Some((account,generation))=pending {
                let matches: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM accounts WHERE id=?1 AND deletion_phase='pending_server' AND deletion_generation=?2 AND deleting_ms IS NOT NULL)",
                    params![account.0,generation],|row|row.get(0))?;
                if !matches { return Err(StoreError::Conflict); }
            }
            let record = tx.query_row(
                "SELECT c.account_id,c.session_id,c.revision,a.user_id,c.api_url,c.access_token,c.refresh_token,c.updated_ms \
                 FROM credentials c JOIN accounts a ON a.id=c.account_id \
                 WHERE c.slot=1 AND c.api_url=?1 AND ((?2 IS NULL AND a.deleting_ms IS NULL AND a.deletion_phase IS NULL) \
                 OR (a.id=?2 AND a.deletion_phase='pending_server' AND a.deletion_generation=?3 AND a.deleting_ms IS NOT NULL))",
                params![api_url,pending.map(|(account,_)|account.0),pending.map(|(_,generation)|generation)], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,row.get::<_, String>(4)?,Zeroizing::new(row.get::<_, Vec<u8>>(5)?),
                        Zeroizing::new(row.get::<_, Vec<u8>>(6)?),row.get::<_, i64>(7)?))
                },
            ).optional()?;
            record.map(|(account,session_id,revision,user_id,api_url,access,refresh,updated_ms)| {
                Ok(StoredCredentials {
                    owner: CredentialOwner { account: AccountId(account), session_id,
                        revision: revision.try_into().map_err(|_| StoreError::InvalidData)? },
                    user_id, api_url, tokens: CredentialTokens {
                        access_token: secret_from_owned_bytes(access)?, refresh_token: if refresh.is_empty() { None } else { Some(secret_from_owned_bytes(refresh)?) },
                    }, updated_ms,
                })
            }).transpose()
        })
    }

    /// Install a validated sign-in. None means the slot must still be empty;
    /// replacement requires the exact owner previously observed by the host.
    #[allow(clippy::too_many_arguments)] // Keep every CAS fence explicit at the host boundary.
    pub fn replace_credentials(
        &self,
        expected: Option<&CredentialOwner>,
        account: AccountId,
        api_url: &str,
        session_id: &str,
        revision: u64,
        tokens: &CredentialTokens,
        updated_ms: i64,
    ) -> StoreResult<CredentialOwner> {
        let replacement = CredentialReplacement {
            account,
            api_url,
            session_id,
            revision,
            tokens,
            updated_ms,
        };
        replacement.validate()?;
        let mut changed = false;
        let result = self.write(Durability::Durable, |tx| {
            tasks::writable_account(tx, account)?;
            // Recheck under the writer transaction: a deletion can begin after
            // a sign-in observed the slot, including at a different API origin.
            ensure_credential_slot_not_pending(tx)?;
            let owner = replace_credentials_tx(tx, expected, &replacement)?;
            changed = true;
            Ok(owner)
        });
        // Only a matching CAS may invalidate the prior authenticated session.
        // A subsequent commit/synchronization error is conservatively revoked.
        if changed
            && let Some(previous) = expected
            && (previous.account != account || previous.session_id != session_id)
            && let Err(error) = self.cancel_account_replays(previous.account)
        {
            log::warn!("Replaced account replay cleanup failed: {error}");
        }
        result
    }

    /// Persist SDK refresh during deletion reconciliation without making the
    /// pending account available to normal authentication or changing its API.
    /// The caller must retain the original owner until this durable CAS succeeds.
    #[allow(clippy::too_many_arguments)] // Includes the independent pending-operation fence.
    pub fn replace_pending_deletion_credentials(
        &self,
        expected: &CredentialOwner,
        generation: &str,
        api_url: &str,
        session_id: &str,
        revision: u64,
        tokens: &CredentialTokens,
        updated_ms: i64,
    ) -> StoreResult<CredentialOwner> {
        uuid::Uuid::parse_str(generation).map_err(|_| StoreError::InvalidInput)?;
        let replacement = CredentialReplacement {
            account: expected.account,
            api_url,
            session_id,
            revision,
            tokens,
            updated_ms,
        };
        replacement.validate()?;
        self.write(Durability::Durable, |tx| {
            let pending: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM accounts a JOIN credentials c ON c.account_id=a.id \
                 WHERE a.id=?1 AND a.deletion_phase='pending_server' AND a.deletion_generation=?2 \
                 AND a.deleting_ms IS NOT NULL AND c.slot=1 AND c.api_url=?3)",
                params![expected.account.0, generation, api_url],
                |row| row.get(0),
            )?;
            if !pending {
                return Err(StoreError::Conflict);
            }
            replace_credentials_tx(tx, Some(expected), &replacement)
        })
    }

    pub fn rotate_credentials(
        &self,
        expected: &CredentialOwner,
        tokens: &CredentialTokens,
        updated_ms: i64,
    ) -> StoreResult<CredentialOwner> {
        validate_tokens(tokens)?;
        let next = expected
            .revision
            .checked_add(1)
            .ok_or(StoreError::InvalidInput)?;
        let revision = revision_i64(expected.revision)?;
        revision_i64(next)?;
        self.write(Durability::Durable, |tx| {
            tasks::writable_account(tx, expected.account)?;
            let changed = tx.execute(
                "UPDATE credentials SET access_token=?4,refresh_token=?5,revision=revision+1,updated_ms=?6 \
                 WHERE slot=1 AND account_id=?1 AND session_id=?2 AND revision=?3",
                params![expected.account.0,expected.session_id,revision,tokens.access_token.expose().as_bytes(),
                    refresh_token_bytes(tokens),updated_ms],
            )?;
            if changed != 1 { return Err(StoreError::Conflict); }
            Ok(CredentialOwner { revision: next, ..expected.clone() })
        })
    }

    /// A stale sign-out cannot clear a newer login or token rotation.
    pub fn clear_credentials(&self, expected: &CredentialOwner) -> StoreResult<bool> {
        let revision = revision_i64(expected.revision)?;
        let mut changed = false;
        let result = self.write(Durability::Durable, |tx| {
            tasks::writable_account(tx, expected.account)?;
            changed = tx.execute("DELETE FROM credentials WHERE slot=1 AND account_id=?1 AND session_id=?2 AND revision=?3",
                params![expected.account.0,expected.session_id,revision])? == 1;
            Ok(changed)
        });
        if changed && let Err(error) = self.cancel_account_replays(expected.account) {
            log::warn!("Signed-out account replay cleanup failed: {error}");
        }
        result
    }

    /// Returns None only after a new durable reservation has committed. Dispatch
    /// must happen after this method returns; a failed call never authorizes it.
    pub fn reserve_send(
        &self,
        account: AccountId,
        id: &str,
        task_uuid: &str,
        parameters: &Value,
        created_ms: i64,
    ) -> StoreResult<Option<SendOutcome>> {
        uuid::Uuid::parse_str(id).map_err(|_| StoreError::InvalidInput)?;
        let fingerprint = send_fingerprint(parameters)?;
        self.write(Durability::Durable, |tx| {
            tasks::writable_account(tx, account)?;
            let previous: Option<StoredSendReceipt> = tx.query_row(
                "SELECT account_id,fingerprint,task_uuid,result,interrupted FROM send_receipts WHERE id=?1",
                [id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
            ).optional()?;
            if let Some((owner,saved,task,result,interrupted)) = previous {
                if owner != account.0 || saved != fingerprint || task != task_uuid { return Err(StoreError::Conflict); }
                return Ok(Some(decode_outcome(result,interrupted)?));
            }
            tasks::task_key_tx(tx, account, task_uuid)?;
            tx.execute("INSERT INTO send_receipts(id,account_id,fingerprint,task_uuid,created_ms) VALUES (?1,?2,?3,?4,?5)",
                params![id,account.0,fingerprint.as_slice(),task_uuid,created_ms])?;
            Ok(None)
        })
    }

    pub fn send_outcome(&self, account: AccountId, id: &str) -> StoreResult<SendOutcome> {
        self.read(|tx| {
            tasks::writable_account(tx, account)?;
            let result: Option<(Option<String>, bool)> = tx
                .query_row(
                    "SELECT result,interrupted FROM send_receipts WHERE id=?1 AND account_id=?2",
                    params![id, account.0],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            result.map_or(Ok(SendOutcome::Missing), |(result, interrupted)| {
                decode_outcome(result, interrupted)
            })
        })
    }

    /// Completion is immutable and durable. A recovered interrupted reservation
    /// cannot become replayable or be overwritten by a late completion.
    pub fn finish_send(
        &self,
        account: AccountId,
        id: &str,
        result: &Result<Value, String>,
    ) -> StoreResult<()> {
        let result =
            Zeroizing::new(serde_json::to_string(result).map_err(|_| StoreError::InvalidInput)?);
        if result.len() > MAX_RECORD_BYTES {
            return Err(StoreError::InvalidInput);
        }
        self.write(Durability::Durable, |tx| {
            tasks::writable_account(tx, account)?;
            let current: Option<(Option<String>,bool)> = tx.query_row(
                "SELECT result,interrupted FROM send_receipts WHERE id=?1 AND account_id=?2",
                params![id,account.0], |row| Ok((row.get(0)?,row.get(1)?)),
            ).optional()?;
            match current {
                None => Err(StoreError::NotFound),
                Some((Some(saved),_)) if saved == *result => Ok(()),
                Some((Some(_),_)) | Some((None,true)) => Err(StoreError::Conflict),
                Some((None,false)) => {
                    tx.execute("UPDATE send_receipts SET result=?3 WHERE id=?1 AND account_id=?2 AND result IS NULL AND interrupted=0",
                        params![id,account.0,result.as_str()])?;
                    Ok(())
                }
            }
        })
    }

    /// Bound the cleanup and keep all uncertain identities reserved forever.
    pub fn prune_finished_send_receipts(
        &self,
        account: AccountId,
        now_ms: i64,
    ) -> StoreResult<usize> {
        let cutoff = now_ms.saturating_sub(7 * 24 * 60 * 60 * 1000);
        self.write(Durability::Durable, |tx| {
            tasks::writable_account(tx, account)?;
            Ok(tx.execute("DELETE FROM send_receipts WHERE account_id=?1 AND id IN \
                (SELECT id FROM send_receipts WHERE account_id=?1 AND result IS NOT NULL AND interrupted=0 AND created_ms<?2 ORDER BY created_ms LIMIT 1000)",
                params![account.0,cutoff])?)
        })
    }
}

fn checked_text(value: &str, maximum: usize) -> StoreResult<()> {
    if value.is_empty() || value.len() > maximum || value.contains('\0') {
        Err(StoreError::InvalidInput)
    } else {
        Ok(())
    }
}

fn revision_i64(value: u64) -> StoreResult<i64> {
    value.try_into().map_err(|_| StoreError::InvalidInput)
}

fn validate_tokens(tokens: &CredentialTokens) -> StoreResult<()> {
    checked_text(tokens.access_token.expose(), MAX_SECRET_BYTES)?;
    if let Some(refresh) = &tokens.refresh_token {
        checked_text(refresh.expose(), MAX_SECRET_BYTES)?;
    }
    Ok(())
}

fn refresh_token_bytes(tokens: &CredentialTokens) -> &[u8] {
    tokens
        .refresh_token
        .as_ref()
        .map_or(&[], |value| value.expose().as_bytes())
}

fn secret_from_bytes(bytes: Vec<u8>) -> StoreResult<SecretString> {
    // String::from_utf8 returns ownership of its input on error; wipe that too.
    match String::from_utf8(bytes) {
        Ok(value) => Ok(SecretString::new(value)),
        Err(error) => {
            let _wipe = Zeroizing::new(error.into_bytes());
            Err(StoreError::InvalidData)
        }
    }
}

fn secret_from_owned_bytes(mut bytes: Zeroizing<Vec<u8>>) -> StoreResult<SecretString> {
    secret_from_bytes(std::mem::take(&mut *bytes))
}

struct CredentialReplacement<'a> {
    account: AccountId,
    api_url: &'a str,
    session_id: &'a str,
    revision: u64,
    tokens: &'a CredentialTokens,
    updated_ms: i64,
}

impl CredentialReplacement<'_> {
    fn validate(&self) -> StoreResult<()> {
        checked_text(self.api_url, 8192)?;
        checked_text(self.session_id, 4096)?;
        validate_tokens(self.tokens)?;
        revision_i64(self.revision)?;
        Ok(())
    }
}

fn replace_credentials_tx(
    tx: &Transaction<'_>,
    expected: Option<&CredentialOwner>,
    replacement: &CredentialReplacement<'_>,
) -> StoreResult<CredentialOwner> {
    let CredentialReplacement {
        account,
        api_url,
        session_id,
        revision,
        tokens,
        updated_ms,
    } = replacement;
    let current = credential_owner_tx(tx)?;
    if current.as_ref() != expected {
        return Err(StoreError::Conflict);
    }
    if let Some(current) = &current
        && current.account == *account
        && current.session_id == *session_id
        && *revision < current.revision
    {
        return Err(StoreError::Conflict);
    }
    if let Some(current) = &current
        && current.account == *account
        && current.session_id == *session_id
        && *revision == current.revision
    {
        let identical: bool = tx.query_row("SELECT api_url=?1 AND access_token=?2 AND refresh_token=?3 FROM credentials WHERE slot=1",
            params![api_url,tokens.access_token.expose().as_bytes(),refresh_token_bytes(tokens)],|row|row.get(0))?;
        return if identical {
            Ok(current.clone())
        } else {
            Err(StoreError::Conflict)
        };
    }
    tx.execute(
        "INSERT INTO credentials(slot,account_id,api_url,session_id,revision,access_token,refresh_token,updated_ms) \
         VALUES (1,?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(slot) DO UPDATE SET \
         account_id=excluded.account_id,api_url=excluded.api_url,session_id=excluded.session_id,revision=excluded.revision,\
         access_token=excluded.access_token,refresh_token=excluded.refresh_token,updated_ms=excluded.updated_ms",
        params![account.0,api_url,session_id,revision_i64(*revision)?,tokens.access_token.expose().as_bytes(),
            refresh_token_bytes(tokens),updated_ms],
    )?;
    Ok(CredentialOwner {
        account: *account,
        session_id: (*session_id).to_owned(),
        revision: *revision,
    })
}

fn ensure_credential_slot_not_pending(tx: &Transaction<'_>) -> StoreResult<()> {
    let pending: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM credentials c JOIN accounts a ON a.id=c.account_id \
         WHERE c.slot=1 AND a.deletion_phase='pending_server')",
        [],
        |row| row.get(0),
    )?;
    if pending {
        Err(StoreError::Conflict)
    } else {
        Ok(())
    }
}

fn credential_owner_tx(tx: &Transaction<'_>) -> StoreResult<Option<CredentialOwner>> {
    let owner: Option<(i64, String, i64)> = tx
        .query_row(
            "SELECT account_id,session_id,revision FROM credentials WHERE slot=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    owner
        .map(|(account, session_id, revision)| {
            Ok(CredentialOwner {
                account: AccountId(account),
                session_id,
                revision: revision.try_into().map_err(|_| StoreError::InvalidData)?,
            })
        })
        .transpose()
}

fn decode_outcome(result: Option<String>, interrupted: bool) -> StoreResult<SendOutcome> {
    match result {
        Some(result) => serde_json::from_str(&result)
            .map(SendOutcome::Finished)
            .map_err(|_| StoreError::InvalidData),
        None if interrupted => Ok(SendOutcome::Interrupted),
        None => Ok(SendOutcome::Pending),
    }
}

fn send_fingerprint(parameters: &Value) -> StoreResult<[u8; 32]> {
    fn update(
        value: &Value,
        hash: &mut Sha256,
        bytes: &mut usize,
        depth: usize,
    ) -> StoreResult<()> {
        if depth > 128 {
            return Err(StoreError::InvalidInput);
        }
        match value {
            Value::Array(values) => {
                hash.update(b"[");
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        hash.update(b",");
                    }
                    update(value, hash, bytes, depth + 1)?;
                }
                hash.update(b"]");
            }
            Value::Object(values) => {
                hash.update(b"{");
                let ordered: BTreeMap<_, _> = values.iter().collect();
                for (index, (key, value)) in ordered.into_iter().enumerate() {
                    if index != 0 {
                        hash.update(b",");
                    }
                    let encoded = Zeroizing::new(
                        serde_json::to_vec(key).map_err(|_| StoreError::InvalidInput)?,
                    );
                    *bytes = bytes.saturating_add(encoded.len());
                    hash.update(&*encoded);
                    hash.update(b":");
                    update(value, hash, bytes, depth + 1)?;
                }
                hash.update(b"}");
            }
            _ => {
                let encoded = Zeroizing::new(
                    serde_json::to_vec(value).map_err(|_| StoreError::InvalidInput)?,
                );
                *bytes = bytes.saturating_add(encoded.len());
                hash.update(&*encoded);
            }
        }
        if *bytes > MAX_SEND_PARAMETERS_BYTES {
            return Err(StoreError::InvalidInput);
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    update(parameters, &mut hash, &mut 0, 0)?;
    Ok(hash.finalize().into())
}

impl Store {
    /// Replace the account's configured server list atomically. Keep retains a
    /// named value only for its current target; omitted names are removed.
    /// Remote target changes require Set or Clear for every saved value.
    pub fn save_mcp_definitions(
        &self,
        account: AccountId,
        edits: &[McpServerEdit],
        origin: McpEditOrigin,
    ) -> StoreResult<()> {
        if edits.len() > MAX_MCP_SERVERS {
            return Err(StoreError::InvalidInput);
        }
        let mut keys = BTreeSet::new();
        let mut definitions = Vec::with_capacity(edits.len());
        for edit in edits {
            if !keys.insert(edit.key.as_str()) {
                return Err(StoreError::InvalidInput);
            }
            definitions.push(mcp_definition(edit)?);
        }
        self.write(Durability::Durable, |tx| {
            tasks::writable_account(tx, account)?;
            let mut statement = tx.prepare("SELECT id,key FROM mcp_servers WHERE account_id=?1")?;
            let old = statement.query_map([account.0],|row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            for (position,(edit,definition)) in edits.iter().zip(&definitions).enumerate() {
                let old_server = mcp_server_tx(tx,account,&edit.key)?;
                let old_values = if let Some((id,_)) = &old_server { mcp_values_tx(tx,*id)? } else { BTreeMap::new() };
                let target = mcp_target_hash(&definition.target)?;
                // Ordinary metadata/default toggles retain OAuth. Changing its
                // destination or adding generic credential inputs cannot reuse
                // the dedicated OAuth grant.
                if (old_server.as_ref().is_some_and(|(_,old)| old.target != definition.target)
                    || !definition.environment.is_empty()
                    || !definition.headers.is_empty())
                    && let Some((id,_)) = &old_server
                {
                    tx.execute("DELETE FROM mcp_oauth_credentials WHERE server_id=?1",[id])?;
                }
                if origin == McpEditOrigin::Remote && old_server.as_ref().is_some_and(|(_,old)| old.target != definition.target) {
                    let explicit: BTreeSet<_> = edit.environment.iter()
                        .filter(|value| !matches!(&value.update, McpSecretUpdate::Keep))
                        .map(|value| ("env",value.name.clone()))
                        .chain(edit.headers.iter()
                            .filter(|value| !matches!(&value.update, McpSecretUpdate::Keep))
                            .map(|value| ("header",value.name.to_ascii_lowercase()))).collect();
                    if old_values.keys().any(|name| !explicit.contains(&(name.0.as_str(),name.1.clone()))) {
                        return Err(StoreError::Conflict);
                    }
                }
                let definition_json = serde_json::to_string(definition).map_err(|_| StoreError::InvalidInput)?;
                let id: i64 = tx.query_row(
                    "INSERT INTO mcp_servers(account_id,key,position,definition) VALUES (?1,?2,?3,?4) \
                     ON CONFLICT(account_id,key) DO UPDATE SET position=excluded.position,definition=excluded.definition RETURNING id",
                    params![account.0,edit.key,position as i64,definition_json], |row| row.get(0),
                )?;
                tx.execute("DELETE FROM mcp_server_secrets WHERE server_id=?1",[id])?;
                for (kind,values) in [("env",&edit.environment),("header",&edit.headers)] {
                    for edit_value in values {
                        let name = if kind == "header" { edit_value.name.to_ascii_lowercase() } else { edit_value.name.clone() };
                        let previous = old_values.get(&(kind.to_owned(),name.clone()));
                        let value = match &edit_value.update {
                            McpSecretUpdate::Set(value) => Some(value),
                            McpSecretUpdate::Keep => previous.filter(|(saved_target,_)| saved_target == &target).map(|(_,value)| value),
                            McpSecretUpdate::Clear => None,
                        };
                        if let Some(value) = value {
                            tx.execute("INSERT INTO mcp_server_secrets(server_id,kind,name,target,value) VALUES (?1,?2,?3,?4,?5)",
                                params![id,kind,name,target.as_slice(),value.expose().as_bytes()])?;
                        }
                    }
                }
            }
            for (id,key) in old {
                if !keys.contains(key.as_str()) {
                    tx.execute("DELETE FROM mcp_servers WHERE id=?1 AND account_id=?2",params![id,account.0])?;
                }
            }
            Ok(())
        })
    }

    /// Safe for settings/remote responses: a value can never be serialized here.
    pub fn mcp_definitions(&self, account: AccountId) -> StoreResult<Vec<McpDefinition>> {
        self.read(|tx| {
            tasks::writable_account(tx, account)?;
            let mut statement = tx.prepare(
                "SELECT id,definition FROM mcp_servers WHERE account_id=?1 ORDER BY position,id",
            )?;
            let records = statement
                .query_map([account.0], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            records
                .into_iter()
                .map(|(id, json)| {
                    let mut definition = decode_mcp_definition(&json)?;
                    let target = mcp_target_hash(&definition.target)?;
                    let mut values = tx.prepare(
                        "SELECT kind,name FROM mcp_server_secrets WHERE server_id=?1 AND target=?2",
                    )?;
                    let present = values
                        .query_map(params![id, target.as_slice()], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })?
                        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
                    for (kind, names) in [
                        ("env", &mut definition.environment),
                        ("header", &mut definition.headers),
                    ] {
                        for name in names {
                            name.has_value =
                                present.contains(&(kind.to_owned(), name.name.clone()));
                        }
                    }
                    Ok(definition)
                })
                .collect()
        })
    }

    /// Host-only injection. A changed definition cannot receive the values of a
    /// definition the caller inspected earlier, even across a concurrent save.
    pub fn mcp_launch_secrets(
        &self,
        account: AccountId,
        key: &str,
        expected_target: &McpTarget,
    ) -> StoreResult<McpLaunchSecrets> {
        let expected = mcp_target_hash(expected_target)?;
        self.read(|tx| {
            tasks::writable_account(tx, account)?;
            let (id, definition) = mcp_server_tx(tx, account, key)?.ok_or(StoreError::NotFound)?;
            if mcp_target_hash(&definition.target)? != expected {
                return Err(StoreError::Conflict);
            }
            let mut environment = BTreeMap::new();
            let mut headers = BTreeMap::new();
            let names: BTreeSet<_> = definition
                .environment
                .iter()
                .map(|value| ("env", value.name.as_str()))
                .chain(
                    definition
                        .headers
                        .iter()
                        .map(|value| ("header", value.name.as_str())),
                )
                .collect();
            for ((kind, name), (target, value)) in mcp_values_tx(tx, id)? {
                if target != expected || !names.contains(&(kind.as_str(), name.as_str())) {
                    continue;
                }
                match kind.as_str() {
                    "env" => {
                        environment.insert(name, value);
                    }
                    "header" => {
                        headers.insert(name, value);
                    }
                    _ => return Err(StoreError::InvalidData),
                }
            }
            Ok(McpLaunchSecrets {
                definition,
                environment,
                headers,
            })
        })
    }
}

fn canonical_mcp_target(target: &McpTarget) -> StoreResult<McpTarget> {
    match target {
        McpTarget::Stdio { command } => {
            checked_text(command.trim(), 8192)?;
            Ok(McpTarget::Stdio {
                command: command.trim().to_owned(),
            })
        }
        McpTarget::StreamableHttp { url } => {
            checked_text(url.trim(), 8192)?;
            let parsed = reqwest::Url::parse(url.trim()).map_err(|_| StoreError::InvalidInput)?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                return Err(StoreError::InvalidInput);
            }
            Ok(McpTarget::StreamableHttp {
                url: parsed.to_string(),
            })
        }
    }
}

fn mcp_target_hash(target: &McpTarget) -> StoreResult<[u8; 32]> {
    let canonical = canonical_mcp_target(target)?;
    let bytes = serde_json::to_vec(&canonical).map_err(|_| StoreError::InvalidInput)?;
    Ok(Sha256::digest(bytes).into())
}

fn mcp_definition(edit: &McpServerEdit) -> StoreResult<McpDefinition> {
    checked_text(&edit.name, 256)?;
    if edit.name.trim() != edit.name
        || edit.name.chars().count() > 64
        || edit.name.chars().any(char::is_control)
        || edit.description.len() > 16 * 1024
        || edit.description.contains('\0')
        || edit.timeout_seconds == 0
        || edit.timeout_seconds > 86400
        || edit.key.is_empty()
        || crate::agent::AgentMcpServer::key_for_name(&edit.name) != edit.key
    {
        return Err(StoreError::InvalidInput);
    }
    let target = canonical_mcp_target(&edit.target)?;
    if matches!(target, McpTarget::Stdio { .. })
        && edit
            .headers
            .iter()
            .any(|value| !matches!(&value.update, McpSecretUpdate::Clear))
    {
        return Err(StoreError::InvalidInput);
    }
    let names = |values: &[McpSecretEdit], header: bool| -> StoreResult<Vec<McpSecretPresence>> {
        if values.len() > 256 {
            return Err(StoreError::InvalidInput);
        }
        let mut seen = BTreeSet::new();
        values
            .iter()
            .map(|edit| {
                let name = if header {
                    http::header::HeaderName::from_bytes(edit.name.as_bytes())
                        .map_err(|_| StoreError::InvalidInput)?
                        .as_str()
                        .to_owned()
                } else {
                    let mut chars = edit.name.chars();
                    if !chars
                        .next()
                        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
                        || !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
                        || edit.name.len() > 256
                    {
                        return Err(StoreError::InvalidInput);
                    }
                    edit.name.clone()
                };
                if !seen.insert(name.clone()) {
                    return Err(StoreError::InvalidInput);
                }
                if let McpSecretUpdate::Set(value) = &edit.update {
                    if value.expose().len() > MAX_SECRET_BYTES || value.expose().contains('\0') {
                        return Err(StoreError::InvalidInput);
                    }
                    if header {
                        http::HeaderValue::from_str(value.expose())
                            .map_err(|_| StoreError::InvalidInput)?;
                    }
                }
                Ok(McpSecretPresence {
                    name,
                    has_value: false,
                })
            })
            .collect()
    };
    let mut headers = names(&edit.headers, true)?;
    if matches!(target, McpTarget::Stdio { .. }) {
        // Clear edits acknowledge credentials from the previous HTTP target;
        // the resulting stdio definition has no HTTP header names.
        headers.clear();
    }
    Ok(McpDefinition {
        key: edit.key.clone(),
        name: edit.name.clone(),
        description: edit.description.clone(),
        enabled: edit.enabled,
        timeout_seconds: edit.timeout_seconds,
        target,
        environment: names(&edit.environment, false)?,
        headers,
    })
}

fn decode_mcp_definition(json: &str) -> StoreResult<McpDefinition> {
    if json.len() > MAX_RECORD_BYTES {
        return Err(StoreError::InvalidData);
    }
    let definition: McpDefinition =
        serde_json::from_str(json).map_err(|_| StoreError::InvalidData)?;
    let edit = McpServerEdit {
        key: definition.key.clone(),
        name: definition.name.clone(),
        description: definition.description.clone(),
        enabled: definition.enabled,
        timeout_seconds: definition.timeout_seconds,
        target: definition.target.clone(),
        environment: definition
            .environment
            .iter()
            .map(|name| McpSecretEdit {
                name: name.name.clone(),
                update: McpSecretUpdate::Keep,
            })
            .collect(),
        headers: definition
            .headers
            .iter()
            .map(|name| McpSecretEdit {
                name: name.name.clone(),
                update: McpSecretUpdate::Keep,
            })
            .collect(),
    };
    let mut checked = mcp_definition(&edit).map_err(|_| StoreError::InvalidData)?;
    checked.environment.clone_from(&definition.environment);
    checked.headers.clone_from(&definition.headers);
    if checked != definition {
        return Err(StoreError::InvalidData);
    }
    Ok(definition)
}

fn mcp_server_tx(
    tx: &Transaction<'_>,
    account: AccountId,
    key: &str,
) -> StoreResult<Option<(i64, McpDefinition)>> {
    let record = tx
        .query_row(
            "SELECT id,definition FROM mcp_servers WHERE account_id=?1 AND key=?2",
            params![account.0, key],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    record
        .map(|(id, json)| Ok((id, decode_mcp_definition(&json)?)))
        .transpose()
}

type McpValues = BTreeMap<(String, String), ([u8; 32], SecretString)>;

fn mcp_values_tx(tx: &Transaction<'_>, server: i64) -> StoreResult<McpValues> {
    let mut statement =
        tx.prepare("SELECT kind,name,target,value FROM mcp_server_secrets WHERE server_id=?1")?;
    let mut rows = statement.query([server])?;
    let mut values = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let name: String = row.get(1)?;
        let target: Vec<u8> = row.get(2)?;
        let value = secret_from_bytes(row.get(3)?)?;
        values.insert(
            (kind, name),
            (
                target.try_into().map_err(|_| StoreError::InvalidData)?,
                value,
            ),
        );
    }
    Ok(values)
}

impl Store {
    /// Generate an identity only in this new store. Discovery and token refresh
    /// never supply its key, grants or previously attempted pairing windows.
    pub fn remote_identity(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
    ) -> StoreResult<RemoteIdentity> {
        validate_namespace(namespace)?;
        self.write(Durability::Durable, |tx| {
            tasks::writable_account(tx,account)?;
            let scope = remote_scope_tx(tx,account,namespace)?;
            if let Some(identity) = remote_identity_tx(tx,account,&scope)? { return Ok(identity); }
            let identity = RemoteIdentity { secret:RemoteSecret(Zeroizing::new(rand::random())),enabled:false,
                name:None,peers:BTreeMap::new(),attempted_windows:BTreeMap::new() };
            let record = remote_record_json(&identity)?;
            tx.execute("INSERT INTO remote_identities(scope,account_id,secret,enabled,name,record,updated_ms) VALUES (?1,?2,?3,0,NULL,?4,?5)",
                params![scope,account.0,identity.secret.expose().as_slice(),record,super::now_ms()])?;
            Ok(identity)
        })
    }

    /// Checking existence never generates or reconstructs authority.
    pub fn saved_remote_identity(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
    ) -> StoreResult<Option<RemoteIdentity>> {
        validate_namespace(namespace)?;
        self.read(|tx| {
            tasks::writable_account(tx, account)?;
            let scope = remote_scope_tx(tx, account, namespace)?;
            remote_identity_tx(tx, account, &scope)
        })
    }

    pub fn set_remote_enabled(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
        enabled: bool,
    ) -> StoreResult<()> {
        self.edit_remote(account, namespace, Durability::Durable, |identity| {
            identity.enabled = enabled;
            Ok(())
        })
    }

    pub fn set_remote_name(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
        name: &str,
    ) -> StoreResult<()> {
        let name = checked_computer_name(name)?;
        self.edit_remote(account, namespace, Durability::Durable, |identity| {
            identity.name = Some(name);
            Ok(())
        })
    }

    /// Directional grants merge rather than revoking the opposite direction.
    pub fn grant_remote_peer(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
        peer_key: &str,
        peer: RemotePeer,
    ) -> StoreResult<()> {
        checked_peer_key(peer_key)?;
        checked_computer_name(&peer.name)?;
        self.edit_remote(account, namespace, Durability::Durable, |identity| {
            let mut peer = peer;
            // Incoming pairing cannot introduce or replace outgoing authority.
            peer.can_connect = false;
            peer.host_grant = None;
            peer.pending_confirmation = None;
            if let Some(previous) = identity.peers.get(peer_key) {
                peer.can_connect = previous.can_connect;
                peer.host_grant = previous.host_grant;
                peer.pending_confirmation = previous.pending_confirmation;
            }
            identity.peers.insert(peer_key.to_owned(), peer);
            Ok(())
        })
    }

    pub fn remember_remote_host_proof(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
        peer_key: &str,
        window: [u8; 16],
        name: &str,
    ) -> StoreResult<()> {
        checked_peer_key(peer_key)?;
        let name = checked_computer_name(name)?;
        self.edit_remote(account, namespace, Durability::Durable, |identity| {
            let peer = identity
                .peers
                .entry(peer_key.to_owned())
                .or_insert(RemotePeer {
                    name: name.clone(),
                    grant: [0; 16],
                    controls_this_computer: false,
                    can_connect: false,
                    host_grant: None,
                    pending_confirmation: None,
                });
            peer.name = name;
            peer.pending_confirmation = Some(window);
            Ok(())
        })
    }

    /// Grant controller authority only after the exact pending proof was acked.
    pub fn confirm_remote_host(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
        peer_key: &str,
        window: [u8; 16],
    ) -> StoreResult<()> {
        checked_peer_key(peer_key)?;
        if window == [0; 16] {
            return Err(StoreError::InvalidInput);
        }
        self.edit_remote(account, namespace, Durability::Durable, |identity| {
            let peer = identity
                .peers
                .get_mut(peer_key)
                .ok_or(StoreError::NotFound)?;
            if peer.pending_confirmation != Some(window) {
                return Err(StoreError::Conflict);
            }
            peer.host_grant = Some(window);
            peer.pending_confirmation = None;
            peer.can_connect = true;
            Ok(())
        })
    }

    /// Persist an authenticated host confirmation for an existing legacy peer.
    /// The caller verifies the exact endpoint and acknowledgement before this
    /// compare-and-set; attempt history supplies candidates, never authority.
    pub fn recover_remote_host_grant(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
        peer_key: &str,
        expected_peer: &RemotePeer,
        expected_attempts: &[[u8; 16]],
        window: [u8; 16],
    ) -> StoreResult<()> {
        checked_peer_key(peer_key)?;
        if window == [0; 16] {
            return Err(StoreError::InvalidInput);
        }
        self.edit_remote(account, namespace, Durability::Durable, |identity| {
            let attempts = identity.attempted_windows.get(peer_key).map(Vec::as_slice);
            if attempts != Some(expected_attempts) || !expected_attempts.contains(&window) {
                return Err(StoreError::Conflict);
            }
            let peer = identity
                .peers
                .get_mut(peer_key)
                .ok_or(StoreError::Conflict)?;
            // Discovery may rename a peer while the host confirms its binding.
            // Only authority fields participate in this compare-and-set.
            if peer.grant != expected_peer.grant
                || peer.controls_this_computer != expected_peer.controls_this_computer
                || peer.can_connect != expected_peer.can_connect
                || peer.host_grant != expected_peer.host_grant
                || peer.pending_confirmation != expected_peer.pending_confirmation
                || !peer.can_connect
                || peer.host_grant.is_some()
                || peer.pending_confirmation.is_some()
            {
                return Err(StoreError::Conflict);
            }
            peer.host_grant = Some(window);
            Ok(())
        })
    }

    /// Commit before sending a code-derived proof. Attempts are never evicted;
    /// replayed discovery cannot turn a used window back into a fresh one.
    pub fn reserve_remote_pairing_attempt(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
        peer_key: &str,
        window: [u8; 16],
    ) -> StoreResult<()> {
        checked_peer_key(peer_key)?;
        self.edit_remote(account, namespace, Durability::Durable, |identity| {
            let windows = identity
                .attempted_windows
                .entry(peer_key.to_owned())
                .or_default();
            if windows.contains(&window) || windows.len() >= MAX_ATTEMPTS {
                return Err(StoreError::Conflict);
            }
            windows.push(window);
            Ok(())
        })
    }

    pub fn revoke_remote_peer(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
        peer_key: &str,
    ) -> StoreResult<()> {
        checked_peer_key(peer_key)?;
        self.edit_remote(account, namespace, Durability::Durable, |identity| {
            identity.peers.remove(peer_key);
            Ok(())
        })
    }

    /// Discovery may refresh labels of known peers only. It cannot create a
    /// grant, change its direction, enable remote access or forget an attempt.
    pub fn refresh_remote_peer_names(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
        names: &BTreeMap<String, String>,
    ) -> StoreResult<()> {
        if names.len() > MAX_REMOTE_PEERS {
            return Err(StoreError::InvalidInput);
        }
        let mut checked = BTreeMap::new();
        for (key, name) in names {
            checked_peer_key(key)?;
            checked.insert(key.clone(), checked_computer_name(name)?);
        }
        self.edit_remote(account, namespace, Durability::Normal, |identity| {
            for (key, name) in checked {
                if let Some(peer) = identity.peers.get_mut(&key) {
                    peer.name = name;
                }
            }
            Ok(())
        })
    }

    fn edit_remote(
        &self,
        account: AccountId,
        namespace: &RemoteNamespace,
        durability: Durability,
        edit: impl FnOnce(&mut RemoteIdentity) -> StoreResult<()>,
    ) -> StoreResult<()> {
        validate_namespace(namespace)?;
        self.write(durability,|tx| {
            tasks::writable_account(tx,account)?;
            let scope = remote_scope_tx(tx,account,namespace)?;
            let mut identity = remote_identity_tx(tx,account,&scope)?.ok_or(StoreError::NotFound)?;
            edit(&mut identity)?;
            let record = remote_record_json(&identity)?;
            let changed = tx.execute("UPDATE remote_identities SET enabled=?3,name=?4,record=?5,updated_ms=?6 WHERE scope=?1 AND account_id=?2",
                params![scope,account.0,identity.enabled,identity.name,record,super::now_ms()])?;
            if changed != 1 { return Err(StoreError::Conflict); }
            Ok(())
        })
    }
}

fn checked_computer_name(name: &str) -> StoreResult<String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 128
        || name.chars().count() > 64
        || name.chars().any(char::is_control)
    {
        return Err(StoreError::InvalidInput);
    }
    Ok(name.to_owned())
}

fn checked_peer_key(key: &str) -> StoreResult<()> {
    // Iroh's canonical EndpointId representation is a lowercase 32-byte hex key.
    // The app's Iroh boundary additionally decodes/verifies the public key.
    if key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(StoreError::InvalidInput)
    }
}

fn validate_namespace(namespace: &RemoteNamespace) -> StoreResult<()> {
    checked_text(&namespace.api_url, 8192)?;
    checked_text(&namespace.client_id, 1024)?;
    checked_text(&namespace.profile, 1024)
}

fn remote_scope_tx(
    tx: &Transaction<'_>,
    account: AccountId,
    namespace: &RemoteNamespace,
) -> StoreResult<String> {
    let user_key: String = tx.query_row(
        "SELECT user_key FROM accounts WHERE id=?1",
        [account.0],
        |row| row.get(0),
    )?;
    let mut hash = Sha256::new();
    for part in [
        "maple.remote.identity.v1",
        &namespace.api_url,
        &namespace.client_id,
        &namespace.profile,
        &user_key,
    ] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn remote_record_json(identity: &RemoteIdentity) -> StoreResult<String> {
    validate_remote_identity(identity)?;
    let record = RemoteRecord {
        version: 1,
        peers: identity.peers.clone(),
        attempted_windows: identity.attempted_windows.clone(),
    };
    let json = serde_json::to_string(&record).map_err(|_| StoreError::InvalidData)?;
    // A full 64-peer/4096-window history can exceed 1 MiB; the structural bounds
    // above preserve the existing bounded lifetime history without evictions.
    if json.len() > 32 * MAX_RECORD_BYTES {
        return Err(StoreError::InvalidInput);
    }
    Ok(json)
}

fn validate_remote_identity(identity: &RemoteIdentity) -> StoreResult<()> {
    if identity.peers.len() > MAX_REMOTE_PEERS
        || identity.attempted_windows.len() > MAX_REMOTE_PEERS
    {
        return Err(StoreError::Conflict);
    }
    if let Some(name) = &identity.name {
        checked_computer_name(name)?;
    }
    for (key, peer) in &identity.peers {
        checked_peer_key(key)?;
        checked_computer_name(&peer.name)?;
        if peer.host_grant == Some([0; 16]) || (peer.host_grant.is_some() && !peer.can_connect) {
            return Err(StoreError::InvalidInput);
        }
    }
    for (key, windows) in &identity.attempted_windows {
        checked_peer_key(key)?;
        if windows.len() > MAX_ATTEMPTS
            || windows.iter().collect::<BTreeSet<_>>().len() != windows.len()
        {
            return Err(StoreError::Conflict);
        }
    }
    Ok(())
}

fn remote_identity_tx(
    tx: &Transaction<'_>,
    account: AccountId,
    scope: &str,
) -> StoreResult<Option<RemoteIdentity>> {
    let record = tx.query_row("SELECT secret,enabled,name,record FROM remote_identities WHERE scope=?1 AND account_id=?2",
        params![scope,account.0],|row|Ok((Zeroizing::new(row.get::<_,Vec<u8>>(0)?),row.get::<_,bool>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,String>(3)?))).optional()?;
    record
        .map(|(secret, enabled, name, json)| {
            if secret.len() != 32 || json.len() > 32 * MAX_RECORD_BYTES {
                return Err(StoreError::InvalidData);
            }
            let record: RemoteRecord =
                serde_json::from_str(&json).map_err(|_| StoreError::InvalidData)?;
            if record.version != 1 {
                return Err(StoreError::InvalidData);
            }
            let mut key = Zeroizing::new([0; 32]);
            key.copy_from_slice(&secret);
            let identity = RemoteIdentity {
                secret: RemoteSecret(key),
                enabled,
                name,
                peers: record.peers,
                attempted_windows: record.attempted_windows,
            };
            validate_remote_identity(&identity).map_err(|_| StoreError::InvalidData)?;
            Ok(identity)
        })
        .transpose()
}

#[cfg(test)]
mod tests;
