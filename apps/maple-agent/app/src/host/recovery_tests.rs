use super::*;

fn endpoint() -> (tempfile::TempDir, ipc::Endpoint) {
    #[cfg(unix)]
    let root = tempfile::Builder::new()
        .prefix("maple-recovery-")
        .tempdir_in("/tmp")
        .unwrap();
    #[cfg(windows)]
    let root = tempfile::tempdir().unwrap();
    let endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
    (root, endpoint)
}

async fn accept(
    listener: &mut ipc::Listener,
    endpoint: &ipc::Endpoint,
    user: &str,
) -> ipc::Connection {
    let mut connection = listener.accept().await.unwrap();
    assert_eq!(
        accept_handshake(&mut connection, &endpoint.profile, Some(user.into()))
            .await
            .unwrap(),
        ConnectionMode::Client
    );
    connection
}

async fn client(
    endpoint: &ipc::Endpoint,
    listener: &mut ipc::Listener,
) -> (RemoteClient, ipc::Connection) {
    let (connection, server) = tokio::join!(
        connect(endpoint, ConnectionMode::Client, false),
        accept(listener, endpoint, "first-user")
    );
    let (connection, welcome) = connection.unwrap();
    (
        RemoteClient::attach(endpoint.clone(), connection, welcome),
        server,
    )
}

async fn reconnected(events: &mut mpsc::Receiver<AgentServiceEvent>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if matches!(event, AgentServiceEvent::HostReconnected) {
                return;
            }
            assert!(!matches!(event, AgentServiceEvent::HostDisconnected));
        }
        panic!("event stream ended before reconnection");
    })
    .await
    .unwrap();
}

