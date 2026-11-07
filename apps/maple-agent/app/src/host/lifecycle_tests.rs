//! Real local IPC and OwnerContext lifecycle routing with disposable stores.
//! Keyed fixtures use SQLCipher and a real wrapping slot; no checker is bypassed.
use super::*;
use maple_agent::store::keys::{self, KdfParams};
use maple_agent::store::records::{CredentialTokens, SecretString};
use maple_agent::store::{StorePaths, lifecycle::StoreMode};
use serde_json::{Value, json};
use std::io::Read;

const USER: &str = "host-lifecycle@example.test";

fn directory() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("maple-life-")
            .tempdir_in("/tmp")
            .unwrap()
    }
    #[cfg(windows)]
    {
        tempfile::tempdir().unwrap()
    }
}

fn credentials(backend: &LocalBackend) {
    let store = backend.store().unwrap();
    let account = store.upsert_account(USER, 1).unwrap();
    store
        .replace_credentials(
            None,
            account,
            backend.remote_api_url(),
            "lifecycle-session",
            1,
            &CredentialTokens {
                access_token: SecretString::new("synthetic-token".into()),
                refresh_token: None,
            },
            1,
        )
        .unwrap();
    backend.reload_open_store().unwrap();
}

struct HostFixture {
    client: RemoteClient,
    backend: Arc<LocalBackend>,
    store: Arc<Store>,
    owner: Arc<OwnerContext>,
    remote: Arc<crate::remote::manager::Manager>,
    events: broadcast::Sender<Arc<AgentServiceEvent>>,
    server: tokio::task::JoinHandle<Result<bool, String>>,
}

impl HostFixture {
    async fn new(root: &std::path::Path, backend: Arc<LocalBackend>) -> Self {
        let store = backend.store().unwrap().clone();
        let receipts = Arc::new(receipts::Receipts::new(store.clone()));
        let (events, _) = broadcast::channel(32);
        let remote =
            crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts.clone());
        let owner = Arc::new(OwnerContext::new(Some(&store)));
        owner.passwords.use_floor_parameters_for_test().await;
        let endpoint = ipc::Endpoint::for_test(root.to_path_buf());
        let mut listener = endpoint.bind().unwrap();
        let (stop, _stopped) = mpsc::channel(1);
        let server = {
            let endpoint = endpoint.clone();
            let backend = backend.clone();
            let remote = remote.clone();
            let owner = owner.clone();
            let events = events.clone();
            tokio::spawn(async move {
                let connection = listener.accept().await.unwrap();
                serve_client(
                    connection, endpoint, backend, events, stop, receipts, remote, owner,
                )
                .await
            })
        };
        let (connection, welcome) = connect(&endpoint, ConnectionMode::Client, false)
            .await
            .unwrap();
        let client = RemoteClient::attach(endpoint, connection, welcome);
        Self {
            client,
            backend,
            store,
            owner,
            remote,
            events,
            server,
        }
    }

    async fn enable_remote(&self) {
        crate::remote::enable_saved_remote_for_test(&self.backend, USER).unwrap();
        self.remote.restore().await;
        assert!(self.remote.stays_running());
    }

    async fn finish(self) {
        self.client.detach().await;
        assert!(self.server.await.unwrap().unwrap());
        self.remote.shutdown().await;
        self.store.close_for_shutdown().unwrap();
    }
}

fn assert_no_maintenance(observed: &mut broadcast::Receiver<Arc<AgentServiceEvent>>) {
    while let Ok(event) = observed.try_recv() {
        assert!(!matches!(
            &*event,
            AgentServiceEvent::StoreStateChanged {
                state: StoreState::Maintenance
            }
        ));
    }
}

