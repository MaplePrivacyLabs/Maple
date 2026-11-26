//! Task-local image/file snapshots are optional: legacy hosts keep ordinary
//! chats, while a capable host receives the bounded execution-side request.
use super::tests::{command, endpoints};
use super::*;
use crate::remote::update::{
    PeerInfo, ProtocolInfo, RemoteUpdateStatus, RunningApplication, TASK_FILES_CAPABILITY,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn information(supported: bool) -> PeerInfo {
    let mut protocol = ProtocolInfo::current();
    if !supported {
        protocol
            .capabilities
            .retain(|capability| capability != TASK_FILES_CAPABILITY);
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
            failure: None,
            id,
            result: Ok(value),
        },
    )
    .await
    .unwrap();
}

fn import(id: u64) -> Command {
    Command {
        id,
        method: Method::ImportTaskFile,
        send_id: None,
        params: json!(["account", {
            "task": "01234567-89ab-7def-8123-456789abcdef",
            "source": "/workspace/artifacts/preview.gif",
            "kind": "download"
        }]),
    }
}

#[tokio::test]
async fn task_image_import_requires_one_capability_probe_and_preserves_legacy_chats() {
    for supported in [false, true] {
        let (a, b, connection, server) = endpoints().await;
        let client = Client::new(connection);
        let peer = async {
            let (probe, mut tx) = request(&server).await;
            assert_eq!(probe.method, Method::PeerInfo);
            response(
                &mut tx,
                probe.id,
                serde_json::to_value(information(supported)).unwrap(),
            )
            .await;
            if supported {
                for _ in 0..2 {
                    let (call, mut tx) = request(&server).await;
                    assert_eq!(call.method, Method::ImportTaskFile);
                    assert_eq!(call.params[1]["source"], "/workspace/artifacts/preview.gif");
                    response(&mut tx, call.id, json!(null)).await;
                }
            }
            let (ordinary, mut tx) = request(&server).await;
            assert_eq!(ordinary.method, Method::ListTasks);
            response(&mut tx, ordinary.id, json!("chat still works")).await;
        };
        let authorized = AtomicUsize::new(0);
        let viewer = async {
            for (number, id) in [1, 2].into_iter().enumerate() {
                let response = client
                    .call(import(id), async {
                        authorized.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
                    .unwrap();
                if supported {
                    assert!(matches!(
                        response,
                        Message::Response { id: got, result: Ok(_), .. } if got == id
                    ));
                } else {
                    assert!(matches!(
                        response,
                        Message::Response {
                            id: got,
                            result: Err(_),
                            failure: Some(ref failure),
                            ..
                        } if got == id && failure.code == "remote-task-files-unsupported"
                    ));
                    assert_eq!(authorized.load(Ordering::SeqCst), 0);
                }
                assert_eq!(
                    authorized.load(Ordering::SeqCst),
                    if supported { number + 1 } else { 0 }
                );
            }
            let ordinary = client
                .call(command(3, Method::ListTasks), async { Ok(()) })
                .await
                .unwrap();
            assert!(matches!(
                ordinary,
                Message::Response { result: Ok(value), .. } if value == json!("chat still works")
            ));
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(peer, viewer);
        })
        .await
        .unwrap();
        assert_eq!(
            authorized.load(Ordering::SeqCst),
            if supported { 2 } else { 0 },
            "supported={supported}"
        );
        a.close().await;
        b.close().await;
    }
}
