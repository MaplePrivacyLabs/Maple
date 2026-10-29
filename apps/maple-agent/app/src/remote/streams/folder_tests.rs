//! Real local QUIC streams exercise the exact outbound client used by bridge.
//! These are compatibility fixtures, not signed installation or auth smoke.
use super::tests::{command, endpoints};
use super::*;
use crate::remote::update::{PeerInfo, ProtocolInfo, RemoteUpdateStatus, RunningApplication};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn information(folders: bool) -> PeerInfo {
    let mut protocol = ProtocolInfo::current();
    if !folders {
        protocol.version = "6.1.0".into();
        protocol
            .capabilities
            .retain(|cap| cap != crate::remote::update::PROJECT_FOLDERS_CAPABILITY);
    }
    PeerInfo {
        protocol,
        running: RunningApplication::current(),
        update: RemoteUpdateStatus::default(),
    }
}

async fn request(connection: &Connection) -> (Command, SendStream) {
    let (tx, mut rx) = connection.accept_bi().await.unwrap();
    let header = Header::read(&mut rx).await.unwrap();
    (header.body(&mut rx).await.unwrap(), tx)
}

async fn response(tx: &mut SendStream, id: u64, value: serde_json::Value) {
    reply(
        tx,
        &Message::Response {
            id,
            result: Ok(value),
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn released_management_peer_only_blocks_folders_and_keeps_update_status_fresh() {
    let (a, b, connection, server) = endpoints().await;
    let client = Client::new(connection);
    let peer = async {
        let (probe, mut tx) = request(&server).await;
        assert_eq!(probe.method, Method::PeerInfo);
        response(
            &mut tx,
            probe.id,
            serde_json::to_value(information(false)).unwrap(),
        )
        .await;
        // Neither folder method may reach an older host. Its ordinary and
        // management calls still dispatch, including a fresh second PeerInfo.
        for method in [
            Method::ListTasks,
            Method::CancelRun,
            Method::AnswerQuestion,
            Method::PeerInfo,
        ] {
            let (call, mut tx) = request(&server).await;
            assert_eq!(call.method, method);
            response(&mut tx, call.id, json!(method.name())).await;
        }
    };
    let viewer = async {
        for method in [Method::BrowseProjectFolders, Method::CreateProjectFolder] {
            assert_eq!(
                client
                    .call(command(1, method), async {
                        panic!("unsupported must not authorize")
                    })
                    .await
                    .err()
                    .expect("request must be refused"),
                FOLDERS_UNSUPPORTED
            );
        }
        for (id, method) in [
            Method::ListTasks,
            Method::CancelRun,
            Method::AnswerQuestion,
            Method::PeerInfo,
        ]
        .into_iter()
        .enumerate()
        {
            let message = client
                .call(command(id as u64 + 2, method), async { Ok(()) })
                .await
                .unwrap();
            assert!(
                matches!(message, Message::Response { result: Ok(value), .. } if value == json!(method.name()))
            );
        }
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(peer, viewer);
    })
    .await
    .unwrap();
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn concurrent_folders_share_discovery_while_stop_and_events_continue() {
    let (a, b, connection, server) = endpoints().await;
    let client = Arc::new(Client::new(connection.clone()));
    let (seen_probe, probe_seen) = tokio::sync::oneshot::channel();
    let (release_probe, released) = tokio::sync::oneshot::channel();
    let peer = async {
        let (probe, mut tx) = request(&server).await;
        assert_eq!(probe.method, Method::PeerInfo);
        seen_probe.send(()).unwrap();
        let (stop, mut stop_tx) = request(&server).await;
        assert_eq!(stop.method, Method::CancelRun);
        response(&mut stop_tx, stop.id, json!("stopped")).await;
        let (mut events, _) = server.open_bi().await.unwrap();
        reply(&mut events, &Message::Resync).await.unwrap();
        released.await.unwrap();
        response(
            &mut tx,
            probe.id,
            serde_json::to_value(information(true)).unwrap(),
        )
        .await;
        let mut methods = Vec::new();
        for _ in 0..2 {
            let (folder, mut tx) = request(&server).await;
            methods.push(folder.method);
            response(&mut tx, folder.id, json!("folder")).await;
        }
        assert!(methods.contains(&Method::BrowseProjectFolders));
        assert!(methods.contains(&Method::CreateProjectFolder));
    };
    let viewer = async {
        let c = client.clone();
        let browse = tokio::spawn(async move {
            c.call(command(1, Method::BrowseProjectFolders), async { Ok(()) })
                .await
        });
        probe_seen.await.unwrap();
        let c = client.clone();
        let create = tokio::spawn(async move {
            c.call(command(2, Method::CreateProjectFolder), async { Ok(()) })
                .await
        });
        let stop = client
            .call(command(3, Method::CancelRun), async { Ok(()) })
            .await
            .unwrap();
        assert!(
            matches!(stop, Message::Response { result: Ok(value), .. } if value == json!("stopped"))
        );
        let (_, mut events) = connection.accept_bi().await.unwrap();
        assert!(matches!(
            wire::read::<Message>(&mut events, wire::MAX_FRAME)
                .await
                .unwrap(),
            Message::Resync
        ));
        release_probe.send(()).unwrap();
        for completed in [browse.await.unwrap(), create.await.unwrap()] {
            assert!(
                matches!(completed.unwrap(), Message::Response { result: Ok(value), .. } if value == json!("folder"))
            );
        }
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(peer, viewer);
    })
    .await
    .unwrap();
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn failed_discovery_is_retryable_and_emits_no_folder_request() {
    for stalled in [false, true] {
        let (a, b, connection, server) = endpoints().await;
        let mut client = Client::new(connection);
        client.discovery_timeout = Duration::from_millis(200);
        let (release, released) = tokio::sync::oneshot::channel();
        let peer = async {
            let (probe, mut tx) = request(&server).await;
            assert_eq!(probe.method, Method::PeerInfo);
            if !stalled {
                response(&mut tx, probe.id, json!({"malformed":true})).await;
            }
            released.await.unwrap();
            drop(tx);
            let (probe, mut tx) = request(&server).await;
            assert_eq!(probe.method, Method::PeerInfo);
            response(
                &mut tx,
                probe.id,
                serde_json::to_value(information(true)).unwrap(),
            )
            .await;
            let (folder, mut tx) = request(&server).await;
            assert_eq!(folder.method, Method::CreateProjectFolder);
            response(&mut tx, folder.id, json!("created once")).await;
        };
        let viewer = async {
            assert_eq!(
                client
                    .call(command(1, Method::CreateProjectFolder), async {
                        panic!("failed probe must not authorize")
                    })
                    .await
                    .err()
                    .expect("request must be refused"),
                FOLDERS_UNCONFIRMED
            );
            release.send(()).unwrap();
            // Give the successful retry the normal production deadline.
            client.discovery_timeout = DISCOVERY_TIMEOUT;
            assert!(
                matches!(client.call(command(2, Method::CreateProjectFolder), async { Ok(()) }).await.unwrap(), Message::Response { result: Ok(value), .. } if value == json!("created once"))
            );
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(peer, viewer);
        })
        .await
        .unwrap();
        a.close().await;
        b.close().await;
    }
}

#[tokio::test]
async fn waiting_for_discovery_is_bounded_without_caching_a_failure() {
    let (a, b, connection, _) = endpoints().await;
    let mut client = Client::new(connection);
    client.discovery_timeout = Duration::from_millis(50);
    let (entered, entry) = tokio::sync::oneshot::channel();
    let held = async {
        let _: Result<&bool, ()> = client
            .project_folders
            .get_or_try_init(|| async {
                entered.send(()).unwrap();
                std::future::pending().await
            })
            .await;
    };
    let waiter = async {
        entry.await.unwrap();
        assert_eq!(
            client
                .call(command(1, Method::BrowseProjectFolders), async { Ok(()) })
                .await
                .err()
                .expect("request must be refused"),
            FOLDERS_UNCONFIRMED
        );
        assert!(client.project_folders.get().is_none());
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! { _ = held => panic!("initializer should still wait"), _ = waiter => {} }
    })
    .await
    .unwrap();
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn positive_capability_never_replaces_current_authority() {
    let (a, b, connection, server) = endpoints().await;
    let client = Client::new(connection);
    let authority = AtomicUsize::new(0);
    let peer = async {
        let (probe, mut tx) = request(&server).await;
        assert_eq!(probe.method, Method::PeerInfo);
        // Sign-out/revocation happens while discovery is in flight.
        authority.store(1, Ordering::SeqCst);
        response(
            &mut tx,
            probe.id,
            serde_json::to_value(information(true)).unwrap(),
        )
        .await;
        // A marker call proves no folder request reached the host either from
        // the first positive probe or from the subsequently cached support.
        let (next, mut tx) = request(&server).await;
        assert_eq!(next.method, Method::ListTasks);
        response(&mut tx, next.id, json!("marker")).await;
    };
    let viewer = async {
        for reason in ["account changed", "grant revoked"] {
            let authorization = async {
                assert_eq!(authority.load(Ordering::SeqCst), 1);
                Err(reason.into())
            };
            assert_eq!(
                client
                    .call(command(1, Method::CreateProjectFolder), authorization)
                    .await
                    .err()
                    .expect("request must be refused"),
                reason
            );
            assert_eq!(client.project_folders.get(), Some(&true));
        }
        client
            .call(command(2, Method::ListTasks), async { Ok(()) })
            .await
            .unwrap();
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(peer, viewer);
    })
    .await
    .unwrap();
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn reconnect_discovers_support_again_in_both_directions() {
    for supported in [true, false, true] {
        let (a, b, connection, server) = endpoints().await;
        let client = Client::new(connection);
        assert!(client.project_folders.get().is_none());
        let peer = async {
            let (probe, mut tx) = request(&server).await;
            assert_eq!(probe.method, Method::PeerInfo);
            response(
                &mut tx,
                probe.id,
                serde_json::to_value(information(supported)).unwrap(),
            )
            .await;
            let (next, mut tx) = request(&server).await;
            assert_eq!(
                next.method,
                if supported {
                    Method::BrowseProjectFolders
                } else {
                    Method::ListTasks
                }
            );
            response(&mut tx, next.id, json!("ok")).await;
        };
        let viewer = async {
            let result = client
                .call(command(1, Method::BrowseProjectFolders), async { Ok(()) })
                .await;
            if supported {
                assert!(result.is_ok());
            } else {
                assert_eq!(
                    result.err().expect("request must be refused"),
                    FOLDERS_UNSUPPORTED
                );
                client
                    .call(command(2, Method::ListTasks), async { Ok(()) })
                    .await
                    .unwrap();
            }
            assert_eq!(client.project_folders.get(), Some(&supported));
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(peer, viewer);
        })
        .await
        .unwrap();
        a.close().await;
        b.close().await;
    }
}

#[tokio::test]
async fn scratch_creation_requires_its_own_capability_and_fresh_authorization() {
    for supported in [false, true] {
        let (a, b, connection, server) = endpoints().await;
        let client = Client::new(connection);
        let peer = async {
            let (probe, mut tx) = request(&server).await;
            assert_eq!(probe.method, Method::PeerInfo);
            let mut info = information(true);
            if !supported {
                info.protocol
                    .capabilities
                    .retain(|cap| cap != crate::remote::update::SCRATCH_PROJECT_CAPABILITY);
            }
            response(&mut tx, probe.id, serde_json::to_value(info).unwrap()).await;
            // An unsupported or revoked request cannot reach the execution host.
            let (call, mut tx) = request(&server).await;
            assert_eq!(
                call.method,
                if supported {
                    Method::CreateScratchProject
                } else {
                    Method::ListTasks
                }
            );
            response(&mut tx, call.id, json!("accepted")).await;
        };
        let viewer = async {
            let error = client
                .call(command(1, Method::CreateScratchProject), async {
                    assert!(supported, "unsupported peers must not authorize");
                    Err("account changed".into())
                })
                .await
                .err()
                .unwrap();
            assert!(error.contains(if supported {
                "account changed"
            } else {
                "Update Maple"
            }));
            let method = if supported {
                Method::CreateScratchProject
            } else {
                Method::ListTasks
            };
            assert!(
                matches!(client.call(command(2, method), async { Ok(()) }).await.unwrap(),
                Message::Response { result: Ok(value), .. } if value == json!("accepted"))
            );
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(peer, viewer);
        })
        .await
        .unwrap();
        a.close().await;
        b.close().await;
    }
}
