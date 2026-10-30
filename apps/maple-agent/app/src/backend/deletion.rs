//! A server request attempt is not permission to collect local account data.
//!
//! The server currently has no deletion-status or idempotency endpoint. In
//! particular, rejection of a retry cannot resolve an earlier lost response.

use std::future::Future;
use std::sync::Arc;

use maple_agent::store::records::{CredentialTokens, SecretString, StoredCredentials};
use maple_agent::store::{AccountId, Store};
use maple_sdk::OpenSecretClient;

use super::LocalBackend;

pub(super) const PENDING_DELETION_MESSAGE: &str = "Account deletion has an unknown outcome. Maple has retained your local history and blocked this account. Local history will remain retained until a successful server confirmation or an authoritative server resolution is available. Signing out or a rejected retry cannot resolve an earlier lost response.";
const WRONG_CODE_MESSAGE: &str = "That confirmation code is wrong or has expired";

/// Non-secret status is available after restart without restoring normal auth.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AccountDeletionStatus {
    pub user_id: String,
    pub started_ms: i64,
    pub message: String,
}

struct Intent {
    account: AccountId,
    generation: String,
    /// Only a rejection of the first dispatch can safely clear this marker.
    resumed: bool,
    credentials: StoredCredentials,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Success,
    RejectedCode,
    Uncertain,
}

struct Attempt {
    outcome: Outcome,
    /// SDK refresh may rotate credentials even when confirmation fails.
    tokens: Option<CredentialTokens>,
}

trait DeletionTransport: Send {
    fn confirm(self, code: String, secret: String) -> impl Future<Output = Attempt> + Send;
}

struct SdkTransport(OpenSecretClient);

impl SdkTransport {
    fn prepare(api_url: &str, tokens: &CredentialTokens) -> Result<Self, String> {
        let environment = maple_agent::open_secret_config::configured_pcr0_environment()?;
        let client = OpenSecretClient::new_with_pcr0_environment(api_url.to_owned(), environment)
            .map_err(|_| "Could not prepare account deletion".to_string())?
            .with_client_app(maple_sdk::ClientApp::Work);
        client
            .set_tokens(
                tokens.access_token.expose().to_owned(),
                tokens
                    .refresh_token
                    .as_ref()
                    .map(|token| token.expose().to_owned()),
            )
            .map_err(|_| "Could not prepare account deletion credentials".to_string())?;
        Ok(Self(client))
    }
}

impl DeletionTransport for SdkTransport {
    async fn confirm(self, code: String, secret: String) -> Attempt {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.0.confirm_account_deletion(code, secret),
        )
        .await;
        let outcome = match result {
            Ok(Ok(())) => Outcome::Success,
            // Error::Api is the authenticated inner response. Transport,
            // framing, authentication, and response-decoding failures retain
            // the marker even if their surrounding HTTP status is 400.
            Ok(Err(maple_sdk::Error::Api { status: 400, .. })) => Outcome::RejectedCode,
            _ => Outcome::Uncertain,
        };
        let tokens = self
            .0
            .get_tokens()
            .ok()
            .flatten()
            .map(|tokens| CredentialTokens {
                access_token: SecretString::new(tokens.access_token),
                refresh_token: tokens.refresh_token.map(SecretString::new),
            });
        Attempt { outcome, tokens }
    }
}

impl LocalBackend {
    pub(super) fn pending_deletion_status(&self) -> Result<Option<AccountDeletionStatus>, String> {
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        let pending = store
            .pending_account_deletion(&self.api_url)
            .map_err(|_| "Could not read account deletion status".to_string())?;
        Ok(
            pending.map(|(_, user_id, _, started_ms)| AccountDeletionStatus {
                user_id,
                started_ms,
                message: PENDING_DELETION_MESSAGE.to_owned(),
            }),
        )
    }

    pub async fn account_deletion_status(&self) -> Result<Option<AccountDeletionStatus>, String> {
        self.pending_deletion_status()
    }

