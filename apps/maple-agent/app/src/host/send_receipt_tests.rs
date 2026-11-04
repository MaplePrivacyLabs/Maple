//! Post-dispatch receipt failures use real durable reservations and native IPC.
//! Model execution is a bounded fixture effect; no account or inference is used.

use super::*;
use maple_agent::store::{NewTask, StorePaths, TaskKind};
use serde_json::{Value, json};

const USER: &str = "receipt-fixture";
const RUN: &str = "already-admitted-run";

fn directory() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("maple-receipt-")
            .tempdir_in("/tmp")
            .unwrap()
    }
    #[cfg(windows)]
    {
        tempfile::tempdir().unwrap()
    }
}

fn seed(store: &Arc<Store>, root: &std::path::Path) -> (String, Value) {
    let account = store.upsert_account(USER, 1).unwrap();
    let task = uuid::Uuid::now_v7().to_string();
    store
        .create_task(
            account,
            &NewTask {
                uuid: task.clone(),
                project_root: root.to_string_lossy().into(),
                kind: TaskKind::Desktop,
                title: "receipt fixture".into(),
                provisional: false,
                model: None,
                created_ms: 1,
            },
        )
        .unwrap();
    (
        uuid::Uuid::new_v4().to_string(),
        json!([USER, {"sessionId":task, "text":"fixture send"}]),
    )
}

fn unavailable_receipts(store: &Store) -> Arc<receipts::Receipts> {
    // This handle opens no SQLite connections. It supplies a real failing
    // receipt sink while the authoritative fixture store retains its committed
    // Pending reservation for queries and deduplication.
    Arc::new(receipts::Receipts::new(Arc::new(
        Store::unavailable(store.paths().clone(), StoreState::Maintenance).unwrap(),
    )))
}

