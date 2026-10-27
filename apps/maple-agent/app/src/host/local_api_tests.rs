//! The local listener's safety controls remain available during store recovery.

use super::*;
use crate::local_api::{LocalApiConfig, LocalApiState, LocalApiStatus};
use maple_agent::store::records::{CredentialTokens, SecretString};
use maple_agent::store::{DamageReason, RefusalReason, StorePaths};
use serde_json::json;

#[test]
fn unavailable_stores_keep_api_status_and_emergency_stop_but_reject_configuration() {
    for state in [
        StoreState::Locked,
        StoreState::NeedsSetup,
        StoreState::Maintenance,
        StoreState::Damaged {
            reason: DamageReason::Generation,
        },
        StoreState::Refused {
            reason: RefusalReason::NewerVersion,
        },
    ] {
        for method in ["get_local_api_status", "stop_local_api"] {
            assert!(
                gate(&state, method).is_ok(),
                "{method} must stay available while the store is {state:?}"
            );
        }
        assert!(gate(&state, "set_local_api_config").is_err());
    }
}

fn directory() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("maple-api-")
            .tempdir_in("/tmp")
            .unwrap()
    }
    #[cfg(windows)]
    {
        tempfile::tempdir().unwrap()
    }
}

const USER: &str = "local-api-lifecycle@example.test";

