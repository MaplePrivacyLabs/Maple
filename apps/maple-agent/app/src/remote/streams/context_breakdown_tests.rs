//! A paired viewer reads what fills a task's context window only from a
//! host that advertises it; any other host's answer is a coded refusal the
//! composer reads as no breakdown. Real local QUIC streams drive the
//! bridge's outbound client.
use super::tests::endpoints;
use super::*;
use crate::remote::update::{
    CONTEXT_BREAKDOWN_CAPABILITY, PeerInfo, ProtocolInfo, RemoteUpdateStatus, RunningApplication,
};
use maple_agent::agent::AgentContextBreakdown;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn information(breakdown: bool) -> PeerInfo {
    let mut protocol = ProtocolInfo::current();
    if !breakdown {
        // Agent 0.0.25 switches tools but cannot break a context down.
        protocol
            .capabilities
            .retain(|cap| cap != CONTEXT_BREAKDOWN_CAPABILITY);
    }
    PeerInfo {
        protocol,
        running: RunningApplication::current(),
        update: RemoteUpdateStatus::default(),
    }
}

/// What the composer's context card asks for a task.
fn breakdown(id: u64) -> Command {
    Command {
        id,
        method: Method::ContextBreakdown,
        send_id: None,
        params: json!(["account", "task"]),
    }
}

/// What a current host answers for a loaded task.
fn answer() -> serde_json::Value {
    serde_json::to_value(Some(AgentContextBreakdown {
        instructions: 4_000,
        messages: 1_200,
        tool_results: 800,
        compact_at: Some(183_616),
        ..AgentContextBreakdown::default()
    }))
    .unwrap()
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
async fn an_older_host_never_receives_a_breakdown_and_keeps_its_meter() {
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
        // No breakdown and no second probe: support is known for this
        // connection. The context meter's own call needs no capability.
        let (call, mut tx) = request(&server).await;
        assert_eq!(call.method, Method::ContextUsage);
        response(&mut tx, call.id, json!([12_000, 200_000])).await;
    };
    let viewer = async {
        for id in [1, 2] {
            let refused = client
                .call(breakdown(id), async {
                    panic!("an unsupported call must not authorize")
                })
                .await
                .unwrap();
            assert!(matches!(&refused, Message::Response { id: got, .. } if *got == id));
            assert_eq!(
                refusal_code(refused),
                "remote-context-breakdown-unsupported"
            );
        }
        let reply = client
            .call(super::tests::command(3, Method::ContextUsage), async {
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            matches!(reply, Message::Response { result: Ok(value), .. } if value == json!([12_000, 200_000]))
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
async fn a_current_host_answers_each_refresh_after_one_probe() {
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
            assert_eq!(call.method, Method::ContextBreakdown);
            assert_eq!(call.params, json!(["account", "task"]));
            response(&mut tx, call.id, answer()).await;
        }
    };
    let viewer = async {
        for id in [1, 2] {
            let reply = client
                .call(breakdown(id), async {
                    authorized.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap();
            let Message::Response {
                result: Ok(value), ..
            } = reply
            else {
                panic!("expected the host's breakdown");
            };
            let read: Option<AgentContextBreakdown> = serde_json::from_value(value).unwrap();
            assert_eq!(read.and_then(|read| read.compact_at), Some(183_616));
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
async fn a_failed_probe_refuses_as_unconfirmed_and_the_next_refresh_asks_again() {
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
        assert_eq!(call.method, Method::ContextBreakdown);
        response(&mut tx, call.id, answer()).await;
    };
    let viewer = async {
        let refused = client
            .call(breakdown(1), async {
                panic!("an unconfirmed call must not authorize")
            })
            .await
            .unwrap();
        assert_eq!(
            refusal_code(refused),
            "remote-context-breakdown-unconfirmed"
        );
        assert!(client.context_breakdown.get().is_none());
        let reply = client.call(breakdown(2), async { Ok(()) }).await.unwrap();
        assert!(matches!(reply, Message::Response { result: Ok(value), .. } if value == answer()));
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
        // The next call to arrive is the marker: no breakdown request got
        // through, either after the probe or from the support it cached.
        let (next, mut tx) = request(&server).await;
        assert_eq!(next.method, Method::ListTasks);
        response(&mut tx, next.id, json!("marker")).await;
    };
    let viewer = async {
        for (id, reason) in [(1, "account changed"), (2, "grant revoked")] {
            let authorization = async {
                assert_eq!(authority.load(Ordering::SeqCst), 1);
                Err(reason.into())
            };
            assert_eq!(
                client
                    .call(breakdown(id), authorization)
                    .await
                    .err()
                    .expect("the call must be refused"),
                reason
            );
            assert_eq!(client.context_breakdown.get(), Some(&true));
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