#[tokio::test]
async fn reservation_refusal_remains_a_definite_error_before_dispatch() {
    let root = directory();
    // LocalBackend owns Tokio resources; construct this isolated fixture in
    // the test runtime, like the existing host lifecycle fixtures.
    let backend = LocalBackend::isolated_for_test(root.path());
    let store = backend.store().unwrap().clone();
    let (_, params) = seed(&store, root.path());
    let receipts = Arc::new(receipts::Receipts::new(store.clone()));
    let invalid_identity = "not-a-valid-send-identity";
    let error = dispatch_with_receipt(
        &backend,
        Request {
            id: 1,
            method: "send_message".into(),
            send_id: Some(invalid_identity.into()),
            params,
        },
        receipts.clone(),
    )
    .await
    .unwrap_err();
    assert!(!error.starts_with("Send outcome unknown"));
    assert_eq!(
        receipts.outcome(USER, invalid_identity).unwrap(),
        receipts::Outcome::Missing
    );
    assert!(
        store
            .app_setting("receipt-dispatch-effect")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn failed_completion_preserves_pending_identity_for_query_and_deduplication() {
    let root = directory();
    let store = Arc::new(Store::open(StorePaths::new(root.path())).unwrap());
    let receipts = Arc::new(receipts::Receipts::new(store.clone()));
    for dispatched in [
        Ok(json!(RUN)),
        Err("dispatch refused before starting".into()),
    ] {
        let (id, params) = seed(&store, root.path());
        assert_eq!(receipts.reserve(&id, &params).unwrap(), None);
        store
            .set_app_setting("receipt-dispatch-effect", &json!(1), 2)
            .unwrap();
        let error = finish_admitted_send(
            unavailable_receipts(&store),
            USER.into(),
            id.clone(),
            dispatched.clone(),
        )
        .await
        .unwrap_err();
        assert!(error.starts_with(&format!("Send outcome unknown ({id})")));
        assert_eq!(
            receipts.outcome(USER, &id).unwrap(),
            receipts::Outcome::Pending
        );
        assert_eq!(
            receipts.reserve(&id, &params).unwrap(),
            Some(receipts::Outcome::Pending)
        );
        assert_eq!(
            store.app_setting("receipt-dispatch-effect").unwrap(),
            Some(json!(1))
        );
        // A later completion settles that exact reservation. It never requires
        // another send or another identity, and definite failures stay definite
        // only once their completion was actually stored.
        assert_eq!(
            finish_admitted_send(
                receipts.clone(),
                USER.into(),
                id.clone(),
                dispatched.clone()
            )
            .await,
            dispatched.clone()
        );
        assert_eq!(
            receipts.outcome(USER, &id).unwrap(),
            receipts::Outcome::Finished(dispatched)
        );
        assert_eq!(
            store.app_setting("receipt-dispatch-effect").unwrap(),
            Some(json!(1))
        );
    }
}

async fn accept_client(listener: &mut ipc::Listener, endpoint: &ipc::Endpoint) -> ipc::Connection {
    let mut connection = listener.accept().await.unwrap();
    assert_eq!(
        accept_handshake(&mut connection, &endpoint.profile, Some(USER.into()))
            .await
            .unwrap(),
        ConnectionMode::Client
    );
    connection
}

#[tokio::test]
async fn client_queries_the_original_typed_receipt_after_completion_failure_without_replaying_send()
{
    let root = directory();
    let store = Arc::new(Store::open(StorePaths::new(root.path().join("data"))).unwrap());
    let (identity, params) = seed(&store, root.path());
    let receipts = Arc::new(receipts::Receipts::new(store.clone()));
    let endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
    let mut listener = endpoint.bind().unwrap();
    let (client_connection, mut original) = tokio::join!(
        connect(&endpoint, ConnectionMode::Client, false),
        accept_client(&mut listener, &endpoint),
    );
    let (client_connection, welcome) = client_connection.unwrap();
    let client = RemoteClient::attach(endpoint.clone(), client_connection, welcome);
    let expected_identity = identity.clone();
    let server_store = store.clone();
    let server_receipts = receipts.clone();
    let server = tokio::spawn(async move {
        let request: Request = protocol::read(&mut original).await.unwrap();
        assert_eq!(request.method, "send_message");
        let id = request.send_id.as_deref().unwrap();
        assert_eq!(id, expected_identity);
        assert_eq!(server_receipts.reserve(id, &request.params).unwrap(), None);
        server_store
            .set_app_setting("receipt-dispatch-effect", &json!(1), 2)
            .unwrap();
        let completion = finish_admitted_send(
            unavailable_receipts(&server_store),
            USER.into(),
            id.into(),
            Ok(json!(RUN)),
        )
        .await;
        assert!(
            completion
                .as_ref()
                .unwrap_err()
                .starts_with(&format!("Send outcome unknown ({id})"))
        );
        protocol::write(
            &mut original,
            &Message::Response {
                failure: None,
                id: request.id,
                result: completion,
            },
        )
        .await
        .unwrap();
        let mut recovery = accept_client(&mut listener, &endpoint).await;
        for settled in [false, true] {
            let lookup: Request = protocol::read(&mut recovery).await.unwrap();
            assert_eq!(
                lookup.method, "send_outcome",
                "an uncertain send must never be replayed"
            );
            assert!(lookup.send_id.is_none());
            assert_eq!(lookup.params, json!(expected_identity));
            if settled {
                server_receipts
                    .finish(USER, &expected_identity, &Ok(json!(RUN)))
                    .unwrap();
            }
            let outcome = server_receipts.outcome(USER, &expected_identity).unwrap();
            assert_eq!(
                outcome,
                if settled {
                    receipts::Outcome::Finished(Ok(json!(RUN)))
                } else {
                    receipts::Outcome::Pending
                }
            );
            protocol::write(
                &mut recovery,
                &Message::Response {
                    failure: None,
                    id: lookup.id,
                    result: Ok(serde_json::to_value(outcome).unwrap()),
                },
            )
            .await
            .unwrap();
        }
        let detach: Request = protocol::read(&mut original).await.unwrap();
        assert_eq!(
            detach.method, "detach",
            "the original connection must not replay a send either"
        );
        protocol::write(
            &mut original,
            &Message::Response {
                failure: None,
                id: detach.id,
                result: Ok(Value::Null),
            },
        )
        .await
        .unwrap();
    });
    let run = tokio::time::timeout(
        Duration::from_secs(5),
        client.call_identified::<String>("send_message", params, Some(identity.clone())),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(run, RUN);
    client.detach().await;
    server.await.unwrap();
    assert_eq!(
        receipts.outcome(USER, &identity).unwrap(),
        receipts::Outcome::Finished(Ok(json!(RUN)))
    );
    assert_eq!(
        store.app_setting("receipt-dispatch-effect").unwrap(),
        Some(json!(1))
    );
}