fn credentials(backend: &LocalBackend) {
    let store = backend.store().unwrap();
    let account = store.upsert_account(USER, 1).unwrap();
    store
        .replace_credentials(
            None,
            account,
            backend.remote_api_url(),
            "local-api-lifecycle-session",
            1,
            &CredentialTokens {
                access_token: SecretString::new("synthetic-api-lifecycle-token".into()),
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

    async fn start_listener(&self) -> u16 {
        let reservation = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        // Only upstream account validation is supplied by the test. This is
        // the production listener, router, request boundary and cancellation.
        self.backend
            .local_api
            .start(
                LocalApiConfig {
                    enabled: true,
                    port,
                },
                USER.to_string(),
            )
            .await
            .unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            http.get(format!("http://127.0.0.1:{port}/health"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        assert_eq!(
            http.get(format!("http://127.0.0.1:{port}/v1/models"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        port
    }

    async fn finish(self) {
        self.backend.local_api.stop().await;
        self.client.detach().await;
        assert!(self.server.await.unwrap().unwrap());
        self.remote.shutdown().await;
        if self.store.state() == StoreState::Open {
            self.store.close_for_shutdown().unwrap();
        }
    }
}

async fn assert_closed(port: u16) {
    assert!(
        tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .is_err(),
        "the listening socket must be gone when cleanup acknowledges success"
    );
}

#[tokio::test]
async fn emergency_stop_over_ipc_closes_the_real_listener_and_preserves_requested_configuration() {
    let root = directory();
    let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
    credentials(&backend);
    let fixture = HostFixture::new(root.path(), backend).await;
    let port = fixture.start_listener().await;
    let status: LocalApiStatus = fixture
        .client
        .call("get_local_api_status", ())
        .await
        .unwrap();
    assert_eq!(status.state, LocalApiState::Running);
    assert!(status.config.enabled);
    assert_eq!(status.config.port, port);

    let stopped: LocalApiStatus = fixture.client.call("stop_local_api", ()).await.unwrap();
    assert_eq!(stopped.state, LocalApiState::Stopped);
    assert!(stopped.config.enabled);
    assert_eq!(stopped.config.port, port);
    assert_closed(port).await;

    // The UI switch has a different contract from emergency Stop: Off is
    // durable and must not require a network request just to stop spending.
    let disabled: LocalApiStatus = fixture
        .client
        .call(
            "set_local_api_config",
            (
                USER,
                LocalApiConfig {
                    enabled: false,
                    port,
                },
            ),
        )
        .await
        .unwrap();
    assert_eq!(disabled.state, LocalApiState::Stopped);
    assert!(!disabled.config.enabled);
    let account = fixture.store.account_id(USER).unwrap().unwrap();
    assert_eq!(
        fixture
            .store
            .account_setting(account, "local_api.config")
            .unwrap(),
        Some(json!({"enabled":false,"port":port}))
    );
    fixture.finish().await;
}

#[tokio::test]
async fn saved_account_and_enabled_preference_do_not_start_before_authentication_is_validated() {
    let root = directory();
    let backend = LocalBackend::isolated_for_test(root.path());
    credentials(&backend);
    let store = backend.store().unwrap();
    let account = store.account_id(USER).unwrap().unwrap();
    let reservation = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    store
        .set_account_setting(
            account,
            "local_api.config",
            &json!({"enabled":true,"port":port}),
            1,
        )
        .unwrap();
    assert_eq!(backend.saved_user_id().as_deref(), Some(USER));

    tokio::time::timeout(
        Duration::from_secs(3),
        backend.resume_local_api(backend.local_api.generation()),
    )
    .await
    .expect("resume must not try to validate a cached account implicitly")
    .unwrap();
    assert_eq!(backend.local_api.status().state, LocalApiState::Stopped);
    assert_closed(port).await;
    assert_eq!(
        store.account_setting(account, "local_api.config").unwrap(),
        Some(json!({"enabled":true,"port":port})),
        "an unvalidated restore must retain the account's desired preference"
    );
    store.close_for_shutdown().unwrap();
}

#[test]
fn failed_off_write_keeps_automatic_resume_paused_with_durable_on_retained() {
    use std::{future::Future, task::Poll};

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = directory();
        let backend = LocalBackend::isolated_for_test(root.path());
        credentials(&backend);
        let store = backend.store().unwrap();
        let account = store.account_id(USER).unwrap().unwrap();
        let old = json!({"enabled":true,"port":8123});
        store
            .set_account_setting(account, "local_api.config", &old, 1)
            .unwrap();

        // Hold this runtime's only blocking worker. The actual setter can
        // admit Off and stop the listener, but its durable write cannot run
        // until the public store lifecycle has closed the fixture database.
        let (entered, wait_entered) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = entered.send(());
            let _ = blocked.recv();
        });
        wait_entered.await.unwrap();
        {
            let off = backend.set_local_api_config(
                USER,
                LocalApiConfig {
                    enabled: false,
                    port: 8123,
                },
            );
            tokio::pin!(off);
            std::future::poll_fn(|cx| {
                assert!(off.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(backend.local_api_suspended_for_test());
            store.close().unwrap();
            release.send(()).unwrap();
            blocker.await.unwrap();
            assert_eq!(off.await.unwrap_err(), "Could not save Local API settings");
        }
        store.initialize_plain().unwrap();
        assert_eq!(
            store.account_setting(account, "local_api.config").unwrap(),
            Some(old),
            "the refused write must leave the old durable On preference intact"
        );
        assert!(backend.local_api_suspended_for_test());
        backend
            .resume_local_api(backend.local_api.generation())
            .await
            .unwrap();
        assert!(backend.local_api_suspended_for_test());
        assert_eq!(backend.local_api.status().state, LocalApiState::Stopped);
        store.close_for_shutdown().unwrap();
    });
    runtime.shutdown_timeout(Duration::from_secs(3));
}

#[tokio::test]
async fn logout_closes_the_listener_without_relying_on_the_account_event_pump() {
    let root = directory();
    let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
    credentials(&backend);
    let fixture = HostFixture::new(root.path(), backend).await;
    let port = fixture.start_listener().await;
    fixture
        .client
        .call::<()>("logout_and_clear", (USER,))
        .await
        .unwrap();
    assert!(fixture.backend.saved_user_id().is_none());
    assert_closed(port).await;
    let stopped: LocalApiStatus = fixture
        .client
        .call("get_local_api_status", ())
        .await
        .unwrap();
    assert_eq!(stopped.state, LocalApiState::Stopped);
    fixture.finish().await;
}

#[tokio::test]
async fn emergency_stop_during_confirmed_erasure_survives_a_precommit_cleanup_refusal() {
    let root = directory();
    let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
    credentials(&backend);
    let fixture = HostFixture::new(root.path(), backend).await;
    let port = fixture.start_listener().await;
    let generation = fixture.store.store_id().unwrap();
    let epoch = fixture.backend.local_api_control_epoch();
    let spills = maple_agent::store::spill::SpillRoot::new(fixture.store.paths()).unwrap();
    spills
        .task_dir("00000000-0000-4000-8000-000000000003")
        .unwrap();
    std::fs::remove_dir_all(spills.path()).unwrap();
    // An unsafe owned-spill type deterministically refuses before replacement
    // verification. No native checker, fake crypto or bypass is needed.
    std::fs::write(spills.path(), b"unsafe owned-spill type").unwrap();
    let admission = fixture.owner.admission.read().await;
    {
        let erasure = fixture
            .client
            .call::<serde_json::Value>("erase_local_data", json!({"confirmation":"ERASE"}));
        tokio::pin!(erasure);
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! {
                result = &mut erasure => panic!("erasure passed held admission: {result:?}"),
                _ = async {
                    while !fixture.owner.maintenance.load(Ordering::Acquire)
                        || fixture.backend.local_api.status().state != LocalApiState::Stopped
                    {
                        tokio::task::yield_now().await;
                    }
                } => {},
            }
        })
        .await
        .expect("maintenance must suspend the listener before waiting for readers");
        assert_eq!(fixture.backend.local_api_control_epoch(), epoch);
        assert_closed(port).await;
        let stopped: LocalApiStatus = tokio::time::timeout(
            Duration::from_secs(3),
            fixture.client.call("stop_local_api", ()),
        )
        .await
        .expect("emergency Stop must bypass held maintenance admission")
        .unwrap();
        assert_eq!(stopped.state, LocalApiState::Stopped);
        assert_eq!(fixture.backend.local_api_control_epoch(), epoch + 1);
        assert!(fixture.backend.local_api_suspended_for_test());
        drop(admission);
        assert!(erasure.await.is_err());
    }
    assert_eq!(fixture.backend.local_api_control_epoch(), epoch + 1);
    assert_eq!(fixture.store.store_id().unwrap(), generation);
    assert_eq!(fixture.store.state(), StoreState::Open);
    assert_closed(port).await;
    let stopped: LocalApiStatus = fixture
        .client
        .call("get_local_api_status", ())
        .await
        .unwrap();
    assert_eq!(stopped.state, LocalApiState::Stopped);
    assert!(stopped.config.enabled);
    fixture
        .backend
        .resume_local_api(fixture.backend.local_api.generation())
        .await
        .unwrap();
    assert!(fixture.backend.local_api_suspended_for_test());
    assert_eq!(
        fixture.backend.local_api.status().state,
        LocalApiState::Stopped
    );
    assert_closed(port).await;
    std::fs::remove_file(spills.path()).unwrap();
    fixture.finish().await;
}

#[tokio::test]
async fn owner_shutdown_closes_the_listener_before_releasing_local_data() {
    let root = directory();
    let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
    let fixture = HostFixture::new(root.path(), backend).await;
    let port = fixture.start_listener().await;
    tokio::time::timeout(
        Duration::from_secs(3),
        fixture
            .owner
            .shutdown(&fixture.backend, &fixture.remote, &fixture.events),
    )
    .await
    .expect("a local listener must not hold an account reader through shutdown")
    .unwrap();
    assert_ne!(fixture.store.state(), StoreState::Open);
    assert_closed(port).await;
    fixture.finish().await;
}

#[tokio::test]
async fn locked_profile_answers_status_and_stop_over_real_private_ipc() {
    let root = directory();
    let store = Arc::new(
        Store::unavailable(
            StorePaths::new(root.path().join("locked-store")),
            StoreState::Locked,
        )
        .unwrap(),
    );
    let backend = Arc::new(
        LocalBackend::new_with_store("http://127.0.0.1:1".into(), String::new(), store.clone())
            .unwrap(),
    );
    let receipts = Arc::new(receipts::Receipts::new(store.clone()));
    let (events, _) = broadcast::channel(32);
    let remote =
        crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts.clone());
    let owner = Arc::new(OwnerContext::new(Some(&store)));
    let endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
    let mut listener = endpoint.bind().unwrap();
    let (stop, _stopped) = mpsc::channel(1);
    let server = {
        let endpoint = endpoint.clone();
        let backend = backend.clone();
        let remote = remote.clone();
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
    assert_eq!(welcome.store, StoreState::Locked);
    let client = RemoteClient::attach(endpoint, connection, welcome);

    for method in ["get_local_api_status", "stop_local_api"] {
        let status = tokio::time::timeout(
            Duration::from_secs(3),
            client.call::<LocalApiStatus>(method, ()),
        )
        .await
        .expect("locked controls must not wait for a store or account gate")
        .unwrap();
        assert_eq!(status.state, LocalApiState::Stopped);
        assert_eq!(status.active_requests, 0);
    }
    let error = client
        .call::<LocalApiStatus>(
            "set_local_api_config",
            json!(["unavailable-account", {"enabled":true,"port":8080}]),
        )
        .await
        .unwrap_err();
    assert!(error.contains("locked"));
    assert_eq!(store.state(), StoreState::Locked);

    client.detach().await;
    assert!(server.await.unwrap().unwrap());
    remote.shutdown().await;
}