    /// Host dispatch drains remote/ACP work before taking the account writer
    /// gate. This method drains local work, then durably blocks the account
    /// before the first possible server dispatch. It never restores a pending
    /// credential into the ordinary authentication/runtime machinery.
    pub async fn confirm_account_deletion(
        &self,
        user_id: &str,
        confirmation_code: String,
        plaintext_secret: String,
    ) -> Result<(), String> {
        self.confirm_account_deletion_using(
            user_id,
            confirmation_code,
            plaintext_secret,
            |tokens| SdkTransport::prepare(&self.api_url, tokens),
        )
        .await
    }

    async fn confirm_account_deletion_using<D: DeletionTransport>(
        &self,
        user_id: &str,
        confirmation_code: String,
        plaintext_secret: String,
        prepare: impl FnOnce(&CredentialTokens) -> Result<D, String>,
    ) -> Result<(), String> {
        let code = confirmation_code.trim().to_owned();
        if code.is_empty() {
            return Err("Enter the confirmation code from the email".into());
        }
        if code.len() > 256 || plaintext_secret.is_empty() || plaintext_secret.len() > 256 {
            return Err("The account deletion confirmation is invalid".into());
        }
        // The durable generation also fences delayed responses; this lock
        // serializes local retries against login/logout and credential restore.
        let _account = self.restore_lock.lock().await;
        let store = self
            .store
            .as_ref()
            .ok_or("Local store is unavailable")?
            .clone();
        let initial = lookup_intent(&store, &self.api_url, user_id)?;

        self.service.drain_account(user_id).await?;
        // Host dispatch has also drained remote/ACP work under its exclusive
        // ownership gate. Every writer of this new store's transient spill
        // namespace has stopped; clearing it needs no history-deletion grant.
        let spill_store = Arc::clone(&store);
        blocking(move || maple_agent::store::spill::SpillRoot::new(spill_store.paths())
            .and_then(|spills| spills.cleanup())
            .map_err(|_| "Could not clear temporary task output. No new account deletion request was sent.".to_string())).await?;
        self.cancel_oauth();
        self.auth.clear_auth(user_id).await?;

        // Shutdown may finish an SDK refresh. Capture the durable pair only
        // after all normal sessions have been invalidated.
        let mut intent = lookup_intent(&store, &self.api_url, user_id)?;
        if intent.account != initial.account
            || intent.resumed != initial.resumed
            || (intent.resumed && intent.generation != initial.generation)
        {
            return Err("The host account changed. Reopen this screen before continuing.".into());
        }
        if !intent.resumed {
            intent.generation = initial.generation;
        }
        let transport = prepare(&intent.credentials.tokens)?;
        let begin_store = Arc::clone(&store);
        let account = intent.account;
        let generation = intent.generation.clone();
        // Clear the frontend identity before an uncancellable writer job: even
        // if this request is cancelled while FULL sync finishes, a committed
        // pending marker cannot leave cached normal authorization behind.
        let _ = self.persisted_auth.forget_owner();
        if let Err(error) = blocking(move || {
            begin_store
                .begin_account_deletion(account, &generation, chrono::Utc::now().timestamp_millis())
                .map_err(|_| "Could not save account deletion intent".to_string())
        })
        .await
        {
            let _ = self.load_persisted_auth();
            return Err(error);
        }

        // Cancellation from this point intentionally leaves a pending marker.
        let attempt = transport.confirm(code, plaintext_secret).await;
        let result = blocking(move || finish_attempt(&store, &intent, attempt)).await;
        // A proven first-dispatch rejection restores ordinary credentials;
        // pending/confirmed phases continue to return no normal identity.
        let _ = self.load_persisted_auth();
        result
    }
}