#[tokio::test]
async fn open_unlock_and_reset_preflight_refusals_preserve_attached_transports() {
    let root = directory();
    let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
    credentials(&backend);
    let fixture = HostFixture::new(root.path(), backend).await;
    fixture.enable_remote().await;
    let generation = fixture.store.store_id().unwrap();
    let draining = *fixture.owner.draining.borrow();
    let remote_revision = fixture.remote.lifecycle_revision_for_test();
    let spills = maple_agent::store::spill::SpillRoot::new(fixture.store.paths()).unwrap();
    let output = spills
        .task_dir("00000000-0000-4000-8000-000000000001")
        .unwrap()
        .join("output");
    std::fs::write(&output, b"retained before reset confirmation").unwrap();
    let mut observed = fixture.events.subscribe();

    for (method, params) in [
        ("unlock", json!({"password":"unused unlock input"})),
        ("erase_local_data", json!({"confirmation":"wrong"})),
        (
            "erase_local_data",
            json!({"confirmation":"ERASE", "extra":true}),
        ),
        ("start_fresh", json!({"confirmation":"START FRESH"})),
    ] {
        assert!(fixture.client.call::<Value>(method, params).await.is_err());
        assert_eq!(
            *fixture.owner.draining.borrow(),
            draining,
            "preflight must not drain ACP"
        );
        assert_eq!(
            fixture.remote.lifecycle_revision_for_test(),
            remote_revision,
            "outer response routing must not restart remote"
        );
        assert!(fixture.remote.stays_running());
        assert_eq!(fixture.store.state(), StoreState::Open);
        assert_eq!(fixture.store.store_id().unwrap(), generation);
        assert_eq!(fixture.backend.saved_user_id().as_deref(), Some(USER));
        assert_eq!(
            std::fs::read(&output).unwrap(),
            b"retained before reset confirmation"
        );
        assert!(!fixture.owner.busy.load(Ordering::Acquire));
        assert!(!fixture.owner.maintenance.load(Ordering::Acquire));
        assert_no_maintenance(&mut observed);
    }
    fixture.finish().await;
    spills.cleanup().unwrap();
}

#[tokio::test]
async fn key_only_rewrap_uses_real_crypto_without_draining_readers_or_transports() {
    let root = directory();
    let phrase = keys::generate_passphrase().unwrap();
    let key_bytes = [0x62; 32];
    let store = Arc::new(
        Store::open_keyed(
            StorePaths::new(root.path().join("keyed")),
            keys::LockedKey::from_bytes(key_bytes).unwrap(),
        )
        .unwrap(),
    );
    let mut salt = [0; 16];
    std::fs::File::open(store.paths().database())
        .unwrap()
        .read_exact(&mut salt)
        .unwrap();
    let slot = keys::KeyFile::create_generated(
        &phrase,
        &keys::LockedKey::from_bytes(key_bytes).unwrap(),
        salt,
        KdfParams::floor(),
    )
    .unwrap();
    keys::KeyFilePair::write(&store.paths().db(), &slot).unwrap();
    let backend = Arc::new(
        LocalBackend::new_with_store("http://127.0.0.1:1".into(), String::new(), store.clone())
            .unwrap(),
    );
    credentials(&backend);
    let fixture = HostFixture::new(root.path(), backend).await;
    fixture.enable_remote().await;
    let generation = store.store_id().unwrap();
    let remote_revision = fixture.remote.lifecycle_revision_for_test();
    let draining = *fixture.owner.draining.borrow();
    let mut observed = fixture.events.subscribe();
    let suggested: Value = fixture
        .client
        .call("generate_store_passphrase", ())
        .await
        .unwrap();
    let input = |current: &str, reencrypt: Option<Value>| {
        let mut input = json!({"current_password":current, "password":suggested["passphrase"], "generated_id":suggested["id"]});
        if let Some(reencrypt) = reencrypt {
            input["reencrypt"] = reencrypt;
        }
        input
    };

    // ACP and remote callbacks can hold these readers. A key-only rewrap must
    // remain compatible with them and with active work; requesting a writer
    // here would deterministically time out.
    let admission = fixture.owner.admission.read().await;
    let account = fixture.backend.account_gate.read().await;
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            fixture.client.call::<Value>(
                "change_store_password",
                input("incorrect current password", None)
            )
        )
        .await
        .unwrap()
        .is_err()
    );
    assert_eq!(
        fixture.remote.lifecycle_revision_for_test(),
        remote_revision
    );
    assert_eq!(*fixture.owner.draining.borrow(), draining);
    let changed: Value = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.client.call(
            "change_store_password",
            input(phrase.password().as_str(), None),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(changed["mode"], "password");
    assert_eq!(store.lock_status().unwrap().mode, StoreMode::Password);
    assert_eq!(store.store_id().unwrap(), generation);
    let pair = keys::KeyFilePair::load(&store.paths().db(), &salt).unwrap();
    let new_password =
        keys::Password::normalize(suggested["passphrase"].as_str().unwrap()).unwrap();
    assert!(pair.primary().unlock(&new_password).is_ok());
    assert!(pair.primary().unlock(phrase.password()).is_err());
    assert_eq!(
        fixture.remote.lifecycle_revision_for_test(),
        remote_revision
    );
    assert_eq!(*fixture.owner.draining.borrow(), draining);
    assert!(fixture.remote.stays_running());
    assert_no_maintenance(&mut observed);
    // Invalid types/unknown schema members cannot select the cheap lane.
    assert!(
        fixture
            .client
            .call::<Value>(
                "change_store_password",
                input(new_password.as_str(), Some(json!("true")))
            )
            .await
            .is_err()
    );
    assert_eq!(
        fixture.remote.lifecycle_revision_for_test(),
        remote_revision
    );
    drop(account);
    drop(admission);
    fixture.finish().await;
}

