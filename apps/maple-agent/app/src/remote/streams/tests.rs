use super::*;
use iroh::{Endpoint, RelayMode, endpoint::presets};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(in crate::remote) async fn endpoints() -> (Endpoint, Endpoint, Connection, Connection) {
    async fn endpoint() -> Endpoint {
        Endpoint::builder(presets::Minimal)
            .alpns(vec![wire::ALPN.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .clear_address_lookup()
            .clear_ip_transports()
            .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .transport_config(transport_config())
            .bind()
            .await
            .unwrap()
    }
    let a = endpoint().await;
    let b = endpoint().await;
    let (client, server) = tokio::join!(
        async { a.connect(b.addr(), wire::ALPN).await.unwrap() },
        async { b.accept().await.unwrap().await.unwrap() },
    );
    (a, b, client, server)
}

pub(in crate::remote) fn command(id: u64, method: Method) -> Command {
    Command {
        id,
        method,
        send_id: None,
        params: json!(["account", "task"]),
    }
}

async fn receive(
    connection: &Connection,
    capacity: &Capacity,
) -> (Header, SendStream, RecvStream, OwnedSemaphorePermit) {
    let (tx, mut rx) = connection.accept_bi().await.unwrap();
    let header = Header::read(&mut rx).await.unwrap();
    let slot = capacity.acquire(header.method).unwrap();
    (header, tx, rx, slot)
}

#[tokio::test]
async fn unsupported_optional_probe_does_not_lose_ordinary_requests() {
    // This decoder is intentionally frozen to the old method set. It must not
    // acquire the new enum variants from the implementation under test.
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum LegacyMethod {
        ListTasks,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyHeader {
        id: u64,
        method: LegacyMethod,
        send_id: Option<String>,
    }
    let (a, b, client, server) = endpoints().await;
    let legacy = tokio::spawn(async move {
        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let header = wire::read::<LegacyHeader>(&mut rx, wire::MAX_PREFACE).await;
        assert!(matches!(header, Err(wire::Error::InvalidFrame)));
        tx.reset(1u32.into()).unwrap();
        rx.stop(1u32.into()).unwrap();
        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let header: LegacyHeader = wire::read(&mut rx, wire::MAX_PREFACE).await.unwrap();
        assert!(matches!(header.method, LegacyMethod::ListTasks));
        assert!(header.send_id.is_none());
        let _: serde_json::Value = wire::read(&mut rx, wire::MAX_FRAME).await.unwrap();
        reply(
            &mut tx,
            &Message::Response {
                id: header.id,
                result: Ok(json!("ordinary task request still works")),
            },
        )
        .await
        .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        let client = Client::new(client);
        assert_eq!(client.call(command(1, Method::BrowseProjectFolders), async { Ok(()) }).await.err().expect("legacy peer must not receive folder requests"), FOLDERS_UNCONFIRMED);
        let response = client.call(command(2, Method::ListTasks), async { Ok(()) }).await.unwrap();
        assert!(matches!(response, Message::Response { result: Ok(value), .. } if value == json!("ordinary task request still works")));
        legacy.await.unwrap();
    }).await.unwrap();
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn optional_probe_has_an_overall_deadline_even_before_first_reply_byte() {
    let (a, b, client, server) = endpoints().await;
    let exercise = async {
        let probe = peer_info_with_timeout(&client, Duration::from_millis(50));
        let stall = async {
            let (tx, mut rx) = server.accept_bi().await.unwrap();
            let header = Header::read(&mut rx).await.unwrap();
            assert_eq!(header.method, Method::PeerInfo);
            header.body(&mut rx).await.unwrap();
            // Keep the stream open and provide no first reply byte.
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(tx);
        };
        let (response, ()) = tokio::join!(probe, stall);
        assert!(response.is_none());
    };
    tokio::time::timeout(Duration::from_secs(3), exercise)
        .await
        .unwrap();
    a.close().await;
    b.close().await;
}

#[test]
fn management_messages_have_small_independent_limits() {
    for method in [Method::PeerInfo, Method::RemoteUpdateInstall] {
        assert_eq!(body_limit(method), 4096);
        assert_eq!(method.reply_limit(), 16 * 1024);
    }
    assert_eq!(body_limit(Method::SendMessage), wire::MAX_FRAME);
}

#[tokio::test]
async fn failed_reply_delivery_never_reaches_the_install_acknowledgement() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (a, b, client, server) = endpoints().await;
    let acknowledged = Arc::new(AtomicBool::new(false));
    let effect = acknowledged.clone();
    let exercise = async {
        let (mut request, mut response) = client.open_bi().await.unwrap();
        wire::write(&mut request, &json!({}), wire::MAX_PREFACE)
            .await
            .unwrap();
        request.finish().unwrap();
        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let _: serde_json::Value = wire::read(&mut rx, wire::MAX_PREFACE).await.unwrap();
        let reply_task = tokio::spawn(async move {
            // Backpressure makes rejection deterministic: the receiver cannot
            // acknowledge this complete response before rejecting the stream.
            reply_with_limit(
                &mut tx,
                &Message::Response {
                    id: 1,
                    result: Ok(json!("x".repeat(2 * 1024 * 1024))),
                },
                wire::MAX_FRAME,
            )
            .await?;
            effect.store(true, Ordering::SeqCst);
            Ok::<_, String>(())
        });
        assert!(response.read_u32().await.unwrap() > 512 * 1024);
        response.stop(17u32.into()).unwrap();
        assert!(reply_task.await.unwrap().is_err());
        assert!(!acknowledged.load(Ordering::SeqCst));
    };
    tokio::time::timeout(Duration::from_secs(3), exercise)
        .await
        .unwrap();
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn actual_install_call_consumes_fin_before_exactly_one_acknowledgement() {
    use crate::remote::update::{InstallAccepted, InstallCommand, InstallRequest, ReleaseIdentity};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (a, b, client, server) = endpoints().await;
    let acknowledgements = Arc::new(AtomicUsize::new(0));
    let acknowledged = acknowledgements.clone();
    let install = InstallRequest {
        operation_id: "11111111-1111-4111-8111-111111111111".into(),
        target: ReleaseIdentity {
            version: "0.0.7".into(),
            build: "551".into(),
            source_sha: "a".repeat(40),
            manifest_sha256: "b".repeat(64),
        },
    };
    let accepted = InstallAccepted {
        operation_id: install.operation_id.clone(),
        target: install.target.clone(),
    };
    let exercise = async {
        let server_task = tokio::spawn(async move {
            let capacity = Capacity::new();
            let (header, mut tx, mut rx, _slot) = receive(&server, &capacity).await;
            assert_eq!(header.method, Method::RemoteUpdateInstall);
            let request = header.body(&mut rx).await.unwrap();
            let command: InstallCommand = serde_json::from_value(request.params).unwrap();
            command.request.validate().unwrap();
            reply_with_limit(
                &mut tx,
                &Message::Response {
                    id: request.id,
                    result: Ok(serde_json::to_value(InstallAccepted {
                        operation_id: command.request.operation_id,
                        target: command.request.target,
                    })
                    .unwrap()),
                },
                wire::MAX_MANAGEMENT_REPLY,
            )
            .await
            .unwrap();
            acknowledged.fetch_add(1, Ordering::SeqCst);
        });
        let response = call(
            &client,
            Command {
                id: 1,
                method: Method::RemoteUpdateInstall,
                send_id: None,
                params: serde_json::to_value(InstallCommand {
                    expected_grant: [7; 16],
                    request: install,
                })
                .unwrap(),
            },
        )
        .await
        .unwrap();
        let Message::Response {
            result: Ok(value), ..
        } = response
        else {
            panic!("expected accepted update");
        };
        assert_eq!(
            serde_json::from_value::<InstallAccepted>(value).unwrap(),
            accepted
        );
        server_task.await.unwrap();
        assert_eq!(acknowledgements.load(Ordering::SeqCst), 1);
    };
    tokio::time::timeout(Duration::from_secs(3), exercise)
        .await
        .unwrap();
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn framed_reply_requires_fin_and_rejects_trailing_data() {
    let (a, b, client, server) = endpoints().await;
    let exercise = async {
        // A valid frame without FIN must not retain a management operation.
        let (mut tx, mut rx) = client.open_bi().await.unwrap();
        wire::write(&mut tx, &json!({}), wire::MAX_PREFACE)
            .await
            .unwrap();
        tx.finish().unwrap();
        let (mut reply, mut request) = server.accept_bi().await.unwrap();
        let _: serde_json::Value = wire::read(&mut request, wire::MAX_PREFACE).await.unwrap();
        wire::write(&mut reply, &json!("frame"), wire::MAX_PREFACE)
            .await
            .unwrap();
        let _: serde_json::Value = wire::read(&mut rx, wire::MAX_PREFACE).await.unwrap();
        assert!(
            finish_reply(&mut rx, Duration::from_millis(30))
                .await
                .unwrap_err()
                .contains("did not finish")
        );
        reply.write_all(b"x").await.unwrap();
        assert!(
            finish_reply(&mut rx, Duration::from_secs(1))
                .await
                .unwrap_err()
                .contains("extra data")
        );
    };
    tokio::time::timeout(Duration::from_secs(3), exercise)
        .await
        .unwrap();
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn stop_and_answers_pass_stalled_upload_and_download() {
    let (a, b, client, server) = endpoints().await;
    let exercise = async {
        let capacity = Capacity::new();
        let (mut upload, mut upload_reply) = client.open_bi().await.unwrap();
        let body = wire::encode(
            &json!(["account", {"image": "x".repeat(2 * 1024 * 1024)}]),
            wire::MAX_FRAME,
        )
        .unwrap();
        wire::write(
            &mut upload,
            &Header {
                id: 1,
                method: Method::SendMessage,
                send_id: Some("original-send".into()),
            },
            wire::MAX_PREFACE,
        )
        .await
        .unwrap();
        upload.write_u32(body.len() as u32).await.unwrap();
        upload.write_all(&body[..1024]).await.unwrap();
        let (header, mut tx, mut rx, upload_slot) = receive(&server, &capacity).await;
        let upload_received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let admitted = upload_received.clone();
        let upload_task = tokio::spawn(async move {
            let _slot = upload_slot;
            let request = header.body(&mut rx).await.unwrap();
            assert_eq!(request.send_id.as_deref(), Some("original-send"));
            admitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            reply(
                &mut tx,
                &Message::Response {
                    id: request.id,
                    result: Ok(json!("admitted")),
                },
            )
            .await
            .unwrap();
        });

        // Request an image, then stop reading its reply after the length. Its
        // payload exceeds the actual per-stream receive window, forcing QUIC
        // backpressure rather than relying on a sleeping mock provider.
        let (mut download, mut image) = client.open_bi().await.unwrap();
        wire::write(
            &mut download,
            &Header {
                id: 2,
                method: Method::ReadAttachment,
                send_id: None,
            },
            wire::MAX_PREFACE,
        )
        .await
        .unwrap();
        wire::write(&mut download, &json!([]), wire::MAX_FRAME)
            .await
            .unwrap();
        download.finish().unwrap();
        let (header, mut tx, mut rx, download_slot) = receive(&server, &capacity).await;
        let request = header.body(&mut rx).await.unwrap();
        let download_task = tokio::spawn(async move {
            let _slot = download_slot;
            reply(
                &mut tx,
                &Message::Response {
                    id: request.id,
                    result: Ok(json!("y".repeat(2 * 1024 * 1024))),
                },
            )
            .await
            .unwrap();
        });
        let image_len = image.read_u32().await.unwrap() as usize;
        assert!(image_len > 512 * 1024);

        // Fill the ordinary admission pool as well. Urgent work must not depend
        // on a free ordinary slot, a completed upload or a drained reply.
        let _ordinary: Vec<_> = (0..31)
            .map(|_| capacity.acquire(Method::ListTasks).unwrap())
            .collect();
        assert!(capacity.acquire(Method::SendMessage).is_err());
        for (id, method) in [(3, Method::CancelRun), (4, Method::AnswerQuestion)] {
            let send = call(&client, command(id, method));
            let answer = async {
                let (header, mut tx, mut rx, _slot) = receive(&server, &capacity).await;
                assert!(header.method.urgent());
                let request = header.body(&mut rx).await.unwrap();
                reply(
                    &mut tx,
                    &Message::Response {
                        id: request.id,
                        result: Ok(json!("done")),
                    },
                )
                .await
                .unwrap();
            };
            let (response, ()) =
                tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(send, answer) })
                    .await
                    .expect("urgent command blocked behind a transfer");
            assert!(
                matches!(response.unwrap(), Message::Response { id: received, result: Ok(value) } if received == id && value == json!("done"))
            );
        }
        assert_eq!(upload_received.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!download_task.is_finished());

        // Completing the upload still admits exactly one original command.
        let finish_upload = async {
            upload.write_all(&body[1024..]).await.unwrap();
            upload.finish().unwrap();
            assert!(matches!(
                wire::read::<Message>(&mut upload_reply, wire::MAX_FRAME)
                    .await
                    .unwrap(),
                Message::Response {
                    id: 1,
                    result: Ok(_)
                }
            ));
        };
        let finish_download = async {
            let mut body = vec![0; image_len];
            image.read_exact(&mut body).await.unwrap();
            assert!(matches!(
                serde_json::from_slice::<Message>(&body).unwrap(),
                Message::Response {
                    id: 2,
                    result: Ok(_)
                }
            ));
        };
        tokio::join!(finish_upload, finish_download);
        upload_task.await.unwrap();
        download_task.await.unwrap();
        assert_eq!(upload_received.load(std::sync::atomic::Ordering::SeqCst), 1);
    };
    let result = tokio::time::timeout(Duration::from_secs(20), exercise).await;
    tokio::join!(a.close(), b.close());
    result.unwrap();
}

#[tokio::test]
async fn closing_connection_interrupts_incomplete_body_without_admission() {
    let (a, b, client, server) = endpoints().await;
    let exercise = async {
        let (mut tx, _rx) = client.open_bi().await.unwrap();
        wire::write(
            &mut tx,
            &Header {
                id: 1,
                method: Method::SendMessage,
                send_id: None,
            },
            wire::MAX_PREFACE,
        )
        .await
        .unwrap();
        tx.write_u32(4096).await.unwrap();
        tx.write_all(b"[").await.unwrap();
        let capacity = Capacity::new();
        let (header, _reply, mut body, _slot) = receive(&server, &capacity).await;
        // Used by revocation/account change and explicit connection teardown.
        server.close(0u32.into(), b"revoked");
        assert!(header.body(&mut body).await.is_err());
    };
    let result = tokio::time::timeout(Duration::from_secs(5), exercise).await;
    tokio::join!(a.close(), b.close());
    result.unwrap();
}

#[tokio::test]
async fn urgent_body_is_bounded_and_reply_identity_is_checked() {
    let (a, b, client, server) = endpoints().await;
    let exercise = async {
        let mut oversized = command(7, Method::AnswerQuestion);
        oversized.params = json!("x".repeat(MAX_CONTROL));
        assert!(matches!(
            call(&client, oversized).await.unwrap(),
            Message::Response {
                id: 7,
                result: Err(_)
            }
        ));
        // No stream was emitted for that rejected body: the first accepted
        // header below must be the next request.
        let send = call(&client, command(8, Method::CancelRun));
        let wrong_reply = async {
            let capacity = Capacity::new();
            let (header, mut tx, mut rx, _slot) = receive(&server, &capacity).await;
            assert_eq!(header.id, 8);
            header.body(&mut rx).await.unwrap();
            reply(
                &mut tx,
                &Message::Response {
                    id: 9,
                    result: Ok(json!(null)),
                },
            )
            .await
        };
        let (response, delivery) = tokio::join!(send, wrong_reply);
        assert!(matches!(response.unwrap(), Message::Closed { .. }));
        // Rejecting the deliberately wrong ID drops the receive stream before
        // FIN. QUIC may acknowledge delivery before that rejection, or report
        // STOP_SENDING to the sender. Both preserve the client's refusal.
        if let Err(error) = delivery {
            assert_eq!(error, "Remote consumer rejected the reply");
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(5), exercise).await;
    tokio::join!(a.close(), b.close());
    result.unwrap();
}

#[tokio::test]
async fn ten_mib_image_ranges_and_oversized_page_do_not_break_other_requests() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let (a, b, client, server) = endpoints().await;
    let exercise = async {
        let original = vec![255_u8; 10 * 1024 * 1024];
        let mut reconstructed = Vec::new();
        for (index, chunk) in original.chunks(1024 * 1024).enumerate() {
            let id = index as u64 + 1;
            let value =
                json!({"mime":"image/png","size":original.len(),"data":STANDARD.encode(chunk)});
            let transfer = call(&client, command(id, Method::ReadAttachment));
            let serve = async {
                let capacity = Capacity::new();
                let (header, mut tx, mut rx, _slot) = receive(&server, &capacity).await;
                header.body(&mut rx).await.unwrap();
                reply_with_limit(
                    &mut tx,
                    &Message::Response {
                        id,
                        result: Ok(value),
                    },
                    Method::ReadAttachment.reply_limit(),
                )
                .await
                .unwrap();
            };
            let (response, ()) = tokio::join!(transfer, serve);
            let Message::Response {
                result: Ok(value), ..
            } = response.unwrap()
            else {
                panic!("range failed")
            };
            reconstructed.extend(STANDARD.decode(value["data"].as_str().unwrap()).unwrap());
        }
        assert_eq!(reconstructed, original);
        for (id, method, value) in [
            (20, Method::ListTasks, json!("x".repeat(5 * 1024 * 1024))),
            (21, Method::CancelRun, json!("stopped")),
        ] {
            let transfer = call(&client, command(id, method));
            let serve = async {
                let capacity = Capacity::new();
                let (header, mut tx, mut rx, _slot) = receive(&server, &capacity).await;
                header.body(&mut rx).await.unwrap();
                reply_with_limit(
                    &mut tx,
                    &Message::Response {
                        id,
                        result: Ok(value),
                    },
                    method.reply_limit(),
                )
                .await
                .unwrap();
            };
            let (response, ()) = tokio::join!(transfer, serve);
            let Message::Response { result, .. } = response.unwrap() else {
                panic!("request closed connection")
            };
            if id == 20 {
                assert!(result.unwrap_err().contains("too large"));
            } else {
                assert_eq!(result.unwrap(), json!("stopped"));
            }
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(30), exercise).await;
    tokio::join!(a.close(), b.close());
    result.unwrap();
}

#[test]
fn ranges_have_four_slots_and_do_not_consume_urgent_capacity() {
    let capacity = Capacity::new();
    let _ranges: Vec<_> = (0..MAX_RANGE_REQUESTS)
        .map(|_| capacity.acquire(Method::ReadAttachment).unwrap())
        .collect();
    assert!(capacity.acquire(Method::ReadAttachment).is_err());
    assert!(capacity.acquire(Method::ReadRow).is_err());
    assert!(capacity.acquire(Method::CancelRun).is_ok());
    assert_eq!(Method::OpenTask.priority(), 5);
    assert_eq!(Method::HistoryBefore.priority(), 5);
    assert_eq!(Method::CancelRun.priority(), 10);
}

#[tokio::test]
async fn header_admission_queues_a_full_legitimate_burst_without_rejecting_urgent_calls() {
    let (a, b, client, server) = endpoints().await;
    let capacity = Arc::new(Capacity::new());
    let mut callers = tokio::task::JoinSet::new();
    let mut receivers = tokio::task::JoinSet::new();
    // Every pool may send at once. Forty short valid headers must not race the
    // eight active reader limit and receive incidental busy/refused errors.
    for id in 0..MAX_REQUESTS as u64 {
        let client = client.clone();
        callers.spawn(async move {
            let method = if id < MAX_ORDINARY_REQUESTS as u64 {
                Method::ListTasks
            } else if id < (MAX_ORDINARY_REQUESTS + MAX_RANGE_REQUESTS) as u64 {
                Method::ReadAttachment
            } else if id < (MAX_ORDINARY_REQUESTS + MAX_RANGE_REQUESTS + MAX_BULK_REQUESTS) as u64 {
                Method::UploadAttachment
            } else {
                Method::CancelRun
            };
            call(&client, command(id, method)).await.unwrap()
        });
    }
    for _ in 0..MAX_REQUESTS {
        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let capacity = capacity.clone();
        receivers.spawn(async move {
            // This is the same queued admission and parse path serve_request
            // executes, with real independently accepted QUIC streams.
            let header = capacity.read_header(&mut rx).await.unwrap();
            let id = header.id;
            header.body(&mut rx).await.unwrap();
            reply(
                &mut tx,
                &Message::Response {
                    id,
                    result: Ok(serde_json::Value::Null),
                },
            )
            .await
            .unwrap();
        });
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(response) = callers.join_next().await {
            assert!(matches!(
                response.unwrap(),
                Message::Response { result: Ok(_), .. }
            ));
        }
        while let Some(response) = receivers.join_next().await {
            response.unwrap();
        }
    })
    .await
    .unwrap();
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn incomplete_headers_expire_and_queued_urgent_header_recovers_without_reset() {
    let (a, b, client, server) = endpoints().await;
    let capacity = Arc::new(Capacity::new());
    let mut withheld = Vec::new();
    let mut readers = tokio::task::JoinSet::new();
    for _ in 0..MAX_PENDING_HEADERS {
        let (mut tx, rx) = client.open_bi().await.unwrap();
        tx.write_u32(100).await.unwrap();
        tx.write_all(b"{").await.unwrap();
        withheld.push((tx, rx));
        let (response, mut body) = server.accept_bi().await.unwrap();
        let capacity = capacity.clone();
        readers.spawn(async move {
            let _response = response;
            capacity
                .read_header_with_timeout(&mut body, Duration::from_secs(3))
                .await
        });
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while capacity.headers.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let control = call(&client, command(100, Method::CancelRun));
    let accept = async {
        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let header = capacity
            .read_header_with_timeout(&mut rx, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(header.method, Method::CancelRun);
        let id = header.id;
        header.body(&mut rx).await.unwrap();
        reply(
            &mut tx,
            &Message::Response {
                id,
                result: Ok(serde_json::Value::Null),
            },
        )
        .await
        .unwrap();
    };
    let (answer, ()) = tokio::time::timeout(Duration::from_secs(6), async {
        tokio::join!(control, accept)
    })
    .await
    .unwrap();
    assert!(matches!(
        answer.unwrap(),
        Message::Response { result: Ok(_), .. }
    ));
    while let Some(result) = readers.join_next().await {
        assert!(matches!(result.unwrap(),Err(reason) if reason=="Remote request header timed out"));
    }
    assert_eq!(capacity.headers.available_permits(), MAX_PENDING_HEADERS);
    drop(withheld);
    a.close().await;
    b.close().await;
}

#[test]
fn request_budgets_reserve_header_tasks_and_the_event_stream() {
    let capacity = Capacity::new();
    let mut slots = Vec::new();
    for (method, limit) in [
        (Method::ListTasks, MAX_ORDINARY_REQUESTS),
        (Method::CancelRun, MAX_URGENT_REQUESTS),
        (Method::ReadAttachment, MAX_RANGE_REQUESTS),
        (Method::UploadAttachment, MAX_BULK_REQUESTS),
    ] {
        slots.extend((0..limit).map(|_| capacity.acquire(method).unwrap()));
        assert!(capacity.acquire(method).is_err());
    }
    assert_eq!(slots.len(), MAX_REQUESTS);
    assert_eq!(MAX_REQUEST_TASKS, slots.len() + MAX_PENDING_HEADERS);
    assert_eq!(MAX_BIDI_STREAMS as usize, MAX_REQUEST_TASKS + 1);
    assert_eq!(capacity.headers.available_permits(), MAX_PENDING_HEADERS);
}

#[tokio::test]
async fn saturated_accept_loop_expires_incomplete_headers_and_serves_both_urgent_calls() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{Barrier, mpsc};

    // Exercise both occupied header readers and a completely full task gate.
    // The fixture handler holds classified work; this does not exercise Node
    // authorization or attachment payload decoding.
    for incomplete in [
        MAX_PENDING_HEADERS,
        MAX_PENDING_HEADERS + MAX_URGENT_REQUESTS,
    ] {
        let (a, b, client, server) = endpoints().await;
        let stage = std::cell::Cell::new("handshake");
        let exercise = async {
            // Control's authenticated handshake/event stream consumes the extra
            // transport slot before the production request accept loop starts.
            let (mut event_tx, event_rx) = client.open_bi().await.unwrap();
            event_tx.write_all(b"e").await.unwrap();
            let (event_reply, mut event_request) = server.accept_bi().await.unwrap();
            assert_eq!(event_request.read_u8().await.unwrap(), b'e');
            let _event_stream = (event_tx, event_rx, event_reply, event_request);

            let capacity = Arc::new(Capacity::new());
            let release = Arc::new(Semaphore::new(0));
            let accepted = Arc::new(AtomicUsize::new(0));
            let completed = Arc::new(AtomicUsize::new(0));
            let held = MAX_ORDINARY_REQUESTS + MAX_RANGE_REQUESTS + MAX_BULK_REQUESTS;
            let header_deadline = Duration::from_secs(3);
            let urgent_methods = [Method::CancelRun, Method::AnswerQuestion];
            assert_eq!(urgent_methods.len(), MAX_URGENT_REQUESTS);
            let start_headers = Arc::new(Barrier::new(incomplete + 1));
            let (admitted_tx, mut admitted_rx) = mpsc::unbounded_channel();
            let (pending_tx, mut pending_rx) = mpsc::unbounded_channel();
            let (expired_tx, mut expired_rx) = mpsc::unbounded_channel();
            let (finished_tx, mut finished_rx) = mpsc::unbounded_channel();
            let mut receiver = tokio::task::JoinSet::new();
            receiver.spawn(serve_requests(server, {
                let capacity = capacity.clone();
                let release = release.clone();
                let accepted = accepted.clone();
                let completed = completed.clone();
                let start_headers = start_headers.clone();
                move |mut tx, mut rx| {
                    let index = accepted.fetch_add(1, Ordering::SeqCst);
                    let withheld = (held..held + incomplete).contains(&index);
                    let capacity = capacity.clone();
                    let release = release.clone();
                    let completed = completed.clone();
                    let start_headers = start_headers.clone();
                    let admitted = admitted_tx.clone();
                    let pending = pending_tx.clone();
                    let expired = expired_tx.clone();
                    let finished = finished_tx.clone();
                    async move {
                        let header = {
                            if withheld {
                                start_headers.wait().await;
                            }
                            let mut read = Box::pin(
                                capacity.read_header_with_timeout(&mut rx, header_deadline),
                            );
                            // Report readiness only after polling the real read:
                            // its header permit is held or queued, not merely
                            // waiting for this task to be scheduled.
                            if withheld {
                                let first = std::future::poll_fn(|cx| {
                                    std::task::Poll::Ready(std::future::Future::poll(
                                        read.as_mut(),
                                        cx,
                                    ))
                                })
                                .await;
                                assert!(first.is_pending());
                                pending.send(()).unwrap();
                            }
                            read.await
                        };
                        let header = match header {
                            Ok(header) => header,
                            Err(reason) => {
                                assert!(withheld);
                                assert_eq!(reason, "Remote request header timed out");
                                expired.send(()).unwrap();
                                completed.fetch_add(1, Ordering::SeqCst);
                                finished.send(()).unwrap();
                                return;
                            }
                        };
                        let _slot = capacity.acquire(header.method).unwrap();
                        let method = header.method;
                        let id = header.id;
                        header.body(&mut rx).await.unwrap();
                        admitted.send(method).unwrap();
                        if !method.urgent() {
                            release.acquire().await.unwrap().forget();
                        }
                        reply(
                            &mut tx,
                            &Message::Response {
                                id,
                                result: Ok(json!(null)),
                            },
                        )
                        .await
                        .unwrap();
                        completed.fetch_add(1, Ordering::SeqCst);
                        finished.send(()).unwrap();
                    }
                }
            }));

            let mut callers = tokio::task::JoinSet::new();
            let mut next_id = 0;
            stage.set("class admission");
            for (method, limit) in [
                (Method::ListTasks, MAX_ORDINARY_REQUESTS),
                (Method::ReadAttachment, MAX_RANGE_REQUESTS),
                (Method::UploadAttachment, MAX_BULK_REQUESTS),
            ] {
                for _ in 0..limit {
                    let client = client.clone();
                    let id = next_id;
                    next_id += 1;
                    callers.spawn(async move { call(&client, command(id, method)).await.unwrap() });
                    assert_eq!(admitted_rx.recv().await.unwrap(), method);
                }
                assert!(capacity.acquire(method).is_err());
            }
            let mut withheld_streams = Vec::new();
            stage.set("incomplete-header admission");
            for _ in 0..incomplete {
                let (mut tx, rx) = client.open_bi().await.unwrap();
                tx.write_u32(100).await.unwrap();
                tx.write_all(b"{").await.unwrap();
                withheld_streams.push((tx, rx));
            }
            start_headers.wait().await;
            for _ in 0..incomplete {
                pending_rx.recv().await.unwrap();
            }
            // Every request uses the same queue-and-read deadline. Issue urgent
            // demand on a later timer tick so its absolute deadline follows the
            // already pending incomplete headers, even with coarse timers.
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(capacity.headers.available_permits(), 0);
            assert_eq!(accepted.load(Ordering::SeqCst), held + incomplete);
            assert_eq!(capacity.urgent.available_permits(), MAX_URGENT_REQUESTS);

            let mut urgent = tokio::task::JoinSet::new();
            stage.set("header expiry");
            for method in urgent_methods {
                let client = client.clone();
                let id = next_id;
                next_id += 1;
                urgent.spawn(async move { call(&client, command(id, method)).await.unwrap() });
            }
            for _ in 0..incomplete {
                expired_rx.recv().await.unwrap();
            }
            // The full-cap case also exhausts QUIC stream credit. A rejected
            // client's unfinished send half must close before that credit can
            // be reused: STOP_SENDING alone does not supply its final offset.
            // Honor the server's expired operations while urgent calls wait,
            // rather than retaining these handles until urgent completion.
            drop(withheld_streams);
            stage.set("urgent replies after stream retirement");
            while let Some(response) = urgent.join_next().await {
                assert!(matches!(
                    response.unwrap(),
                    Message::Response { result: Ok(_), .. }
                ));
            }
            assert!(client.close_reason().is_none());
            stage.set("classified request drain");
            release.add_permits(held);
            while let Some(response) = callers.join_next().await {
                assert!(matches!(
                    response.unwrap(),
                    Message::Response { result: Ok(_), .. }
                ));
            }
            assert!(matches!(
                call(&client, command(next_id, Method::CancelRun))
                    .await
                    .unwrap(),
                Message::Response { result: Ok(_), .. }
            ));
            let expected = held + incomplete + urgent_methods.len() + 1;
            for _ in 0..expected {
                finished_rx.recv().await.unwrap();
            }
            client.close(0u32.into(), b"test complete");
            stage.set("accept-loop shutdown");
            receiver.join_next().await.unwrap().unwrap();
            assert!(receiver.is_empty());
            assert_eq!(accepted.load(Ordering::SeqCst), expected);
            assert_eq!(
                completed.load(Ordering::SeqCst),
                accepted.load(Ordering::SeqCst)
            );
            assert_eq!(capacity.ordinary.available_permits(), MAX_ORDINARY_REQUESTS);
            assert_eq!(capacity.ranges.available_permits(), MAX_RANGE_REQUESTS);
            assert_eq!(capacity.bulk.available_permits(), MAX_BULK_REQUESTS);
            assert_eq!(capacity.urgent.available_permits(), MAX_URGENT_REQUESTS);
            assert_eq!(capacity.headers.available_permits(), MAX_PENDING_HEADERS);
        };
        let result = tokio::time::timeout(Duration::from_secs(10), exercise).await;
        tokio::join!(a.close(), b.close());
        result.unwrap_or_else(|error| {
            panic!(
                "{incomplete} incomplete headers at {}: {error}",
                stage.get()
            )
        });
    }
}