fn lookup_intent(store: &Store, api_url: &str, user_id: &str) -> Result<Intent, String> {
    if let Some((account, pending_user, generation, _)) = store
        .pending_account_deletion(api_url)
        .map_err(|_| "Could not read account deletion intent".to_string())?
    {
        if pending_user != user_id {
            return Err("The host account changed. Reopen this screen before continuing.".into());
        }
        let credentials = store
            .pending_deletion_credentials(account, &generation, api_url)
            .map_err(|_| PENDING_DELETION_MESSAGE.to_string())?
            .ok_or_else(|| PENDING_DELETION_MESSAGE.to_string())?;
        return Ok(Intent {
            account,
            generation,
            resumed: true,
            credentials,
        });
    }
    let credentials = store
        .credentials(api_url)
        .map_err(|_| "Could not read saved credentials".to_string())?
        .filter(|credentials| credentials.user_id == user_id)
        .ok_or("The host account changed. Reopen this screen before continuing.")?;
    Ok(Intent {
        account: credentials.owner.account,
        generation: uuid::Uuid::new_v4().to_string(),
        resumed: false,
        credentials,
    })
}

fn finish_attempt(store: &Store, intent: &Intent, attempt: Attempt) -> Result<(), String> {
    if attempt.outcome == Outcome::Success {
        return store.confirm_account_deletion(intent.account, &intent.generation)
            .map_err(|_| "The server confirmed account deletion, but Maple could not save that confirmation. Local history is retained pending recovery.".into());
    }
    let tokens = attempt
        .tokens
        .ok_or_else(|| PENDING_DELETION_MESSAGE.to_owned())?;
    if tokens != intent.credentials.tokens {
        let revision = intent
            .credentials
            .owner
            .revision
            .checked_add(1)
            .ok_or_else(|| PENDING_DELETION_MESSAGE.to_owned())?;
        store
            .replace_pending_deletion_credentials(
                &intent.credentials.owner,
                &intent.generation,
                &intent.credentials.api_url,
                &intent.credentials.owner.session_id,
                revision,
                &tokens,
                chrono::Utc::now().timestamp_millis(),
            )
            .map_err(|_| PENDING_DELETION_MESSAGE.to_owned())?;
    }
    if attempt.outcome == Outcome::RejectedCode && !intent.resumed {
        store
            .abort_account_deletion(intent.account, &intent.generation)
            .map_err(|_| PENDING_DELETION_MESSAGE.to_owned())?;
        return Err(WRONG_CODE_MESSAGE.to_owned());
    }
    Err(PENDING_DELETION_MESSAGE.to_owned())
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work).await.map_err(|_| {
        "Account deletion persistence was interrupted; local history is retained".to_string()
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use maple_agent::store::{AccountDeletion, NewTask, TaskKind, TaskPatch};

    struct FakeTransport {
        outcome: Outcome,
        tokens: Option<CredentialTokens>,
    }
    impl DeletionTransport for FakeTransport {
        async fn confirm(self, _code: String, _secret: String) -> Attempt {
            Attempt {
                outcome: self.outcome,
                tokens: self.tokens,
            }
        }
    }

    struct DelayedTransport {
        started: tokio::sync::oneshot::Sender<()>,
        reply: tokio::sync::oneshot::Receiver<Outcome>,
        tokens: CredentialTokens,
    }
    impl DeletionTransport for DelayedTransport {
        async fn confirm(self, _code: String, _secret: String) -> Attempt {
            let _ = self.started.send(());
            let outcome = self.reply.await.unwrap_or(Outcome::Uncertain);
            Attempt {
                outcome,
                tokens: Some(self.tokens),
            }
        }
    }

    fn fixture() -> (tempfile::TempDir, LocalBackend, AccountId, String) {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::isolated_for_test(root.path());
        backend.service.begin_draining(); // Maintenance must not race fixture assertions.
        let store = backend.store.as_ref().unwrap();
        let account = store.upsert_account("delete-user", 1).unwrap();
        store
            .replace_credentials(
                None,
                account,
                &backend.api_url,
                "fixture-sdk-session",
                0,
                &tokens("access"),
                1,
            )
            .unwrap();
        let task = uuid::Uuid::new_v4().to_string();
        store
            .create_task(
                account,
                &NewTask {
                    uuid: task.clone(),
                    project_root: project_root(),
                    kind: TaskKind::Desktop,
                    title: "retained history".into(),
                    provisional: false,
                    model: None,
                    created_ms: 1,
                },
            )
            .unwrap();
        (root, backend, account, task)
    }

    fn tokens(value: &str) -> CredentialTokens {
        CredentialTokens {
            access_token: SecretString::new(value.into()),
            refresh_token: Some(SecretString::new("refresh".into())),
        }
    }

    fn project_root() -> String {
        if cfg!(windows) { "C:/project" } else { "/tmp" }.into()
    }

    async fn attempt(backend: &LocalBackend, outcome: Outcome) -> Result<(), String> {
        backend
            .confirm_account_deletion_using(
                "delete-user",
                "code".into(),
                "secret".into(),
                |tokens| {
                    Ok(FakeTransport {
                        outcome,
                        tokens: Some(tokens.clone()),
                    })
                },
            )
            .await
    }

    #[tokio::test]
    async fn first_rejected_code_reopens_account_and_preserves_existing_tombstones() {
        let (_root, backend, account, task) = fixture();
        let store = backend.store.as_ref().unwrap();
        let old_deleted = uuid::Uuid::new_v4().to_string();
        store
            .create_task(
                account,
                &NewTask {
                    uuid: old_deleted.clone(),
                    project_root: project_root(),
                    kind: TaskKind::Desktop,
                    title: "already deleted".into(),
                    provisional: false,
                    model: None,
                    created_ms: 1,
                },
            )
            .unwrap();
        store.mark_task_deleted(account, &old_deleted, 2).unwrap();
        assert_eq!(
            attempt(&backend, Outcome::RejectedCode).await.unwrap_err(),
            WRONG_CODE_MESSAGE
        );
        assert_eq!(store.account_deletion(account).unwrap(), None);
        assert!(store.task(account, &task).unwrap().is_some());
        assert!(
            store
                .deleted_tasks(200)
                .unwrap()
                .iter()
                .any(|row| row.uuid == old_deleted)
        );
        assert!(store.credentials(&backend.api_url).unwrap().is_some());
    }

    #[tokio::test]
    async fn lost_response_and_rejected_retry_remain_pending_without_collection() {
        let (_root, backend, account, task) = fixture();
        let store = backend.store.as_ref().unwrap();
        assert_eq!(
            attempt(&backend, Outcome::Uncertain).await.unwrap_err(),
            PENDING_DELETION_MESSAGE
        );
        let pending = store.account_deletion(account).unwrap();
        assert!(matches!(pending, Some(AccountDeletion::Pending { .. })));
        assert_eq!(
            attempt(&backend, Outcome::RejectedCode).await.unwrap_err(),
            PENDING_DELETION_MESSAGE
        );
        assert_eq!(store.account_deletion(account).unwrap(), pending);
        assert!(store.task(account, &task).unwrap().is_some());
        assert!(store.collect_account_step(account).is_err());
        assert!(store.credentials(&backend.api_url).unwrap().is_none());
        assert!(
            store
                .update_task(
                    account,
                    &task,
                    &TaskPatch {
                        title: Some("blocked".into()),
                        ..Default::default()
                    }
                )
                .is_err()
        );
        assert!(backend.require_signed_out().is_err());
    }

    #[tokio::test]
    async fn explicit_success_confirms_only_target_account_and_allows_bounded_collection() {
        let (_root, backend, account, task) = fixture();
        let store = backend.store.as_ref().unwrap();
        let other = store.upsert_account("other-user", 1).unwrap();
        assert!(attempt(&backend, Outcome::Success).await.is_ok());
        assert!(matches!(
            store.account_deletion(account).unwrap(),
            Some(AccountDeletion::Confirmed { .. })
        ));
        assert!(store.task(account, &task).unwrap().is_none());
        assert_eq!(store.account_deletion(other).unwrap(), None);
        // Account metadata waits for the hidden task queue. Each task step is
        // independently authorized by the confirmed generation.
        assert!(matches!(
            store.collect_account_step(account),
            Err(maple_agent::store::StoreError::Busy)
        ));
        assert!(store.collect_task_step(account, &task).unwrap().done);
        assert!(store.collect_account_step(account).is_ok());
    }

    #[tokio::test]
    async fn pending_logout_preserves_credentials_and_restart_status() {
        let (root, backend, account, task) = fixture();
        attempt(&backend, Outcome::Uncertain).await.unwrap_err();
        backend.logout_and_clear("delete-user").await.unwrap();
        let status = backend.account_deletion_status().await.unwrap().unwrap();
        assert_eq!(status.user_id, "delete-user");
        let pending = backend
            .store
            .as_ref()
            .unwrap()
            .account_deletion(account)
            .unwrap();
        let generation = match pending {
            Some(AccountDeletion::Pending { generation, .. }) => generation,
            _ => panic!("pending"),
        };
        assert!(
            backend
                .store
                .as_ref()
                .unwrap()
                .pending_deletion_credentials(account, &generation, &backend.api_url)
                .unwrap()
                .is_some()
        );
        drop(backend);
        let reopened = LocalBackend::isolated_for_test(root.path());
        reopened.service.begin_draining();
        assert_eq!(
            reopened.account_deletion_status().await.unwrap().unwrap(),
            status
        );
        assert!(reopened.saved_user_id().is_none());
        assert!(
            reopened
                .store
                .as_ref()
                .unwrap()
                .task(account, &task)
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn refresh_after_failed_confirmation_is_durable_but_not_normal_auth() {
        let (_root, backend, account, _task) = fixture();
        backend
            .confirm_account_deletion_using("delete-user", "code".into(), "secret".into(), |_| {
                Ok(FakeTransport {
                    outcome: Outcome::Uncertain,
                    tokens: Some(tokens("rotated")),
                })
            })
            .await
            .unwrap_err();
        let store = backend.store.as_ref().unwrap();
        let generation = match store.account_deletion(account).unwrap() {
            Some(AccountDeletion::Pending { generation, .. }) => generation,
            _ => panic!("pending"),
        };
        let pending = store
            .pending_deletion_credentials(account, &generation, &backend.api_url)
            .unwrap()
            .unwrap();
        assert_eq!(pending.tokens.access_token.expose(), "rotated");
        assert_eq!(pending.owner.revision, 1);
        assert!(store.credentials(&backend.api_url).unwrap().is_none());
    }

    #[tokio::test]
    async fn cancellation_after_dispatch_keeps_the_pending_generation_and_history() {
        let (_root, backend, account, task) = fixture();
        let backend = Arc::new(backend);
        let (started, waiting) = tokio::sync::oneshot::channel();
        let (_reply, reply) = tokio::sync::oneshot::channel();
        let operation_backend = Arc::clone(&backend);
        let operation = tokio::spawn(async move {
            operation_backend
                .confirm_account_deletion_using(
                    "delete-user",
                    "code".into(),
                    "secret".into(),
                    |tokens| {
                        Ok(DelayedTransport {
                            started,
                            reply,
                            tokens: tokens.clone(),
                        })
                    },
                )
                .await
        });
        waiting.await.unwrap();
        operation.abort();
        let _ = operation.await;
        let store = backend.store.as_ref().unwrap();
        assert!(matches!(
            store.account_deletion(account).unwrap(),
            Some(AccountDeletion::Pending { .. })
        ));
        assert!(store.task(account, &task).unwrap().is_some());
        assert!(store.collect_account_step(account).is_err());
    }

    #[tokio::test]
    async fn a_second_confirmation_cannot_abort_an_in_flight_success() {
        let (_root, backend, account, _task) = fixture();
        let backend = Arc::new(backend);
        let (started, waiting) = tokio::sync::oneshot::channel();
        let (send_reply, reply) = tokio::sync::oneshot::channel();
        let first_backend = Arc::clone(&backend);
        let first = tokio::spawn(async move {
            first_backend
                .confirm_account_deletion_using(
                    "delete-user",
                    "code".into(),
                    "secret".into(),
                    |tokens| {
                        Ok(DelayedTransport {
                            started,
                            reply,
                            tokens: tokens.clone(),
                        })
                    },
                )
                .await
        });
        waiting.await.unwrap();
        let second_backend = Arc::clone(&backend);
        let second = tokio::spawn(async move {
            second_backend
                .confirm_account_deletion_using(
                    "delete-user",
                    "code".into(),
                    "secret".into(),
                    |_| -> Result<FakeTransport, String> {
                        panic!("must not dispatch after confirmed deletion")
                    },
                )
                .await
        });
        send_reply.send(Outcome::Success).unwrap();
        first.await.unwrap().unwrap();
        assert!(second.await.unwrap().is_err());
        assert!(matches!(
            backend
                .store
                .as_ref()
                .unwrap()
                .account_deletion(account)
                .unwrap(),
            Some(AccountDeletion::Confirmed { .. })
        ));
    }

    #[tokio::test]
    async fn wrong_account_and_stale_acknowledgement_cannot_confirm_another_generation() {
        let (_root, backend, account, _task) = fixture();
        assert!(
            backend
                .confirm_account_deletion_using(
                    "other-user",
                    "code".into(),
                    "secret".into(),
                    |_| -> Result<FakeTransport, String> { panic!("must not dispatch") }
                )
                .await
                .is_err()
        );
        let store = backend.store.as_ref().unwrap();
        let intent = lookup_intent(store, &backend.api_url, "delete-user").unwrap();
        store
            .begin_account_deletion(account, &intent.generation, 1)
            .unwrap();
        store
            .abort_account_deletion(account, &intent.generation)
            .unwrap();
        let new_generation = uuid::Uuid::new_v4().to_string();
        store
            .begin_account_deletion(account, &new_generation, 2)
            .unwrap();
        assert!(
            finish_attempt(
                store,
                &intent,
                Attempt {
                    outcome: Outcome::Success,
                    tokens: None
                }
            )
            .is_err()
        );
        assert!(
            matches!(store.account_deletion(account).unwrap(), Some(AccountDeletion::Pending { generation, .. }) if generation == new_generation)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsafe_spill_path_blocks_dispatch_and_preserves_previous_files() {
        let (root, backend, account, task) = fixture();
        let store = backend.store.as_ref().unwrap();
        let previous = root.path().join("previous-data");
        std::fs::create_dir(&previous).unwrap();
        std::fs::write(previous.join("sentinel"), b"untouched").unwrap();
        let spills = maple_agent::store::spill::SpillRoot::new(store.paths()).unwrap();
        spills.task_dir(&task).unwrap();
        std::fs::remove_dir_all(spills.path()).unwrap();
        std::os::unix::fs::symlink(&previous, spills.path()).unwrap();
        let result = backend
            .confirm_account_deletion_using(
                "delete-user",
                "code".into(),
                "secret".into(),
                |_| -> Result<FakeTransport, String> {
                    panic!("unsafe output must block dispatch")
                },
            )
            .await;
        assert!(
            result
                .unwrap_err()
                .contains("Could not clear temporary task output")
        );
        assert_eq!(store.account_deletion(account).unwrap(), None);
        assert!(store.task(account, &task).unwrap().is_some());
        assert_eq!(
            std::fs::read(previous.join("sentinel")).unwrap(),
            b"untouched"
        );
        std::fs::remove_file(spills.path()).unwrap();
        std::fs::remove_dir(spills.path().parent().unwrap()).unwrap();
    }
}