#[tokio::test]
async fn count_only_host_status_is_available_without_opening_nonopen_stores() {
    for state in [
        StoreState::Locked,
        StoreState::NeedsSetup,
        StoreState::Maintenance,
        StoreState::Damaged {
            reason: maple_agent::store::DamageReason::Database,
        },
        StoreState::Refused {
            reason: maple_agent::store::RefusalReason::NewerVersion,
        },
    ] {
        let root = directory();
        let store =
            Arc::new(Store::unavailable(StorePaths::new(root.path()), state.clone()).unwrap());
        let backend = Arc::new(
            LocalBackend::new_with_store("http://127.0.0.1:1".into(), String::new(), store.clone())
                .unwrap(),
        );
        let receipts = Arc::new(receipts::Receipts::new(store.clone()));
        let (events, _) = broadcast::channel(8);
        let remote =
            crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts.clone());
        let owner = OwnerContext::new(Some(&store));
        owner.acp_clients.store(2, Ordering::Release);
        let result = owner
            .dispatch(
                &backend,
                &remote,
                &events,
                protocol::SensitiveRequest(Request {
                    id: 1,
                    method: "host_status".into(),
                    send_id: None,
                    params: json!([]),
                }),
                receipts,
                &ConnectionGeneration(None, None),
            )
            .await
            .unwrap();
        assert_eq!(result["acp_clients"], 2);
        assert_eq!(result["running_tasks"], 0);
        assert_eq!(store.state(), state);
        assert!(!store.paths().database().exists());
        assert!(!store.paths().keys().exists());
    }
}

#[tokio::test]
async fn confirmed_reset_reports_owned_spill_cleanup_failure_before_replacing_store() {
    let root = directory();
    let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
    credentials(&backend);
    let fixture = HostFixture::new(root.path(), backend).await;
    fixture.enable_remote().await;
    let generation = fixture.store.store_id().unwrap();
    let spills = maple_agent::store::spill::SpillRoot::new(fixture.store.paths()).unwrap();
    spills
        .task_dir("00000000-0000-4000-8000-000000000002")
        .unwrap();
    std::fs::remove_dir_all(spills.path()).unwrap();
    std::fs::write(spills.path(), b"unsafe owned-spill type").unwrap();
    assert!(
        fixture
            .client
            .call::<Value>("erase_local_data", json!({"confirmation":"ERASE"}))
            .await
            .is_err()
    );
    assert_eq!(fixture.store.store_id().unwrap(), generation);
    assert_eq!(fixture.store.state(), StoreState::Open);
    assert_eq!(fixture.backend.saved_user_id().as_deref(), Some(USER));
    assert!(fixture.remote.stays_running());
    std::fs::remove_file(spills.path()).unwrap();
    fixture.finish().await;
}

