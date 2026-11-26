//! A paired viewer downloads a task's file only from a host that advertises
//! it. Real local QUIC streams drive the bridge's outbound client.
use super::tests::endpoints;
use super::*;
use crate::remote::update::{
    PeerInfo, ProtocolInfo, RemoteUpdateStatus, RunningApplication, TASK_FILES_CAPABILITY,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn information(task_files: bool) -> PeerInfo {
    let mut protocol = ProtocolInfo::current();
    if !task_files {
        // Agent 0.0.25 has everything else this viewer may call.
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

/// The first read of a download, as the chat sends it.
fn read_call(id: u64) -> Command {
    Command {
        id,
        method: Method::ReadTaskFile,
        send_id: None,
        params: json!([
            "account",
            {
                "task": "01234567-89ab-7def-8123-456789abcdef",
                "link": "out/report.pdf",
                "offset": 0,
                "len": 1048576,
                "version": null
            }
        ]),
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

#[tokio::test]
async fn an_older_host_never_receives_a_file_read_and_keeps_its_other_requests() {
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
        // Neither read reaches this host, nor a second probe; attachment
        // reads need no capability.
        let (call, mut tx) = request(&server).await;
        assert_eq!(call.method, Method::ReadAttachment);
        response(&mut tx, call.id, json!("attachment")).await;
    };
    let viewer = async {
        for id in [1, 2] {
            let refused = client
                .call(read_call(id), async {
                    panic!("an unsupported call must not authorize")
                })
                .await
                .unwrap();
            match refused {
                Message::Response {
                    id: got,
                    result: Err(_),
                    failure: Some(failure),
                } => {
                    assert_eq!(got, id);
                    assert_eq!(failure.code, "remote-task-files-unsupported");
                }
                _ => panic!("expected a coded refusal"),
            }
        }
        let reply = client
            .call(super::tests::command(3, Method::ReadAttachment), async {
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            matches!(reply, Message::Response { result: Ok(value), .. } if value == json!("attachment"))
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
async fn a_current_host_receives_file_reads_after_one_probe() {
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
            assert_eq!(call.method, Method::ReadTaskFile);
            assert_eq!(call.params, read_call(call.id).params);
            response(&mut tx, call.id, json!("piece")).await;
        }
    };
    let viewer = async {
        for id in [1, 2] {
            let reply = client
                .call(read_call(id), async {
                    authorized.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap();
            assert!(
                matches!(reply, Message::Response { result: Ok(value), .. } if value == json!("piece"))
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

#[test]
fn file_reads_share_the_range_lane_and_only_reads_cross_the_wire() {
    assert_eq!(Method::parse("read_task_file"), Ok(Method::ReadTaskFile));
    // Opening in place is for the execution computer's own desktop.
    assert!(Method::parse("locate_task_file").is_err());
    assert_eq!(
        Method::ReadTaskFile.reply_limit(),
        Method::ReadAttachment.reply_limit()
    );
    assert!(!Method::ReadTaskFile.urgent());
}