async fn response(connection: &mut ipc::Connection, expected: &str, value: serde_json::Value) {
    let request: Request = protocol::read(connection).await.unwrap();
    assert_eq!(
        request.method, expected,
        "an old command must never be replayed"
    );
    protocol::write(
        connection,
        &Message::Response {
            failure: None,
            id: request.id,
            result: Ok(value),
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn interrupted_calls_fail_then_reconnect_to_the_current_account_without_replay() {
    let (_root, endpoint) = endpoint();
    let mut listener = endpoint.bind().unwrap();
    let (client, mut server) = client(&endpoint, &mut listener).await;
    let mut events = client.take_events().await.unwrap();
    let call = client.call::<()>("test-mutation", ());
    tokio::pin!(call);
    tokio::select! {
        _ = &mut call => panic!("mutation must await its response"),
        request = protocol::read::<Request>(&mut server) => assert_eq!(request.unwrap().method, "test-mutation"),
    }
    drop(server);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .is_err()
    );
    let mut server = accept(&mut listener, &endpoint, "second-user").await;
    reconnected(&mut events).await;
    assert_eq!(client.user_id().as_deref(), Some("second-user"));
    let (result, ()) = tokio::join!(
        client.call::<String>("inspect", ()),
        response(&mut server, "inspect", serde_json::json!("fresh"))
    );
    assert_eq!(result.unwrap(), "fresh");
    let ((), ()) = tokio::join!(
        client.detach(),
        response(&mut server, "detach", serde_json::Value::Null)
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn calls_waiting_for_capacity_do_not_cross_a_connection_generation() {
    use std::{future::Future, task::Poll};
    let (_root, endpoint) = endpoint();
    let mut listener = endpoint.bind().unwrap();
    let (client, server) = client(&endpoint, &mut listener).await;
    let mut events = client.take_events().await.unwrap();
    let capacity = client.permits.acquire_many(MAX_CALLS as u32).await.unwrap();
    let queued = client.call::<()>("queued-mutation", ());
    tokio::pin!(queued);
    std::future::poll_fn(|cx| {
        assert!(queued.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(server);
    let (mut server, ()) = tokio::join!(
        accept(&mut listener, &endpoint, "first-user"),
        reconnected(&mut events)
    );
    drop(capacity);
    assert!(
        queued
            .await
            .unwrap_err()
            .contains("before this operation was sent")
    );
    let (result, ()) = tokio::join!(
        client.call::<()>("new-command", ()),
        response(&mut server, "new-command", serde_json::Value::Null)
    );
    result.unwrap();
    let ((), ()) = tokio::join!(
        client.detach(),
        response(&mut server, "detach", serde_json::Value::Null)
    );
}

#[tokio::test]
async fn slow_frontend_resyncs_after_a_bounded_event_queue_overflow_without_blocking_replies() {
    let (_root, endpoint) = endpoint();
    let mut listener = endpoint.bind().unwrap();
    let (client, mut server) = client(&endpoint, &mut listener).await;
    let mut events = client.take_events().await.unwrap();
    // Keep the UI queue full. The reader must continue accepting replies and
    // replace missed events with one resync on this same connection.
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..2000 {
            protocol::write(
                &mut server,
                &Message::Event(Box::new(AgentServiceEvent::QuestionResolved {
                    request_id: "fixture".into(),
                })),
            )
            .await
            .unwrap();
        }
    })
    .await
    .unwrap();
    assert!(events.len() <= 256);
    let (result, ()) = tokio::join!(
        client.call::<bool>("inspect", ()),
        response(&mut server, "inspect", serde_json::json!(true))
    );
    assert!(result.unwrap());
    reconnected(&mut events).await;
    let ((), ()) = tokio::join!(
        client.detach(),
        response(&mut server, "detach", serde_json::Value::Null)
    );
}

#[tokio::test]
async fn a_lost_send_response_queries_the_receipt_without_resending() {
    let (_root, endpoint) = endpoint();
    let mut listener = endpoint.bind().unwrap();
    let (client, mut first) = client(&endpoint, &mut listener).await;
    let mut events = client.take_events().await.unwrap();
    let server = tokio::spawn(async move {
        let request: Request = protocol::read(&mut first).await.unwrap();
        assert_eq!(request.method, "send_message");
        let identity = request.send_id.unwrap();
        drop(first);
        // One connection restores the stream, the other queries admission.
        let mut connections = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let mut connection = accept(&mut listener, &endpoint, "first-user").await;
            let identity = identity.clone();
            connections.spawn(async move {
                let request: Request = protocol::read(&mut connection).await.unwrap();
                match request.method.as_str() {
                    "send_outcome" => {
                        assert_eq!(request.params, serde_json::json!(identity));
                        protocol::write(
                            &mut connection,
                            &Message::Response {
                                failure: None,
                                id: request.id,
                                result: Ok(serde_json::to_value(receipts::Outcome::Finished(Ok(
                                    serde_json::json!("accepted-run"),
                                )))
                                .unwrap()),
                            },
                        )
                        .await
                        .unwrap();
                    }
                    "detach" => protocol::write(
                        &mut connection,
                        &Message::Response {
                            failure: None,
                            id: request.id,
                            result: Ok(serde_json::Value::Null),
                        },
                    )
                    .await
                    .unwrap(),
                    other => panic!("unexpected replay: {other}"),
                }
            });
        }
        while let Some(result) = connections.join_next().await {
            result.unwrap();
        }
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.call::<String>("send_message", ()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "accepted-run");
    reconnected(&mut events).await;
    client.detach().await;
    server.await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn concurrent_clients_wait_for_the_next_pipe_instance() {
    use std::{future::Future, task::Poll};
    let (_root, endpoint) = endpoint();
    let mut listener = endpoint.bind().unwrap();
    let _first_client = endpoint.connect().await.unwrap();
    // Keep the first connection unaccepted so the only pipe instance is busy.
    // The next open must wait, rather than reporting that the host is absent.
    let waiting = endpoint.connect();
    tokio::pin!(waiting);
    std::future::poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let _first_server = listener.accept().await.unwrap();
    let (second_client, second_server) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(waiting, listener.accept())
    })
    .await
    .unwrap();
    let mut second_client = second_client.unwrap();
    let mut second_server = second_server.unwrap();
    let (sent, received) = tokio::join!(
        protocol::write(&mut second_client, &"second-client"),
        protocol::read::<String>(&mut second_server)
    );
    sent.unwrap();
    assert_eq!(received.unwrap(), "second-client");
}

#[tokio::test]
async fn unexpected_disconnect_keeps_the_host_alive_but_explicit_stop_still_wins() {
    let (root, endpoint) = endpoint();
    let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
    let receipts = Arc::new(receipts::Receipts::new(backend.store().unwrap().clone()));
    let server_endpoint = endpoint.clone();
    let server =
        tokio::spawn(async move { serve_with_receipts(server_endpoint, backend, receipts).await });
    let (connection, _) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(connection) = connect(&endpoint, ConnectionMode::Client, false).await {
                break connection;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(connection);
    tokio::time::sleep(IDLE_GRACE + Duration::from_millis(100)).await;
    assert!(
        !server.is_finished(),
        "an unexpected loss must not use the normal quit grace"
    );
    let (_stop, _) = connect(&endpoint, ConnectionMode::Stop, false)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn host_loss_exhausts_recovery_without_starting_a_replacement() {
    let (_root, endpoint) = endpoint();
    let mut listener = endpoint.bind().unwrap();
    let (client, server) = client(&endpoint, &mut listener).await;
    let mut events = client.take_events().await.unwrap();
    drop(listener);
    drop(server);
    assert!(matches!(
        events.recv().await,
        Some(AgentServiceEvent::HostReconnecting)
    ));
    assert!(matches!(
        tokio::time::timeout(RECONNECT_TIMEOUT + Duration::from_secs(2), events.recv())
            .await
            .unwrap(),
        Some(AgentServiceEvent::HostDisconnected)
    ));
    assert!(endpoint.connect().await.is_err());
    assert!(client.call::<()>("must-not-run", ()).await.is_err());
}

#[tokio::test]
async fn graceful_detach_keeps_other_clients_running_and_last_quit_shuts_down() {
    let (root, endpoint) = endpoint();
    let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
    let receipts = Arc::new(receipts::Receipts::new(backend.store().unwrap().clone()));
    let server_endpoint = endpoint.clone();
    let server =
        tokio::spawn(async move { serve_with_receipts(server_endpoint, backend, receipts).await });
    let (connection, welcome) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(connection) = connect(&endpoint, ConnectionMode::Client, false).await {
                break connection;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let first = RemoteClient::attach(endpoint.clone(), connection, welcome);
    let (connection, welcome) = connect(&endpoint, ConnectionMode::Client, false)
        .await
        .unwrap();
    let second = RemoteClient::attach(endpoint.clone(), connection, welcome);
    first.detach().await;
    tokio::time::sleep(IDLE_GRACE + Duration::from_millis(100)).await;
    assert!(!server.is_finished());
    second
        .call::<()>("set_harness_instructions", ("fixture",))
        .await
        .unwrap();
    second.detach().await;
    tokio::time::timeout(IDLE_GRACE + Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(endpoint.connect().await.is_err());
}

#[tokio::test]
async fn remote_protocol_refusal_ends_recovery_without_replaying_or_reconnecting() {
    let (_root, endpoint) = endpoint();
    let mut listener = endpoint.bind().unwrap();
    let (client, mut server) = client(&endpoint, &mut listener).await;
    let mut events = client.take_events().await.unwrap();
    protocol::write(
        &mut server,
        &Message::Closed {
            reason: "Update both computers".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap(),
        Some(AgentServiceEvent::RemoteAccessEnded(reason)) if reason == "Update both computers")
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .is_none()
    );
    assert!(client.call::<()>("send_message", ()).await.is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn explicit_stop_survives_a_client_that_leaves_before_the_acknowledgement() {
    let (_root, endpoint) = endpoint();
    let mut listener = endpoint.bind().unwrap();
    let (stop, mut stopped) = mpsc::channel(1);
    let client = async {
        let mut client = endpoint.connect().await.unwrap();
        protocol::write(
            &mut client,
            &Hello {
                version: 0,
                profile: "older-profile".into(),
                build: "older-build".into(),
                mode: ConnectionMode::Stop,
                remote_host: None,
            },
        )
        .await
        .unwrap();
        drop(client);
    };
    let server = async {
        let mut connection = listener.accept().await.unwrap();
        let _ = accept_scoped_handshake(
            &mut connection,
            &endpoint.profile,
            None,
            StoreState::Open,
            None,
            Some(&stop),
        )
        .await;
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(client, server);
        stopped.recv().await.unwrap();
    })
    .await
    .unwrap();
}

#[cfg(feature = "desktop")]
#[tokio::test]
async fn quit_accepts_a_missing_owner_but_not_an_unreachable_live_owner() {
    let (_root, endpoint) = endpoint();
    let owner = endpoint.ownership_lock().unwrap();
    owner.try_lock().unwrap();
    assert!(request_stop_endpoint(&endpoint).await.is_err());
    drop(owner);
    assert!(request_stop_endpoint(&endpoint).await.is_ok());
}

#[cfg(feature = "desktop")]
#[tokio::test]
async fn accepted_stop_waits_for_connection_close_and_authoritative_ownership_release() {
    let (_root, endpoint) = endpoint();
    let mut listener = endpoint.bind().unwrap();
    let owner = endpoint.ownership_lock().unwrap();
    owner.try_lock().unwrap();
    let request_endpoint = endpoint.clone();
    let request = tokio::spawn(async move { request_stop_endpoint(&request_endpoint).await });
    let mut connection = listener.accept().await.unwrap();
    let (stop, mut stopped) = mpsc::channel(1);
    accept_scoped_handshake(
        &mut connection,
        &endpoint.profile,
        None,
        StoreState::Open,
        None,
        Some(&stop),
    )
    .await
    .unwrap();
    stopped.recv().await.unwrap();
    assert!(!request.is_finished());
    drop(connection);
    tokio::task::yield_now().await;
    assert!(!request.is_finished());
    drop(owner);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), request)
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn maintenance_client_gets_its_state_without_waiting_for_open_or_sign_in() {
    let (client, mut server) = tokio::io::duplex(4096);
    let mut client: ipc::Connection = Box::new(client);
    let server = async {
        let hello = protocol::read::<Hello>(&mut server).await.unwrap();
        let mut server: ipc::Connection = Box::new(server);
        accept_hello(
            &mut server,
            hello,
            "profile",
            None,
            StoreState::Maintenance,
            None,
            None,
        )
        .await
        .unwrap()
    };
    let client = async {
        protocol::write(
            &mut client,
            &Hello {
                version: protocol::VERSION,
                profile: "profile".into(),
                build: protocol::BUILD.into(),
                mode: ConnectionMode::Client,
                remote_host: None,
            },
        )
        .await
        .unwrap();
        let welcome = protocol::read::<Result<Welcome, String>>(&mut client)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(welcome.store, StoreState::Maintenance);
        assert!(welcome.user_id.is_none());
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(server, client);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn presented_calls_preserve_legacy_unknown_and_typed_failures_over_ipc() {
    let (_root, endpoint) = endpoint();
    let mut listener = endpoint.bind().unwrap();
    let (client, mut server) = client(&endpoint, &mut listener).await;
    for code in [None, Some("future-code"), Some("store-locked")] {
        let call = client.call_presented::<()>("login", ("synthetic", "synthetic"));
        let respond = async {
            let request: Request = protocol::read(&mut server).await.unwrap();
            let message = Message::Response {
                failure: None,
                id: request.id,
                result: Err("unchanged diagnostic".into()),
            };
            let failure = code.map(|code| HostFailure::new(code, "unchanged diagnostic"));
            write_presented_message(&mut server, &message, failure.as_ref())
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(call, respond);
        let failure = result.unwrap_err();
        assert_eq!(failure.code, code.unwrap_or("unknown"));
        assert_eq!(failure.message, "unchanged diagnostic");
    }
    // Old clients consume the unchanged result field and ignore presentation metadata.
    let failure = HostFailure::new("store-locked", "original fallback");
    let encoded = serde_json::to_value(PresentedMessage::Response {
        id: 1,
        result: &Err("original fallback".into()),
        failure: &failure,
    })
    .unwrap();
    assert_eq!(encoded["Response"]["failure"]["code"], "store-locked");
    #[derive(serde::Deserialize)]
    enum LegacyMessage {
        Response {
            id: u64,
            result: Result<serde_json::Value, String>,
        },
    }
    let LegacyMessage::Response { id, result } = serde_json::from_value(encoded).unwrap();
    assert_eq!(id, 1);
    assert_eq!(result.unwrap_err(), "original fallback");
}