#[tokio::test]
async fn shutdown_cancels_pending_oauth_before_waiting_for_its_lifecycle_readers() {
    let root = directory();
    let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
    let store = backend.store().unwrap().clone();
    let receipts = Arc::new(receipts::Receipts::new(store.clone()));
    let (events, _) = broadcast::channel(8);
    let remote = crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts);
    let owner = Arc::new(OwnerContext::new(Some(&store)));
    let admission = owner.admission.read().await;
    let account = backend.account_gate.write().await;
    let published = Arc::new(AtomicBool::new(false));
    let (release, waiting) = oneshot::channel();
    let oauth = backend.pending_oauth_waiter_for_test(waiting, published.clone());
    let stopping = {
        let owner = owner.clone();
        let backend = backend.clone();
        let remote = remote.clone();
        let events = events.clone();
        tokio::spawn(async move { owner.shutdown(&backend, &remote, &events).await })
    };
    assert!(
        tokio::time::timeout(Duration::from_secs(5), oauth)
            .await
            .unwrap()
            .is_err()
    );
    assert!(!published.load(Ordering::Acquire));
    assert!(
        release.send(()).is_err(),
        "cancelled browser result cannot publish late"
    );
    drop(account);
    drop(admission);
    tokio::time::timeout(Duration::from_secs(5), stopping)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn shutdown_cancels_oauth_registered_after_its_admission_writer_was_queued() {
    let root = directory();
    let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
    let store = backend.store().unwrap().clone();
    let receipts = Arc::new(receipts::Receipts::new(store.clone()));
    let (events, _) = broadcast::channel(8);
    let remote = crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts);
    let owner = Arc::new(OwnerContext::new(Some(&store)));
    let admission = owner.admission.read().await;
    let stopping = {
        let owner = owner.clone();
        let backend = backend.clone();
        let remote = remote.clone();
        let events = events.clone();
        tokio::spawn(async move { owner.shutdown(&backend, &remote, &events).await })
    };
    // Tokio's fair lock refuses a new reader once Stop's writer is queued.
    // This places registration after the coordinator's initial cancellation.
    tokio::time::timeout(Duration::from_secs(5), async {
        while owner.admission.try_read().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let account = backend.account_gate.write().await;
    let published = Arc::new(AtomicBool::new(false));
    let (release, waiting) = oneshot::channel();
    let oauth = backend.pending_oauth_waiter_for_test(waiting, published.clone());
    assert!(
        tokio::time::timeout(Duration::from_secs(5), oauth)
            .await
            .unwrap()
            .is_err()
    );
    assert!(!published.load(Ordering::Acquire));
    assert!(release.send(()).is_err());
    drop(account);
    drop(admission);
    tokio::time::timeout(Duration::from_secs(5), stopping)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn owner_dispatch_emits_semantic_gate_failure_before_admission() {
    let root = directory();
    let store =
        Arc::new(Store::unavailable(StorePaths::new(root.path()), StoreState::Locked).unwrap());
    let backend = Arc::new(
        LocalBackend::new_with_store("http://127.0.0.1:1".into(), String::new(), store.clone())
            .unwrap(),
    );
    let receipts = Arc::new(receipts::Receipts::new(store.clone()));
    let (events, _) = broadcast::channel(8);
    let remote =
        crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts.clone());
    let owner = OwnerContext::new(Some(&store));
    let _operation = owner.operation.lock().await;
    let _admission = owner.admission.write().await;
    let failure = tokio::time::timeout(
        Duration::from_millis(100),
        owner.dispatch(
            &backend,
            &remote,
            &events,
            protocol::SensitiveRequest(Request {
                id: 1,
                method: "login".into(),
                send_id: None,
                params: json!(["synthetic", "synthetic"]),
            }),
            receipts,
            &ConnectionGeneration(None, None),
        ),
    )
    .await
    .expect("locked rejection must not await admission")
    .unwrap_err();
    assert_eq!(failure.code, "store-locked");
    assert_eq!(failure.message, LOCKED_MESSAGE);
    assert!(!store.paths().database().exists());
    assert!(!store.paths().keys().exists());
}
