//! A paired viewer lists and switches a host's tools only on a host that
//! advertises it. Real local QUIC streams drive the bridge's outbound client.
use super::tests::endpoints;
use super::*;
use crate::remote::update::{
    PeerInfo, ProtocolInfo, RemoteUpdateStatus, RunningApplication, TOOL_SWITCHES_CAPABILITY,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The calls of Settings → Tools.
const TOOL_CALLS: [Method; 2] = [Method::ListTools, Method::SetToolEnabled];

fn information(tools: bool) -> PeerInfo {
    let mut protocol = ProtocolInfo::current();
    if !tools {
        // Agent 0.0.21 changes permission modes but has no Tools page.
        protocol
            .capabilities
            .retain(|cap| cap != TOOL_SWITCHES_CAPABILITY);
    }
    PeerInfo {
        protocol,
        running: RunningApplication::current(),
        update: RemoteUpdateStatus::default(),
    }
}

/// What the Tools page sends: list the switches, or turn `grep` on.
fn tool_call(id: u64, method: Method) -> Command {
    let params = match method {
        Method::ListTools => json!(["account"]),
        Method::SetToolEnabled => json!(["account", "grep", true]),
        _ => unreachable!("not a Tools call"),
    };
    Command {
        id,
        method,
        send_id: None,
        params,
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
async fn an_older_host_never_receives_a_tool_call_and_keeps_its_other_requests() {
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
        // No tool call and no second probe for one: support is known for
        // this connection. Mode changes, which this host offers, ask once
        // for themselves; its integrations calls need no capability.
        let (probe, mut tx) = request(&server).await;
        assert_eq!(probe.method, Method::PeerInfo);
        response(
            &mut tx,
            probe.id,
            serde_json::to_value(information(false)).unwrap(),
        )
        .await;
        for method in [
            Method::SetSessionPermissionMode,
            Method::ListIntegrations,
            Method::SetIntegrationEnabled,
        ] {
            let (call, mut tx) = request(&server).await;
            assert_eq!(call.method, method);
            response(&mut tx, call.id, json!(method.name())).await;
        }
    };
    let viewer = async {
        for (id, method) in TOOL_CALLS.into_iter().enumerate() {
            let id = id as u64 + 1;
            let refused = client
                .call(tool_call(id, method), async {
                    panic!("an unsupported call must not authorize")
                })
                .await
                .unwrap();
            assert!(matches!(&refused, Message::Response { id: got, .. } if *got == id));
            assert_eq!(refusal_code(refused), "remote-tools-unsupported");
        }
        for (id, method) in [
            (3, Method::SetSessionPermissionMode),
            (4, Method::ListIntegrations),
            (5, Method::SetIntegrationEnabled),
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
async fn a_current_host_receives_both_tool_calls_after_one_probe() {
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
        for method in TOOL_CALLS {
            let (call, mut tx) = request(&server).await;
            assert_eq!(call.method, method);
            assert_eq!(call.params, tool_call(call.id, method).params);
            response(&mut tx, call.id, json!(method.name())).await;
        }
    };
    let viewer = async {
        for (id, method) in TOOL_CALLS.into_iter().enumerate() {
            let reply = client
                .call(tool_call(id as u64 + 1, method), async {
                    authorized.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
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
    // Cached support is never authority: account and grant are rechecked.
    assert_eq!(authorized.load(Ordering::SeqCst), 2);
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn a_failed_probe_refuses_as_unconfirmed_and_the_next_call_asks_again() {
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
        assert_eq!(call.method, Method::SetToolEnabled);
        response(&mut tx, call.id, json!("switched")).await;
    };
    let viewer = async {
        let refused = client
            .call(tool_call(1, Method::ListTools), async {
                panic!("an unconfirmed call must not authorize")
            })
            .await
            .unwrap();
        assert_eq!(refusal_code(refused), "remote-tools-unconfirmed");
        assert!(client.tool_switches.get().is_none());
        let reply = client
            .call(tool_call(2, Method::SetToolEnabled), async { Ok(()) })
            .await
            .unwrap();
        assert!(
            matches!(reply, Message::Response { result: Ok(value), .. } if value == json!("switched"))
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

#[tokio::test]
async fn known_support_never_replaces_the_current_account_and_grant() {
    let (a, b, connection, server) = endpoints().await;
    let client = Client::new(connection);
    let authority = AtomicUsize::new(0);
    let peer = async {
        let (probe, mut tx) = request(&server).await;
        assert_eq!(probe.method, Method::PeerInfo);
        // Sign-out or revocation lands while discovery is in flight.
        authority.store(1, Ordering::SeqCst);
        response(
            &mut tx,
            probe.id,
            serde_json::to_value(information(true)).unwrap(),
        )
        .await;
        // The next call to arrive is the marker: no tool call got through,
        // either after the probe or from the support it cached.
        let (next, mut tx) = request(&server).await;
        assert_eq!(next.method, Method::ListTasks);
        response(&mut tx, next.id, json!("marker")).await;
    };
    let viewer = async {
        for (id, (method, reason)) in TOOL_CALLS
            .into_iter()
            .zip(["account changed", "grant revoked"])
            .enumerate()
        {
            let authorization = async {
                assert_eq!(authority.load(Ordering::SeqCst), 1);
                Err(reason.into())
            };
            assert_eq!(
                client
                    .call(tool_call(id as u64 + 1, method), authorization)
                    .await
                    .err()
                    .expect("the call must be refused"),
                reason
            );
            assert_eq!(client.tool_switches.get(), Some(&true));
        }
        client
            .call(super::tests::command(3, Method::ListTasks), async {
                Ok(())
            })
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
