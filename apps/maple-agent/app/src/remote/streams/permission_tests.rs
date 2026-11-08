//! A paired viewer changes a task's permission mode only on a host that
//! advertises it. Real local QUIC streams drive the bridge's outbound client.
use super::tests::endpoints;
use super::*;
use crate::remote::update::{
    PERMISSION_MODES_CAPABILITY, PeerInfo, ProtocolInfo, RemoteUpdateStatus, RunningApplication,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn information(modes: bool) -> PeerInfo {
    let mut protocol = ProtocolInfo::current();
    if !modes {
        // Agent 0.0.19 has the modes but refuses the setter.
        protocol
            .capabilities
            .retain(|cap| cap != PERMISSION_MODES_CAPABILITY);
    }
    PeerInfo {
        protocol,
        running: RunningApplication::current(),
        update: RemoteUpdateStatus::default(),
    }
}

fn set_mode(id: u64) -> Command {
    Command {
        id,
        method: Method::SetSessionPermissionMode,
        send_id: None,
        params: json!(["account", "task", "readOnly"]),
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

/// The refusal a viewer gets instead of a stream the host would drop.
fn refusal_code(message: Message) -> String {
    match message {
        Message::Response {
            result: Err(_),
            failure: Some(failure),
            ..
        } => failure.code,
        _ => panic!("expected a coded refusal"),
    }
}

#[tokio::test]
async fn an_older_host_never_receives_a_mode_change_and_keeps_its_other_requests() {
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
        // No mode change and no second probe: support is known for this
        // connection. Its other task calls and question answers still go.
        for method in [Method::SetSessionWebEnabled, Method::AnswerQuestion] {
            let (call, mut tx) = request(&server).await;
            assert_eq!(call.method, method);
            response(&mut tx, call.id, json!(method.name())).await;
        }
    };
    let viewer = async {
        for id in [1, 2] {
            let refused = client
                .call(set_mode(id), async {
                    panic!("an unsupported change must not authorize")
                })
                .await
                .unwrap();
            assert!(matches!(&refused, Message::Response { id: got, .. } if *got == id));
            assert_eq!(refusal_code(refused), "remote-permissions-unsupported");
        }
        for (id, method) in [
            (3, Method::SetSessionWebEnabled),
            (4, Method::AnswerQuestion),
        ] {
            let reply = client
                .call(super::tests::command(id, method), async { Ok(()) })
                .await
                .unwrap();
            assert!(
                matches!(reply, Message::Response { result: Ok(value), .. } if value == json!(method.name()))
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
async fn a_current_host_receives_each_mode_change_after_one_probe() {
    let (a, b, connection, server) = endpoints().await;
    let client = Client::new(connection);
    let authorized = AtomicUsize::new(0);
    let peer = async {
        let (probe, mut tx) = request(&server).await;
        assert_eq!(probe.method, Method::PeerInfo);
        response(
            &mut tx,
            probe.id,
            serde_json::to_value(information(true)).unwrap(),
        )
        .await;
        for _ in 0..2 {
            let (call, mut tx) = request(&server).await;
            assert_eq!(call.method, Method::SetSessionPermissionMode);
            assert_eq!(call.params, json!(["account", "task", "readOnly"]));
            response(&mut tx, call.id, json!({"permissionMode": "readOnly"})).await;
        }
    };
    let viewer = async {
        for id in [1, 2] {
            let reply = client
                .call(set_mode(id), async {
                    authorized.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap();
            assert!(matches!(reply, Message::Response { result: Ok(_), .. }));
        }
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(peer, viewer);
    })
    .await
    .unwrap();
    // Cached support is never authority: account and grant are rechecked.
    assert_eq!(authorized.load(Ordering::SeqCst), 2);
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn a_failed_probe_refuses_as_unconfirmed_and_the_next_change_asks_again() {
    let (a, b, connection, server) = endpoints().await;
    let client = Client::new(connection);
    let peer = async {
        let (probe, mut tx) = request(&server).await;
        assert_eq!(probe.method, Method::PeerInfo);
        response(&mut tx, probe.id, json!({"malformed": true})).await;
        let (probe, mut tx) = request(&server).await;
        assert_eq!(probe.method, Method::PeerInfo);
        response(
            &mut tx,
            probe.id,
            serde_json::to_value(information(true)).unwrap(),
        )
        .await;
        let (call, mut tx) = request(&server).await;
        assert_eq!(call.method, Method::SetSessionPermissionMode);
        response(&mut tx, call.id, json!({"permissionMode": "readOnly"})).await;
    };
    let viewer = async {
        let refused = client
            .call(set_mode(1), async {
                panic!("an unconfirmed change must not authorize")
            })
            .await
            .unwrap();
        assert_eq!(refusal_code(refused), "remote-permissions-unconfirmed");
        let reply = client.call(set_mode(2), async { Ok(()) }).await.unwrap();
        assert!(matches!(reply, Message::Response { result: Ok(_), .. }));
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(peer, viewer);
    })
    .await
    .unwrap();
    a.close().await;
    b.close().await;
}
