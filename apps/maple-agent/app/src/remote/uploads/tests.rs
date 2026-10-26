use super::*;
use crate::remote::streams::tests::{command, endpoints};
use maple_agent::store::{AccountId, AttachmentUploadStatus, NewTask, Store, StorePaths, TaskKind};
use sha2::{Digest, Sha256};

const TASK: &str = "01234567-89ab-7def-8123-456789abcdef";
const OP: &str = "11111111-1111-4111-8111-111111111111";

fn store(root: &std::path::Path) -> (Arc<Store>, AccountId) {
    let store = Arc::new(Store::open(StorePaths::new(root.join("execution"))).unwrap());
    let account = store.upsert_account("account", 1).unwrap();
    store
        .create_task(
            account,
            &NewTask {
                uuid: TASK.into(),
                project_root: root.to_string_lossy().into_owned(),
                kind: TaskKind::Desktop,
                title: "Upload".into(),
                provisional: true,
                model: None,
                created_ms: 1,
            },
        )
        .unwrap();
    (store, account)
}

fn metadata(bytes: &[u8]) -> AttachmentUploadMetadata {
    AttachmentUploadMetadata {
        name: "notes.bin".into(),
        mime_type: "application/octet-stream".into(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
    }
}

#[test]
fn upload_metadata_cannot_name_a_controller_or_executor_source_path() {
    let value = serde_json::json!({"user":"account","sessionId":TASK,"operationId":OP,"metadata":metadata(b"x"),"path":"/private/controller/secret"});
    assert!(serde_json::from_value::<Upload>(value).is_err());
    assert!(wire::Method::parse("import_attachment").is_err());
}

#[test]
fn bulk_budgets_leave_urgent_capacity_available() {
    let capacity = streams::Capacity::new();
    let _bulk: Vec<_> = (0..2)
        .map(|_| capacity.acquire(wire::Method::UploadAttachment).unwrap())
        .collect();
    let _ordinary: Vec<_> = (0..32)
        .map(|_| capacity.acquire(wire::Method::ListTasks).unwrap())
        .collect();
    assert!(capacity.acquire(wire::Method::UploadAttachment).is_err());
    assert!(capacity.acquire(wire::Method::CancelRun).is_ok());
    assert!(
        capacity
            .acquire(wire::Method::CancelAttachmentUpload)
            .is_ok()
    );
    assert_eq!(wire::MAX_FRAME, 16 * 1024 * 1024);
}

#[tokio::test]
async fn binary_file_larger_than_a_control_frame_round_trips_and_reuses_operation() {
    let root = tempfile::tempdir().unwrap();
    let (store, account) = store(root.path());
    let bytes = vec![0xabu8; wire::MAX_FRAME + 123];
    let path = root.path().join("staged.bin");
    std::fs::write(&path, &bytes).unwrap();
    let import = ImportAttachmentRequest {
        session_id: TASK.into(),
        operation_id: OP.into(),
        path: path.to_string_lossy().into_owned(),
        name: "notes.bin".into(),
        mime_type: "application/octet-stream".into(),
    };
    let (a, b, client, server) = endpoints().await;
    let destination = store.clone();
    let serving = tokio::spawn(async move {
        // The second import repeats metadata and returns its committed receipt
        // before admitting any bytes; it never creates a second attachment.
        for _ in 0..2 {
            let (mut tx, mut rx) = server.accept_bi().await.unwrap();
            let header = streams::Header::read(&mut rx).await.unwrap();
            assert_eq!(header.method, wire::Method::UploadAttachment);
            assert!(header.send_id.is_none());
            let upload: Upload = wire::read(&mut rx, wire::MAX_PREFACE).await.unwrap();
            assert_eq!(upload.metadata.size, (wire::MAX_FRAME + 123) as u64);
            match destination
                .begin_attachment_upload(account, TASK, OP, &upload.metadata)
                .unwrap()
            {
                AttachmentUploadStart::Complete(attachment) => {
                    send_admission(&mut tx, Ok(Admission::Complete(attachment)), true)
                        .await
                        .unwrap()
                }
                AttachmentUploadStart::Writing(writer) => {
                    send_admission(&mut tx, Ok(Admission::Ready), false)
                        .await
                        .unwrap();
                    let writer = receive_bytes(&mut rx, *writer).await.unwrap();
                    let attachment = writer.finish(&destination).unwrap();
                    streams::reply(
                        &mut tx,
                        &Message::Response {
                            id: header.id,
                            result: Ok(serde_json::to_value(attachment).unwrap()),
                        },
                    )
                    .await
                    .unwrap();
                }
            }
        }
    });
    let mut ids = Vec::new();
    for id in 1..=2 {
        let prepared = attachments::prepare(&import).await.unwrap();
        let response = call(&client, id, "account".into(), import.clone(), prepared)
            .await
            .unwrap();
        let Message::Response {
            result: Ok(value), ..
        } = response
        else {
            panic!("upload failed")
        };
        let result: StoredAttachment = serde_json::from_value(value).unwrap();
        assert_eq!(std::fs::read(&result.execution_path).unwrap(), bytes);
        assert_ne!(std::path::Path::new(&result.execution_path), path.as_path());
        ids.push(result.id);
    }
    assert_eq!(ids[0], ids[1]);
    serving.await.unwrap();
    assert!(matches!(
        store.attachment_upload_status(account, TASK, OP).unwrap(),
        AttachmentUploadStatus::Complete(_)
    ));
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn stalled_raw_upload_does_not_delay_stop_or_question_answer() {
    let root = tempfile::tempdir().unwrap();
    let (store, account) = store(root.path());
    let (a, b, client, server) = endpoints().await;
    let capacity = streams::Capacity::new();
    let (mut tx, mut rx) = client.open_bi().await.unwrap();
    wire::write(
        &mut tx,
        &streams::Header {
            id: 1,
            method: wire::Method::UploadAttachment,
            send_id: None,
        },
        wire::MAX_PREFACE,
    )
    .await
    .unwrap();
    let upload = Upload {
        user: "account".into(),
        session_id: TASK.into(),
        operation_id: OP.into(),
        metadata: metadata(&vec![7; 1024 * 1024]),
    };
    wire::write(&mut tx, &upload, wire::MAX_PREFACE)
        .await
        .unwrap();
    let (mut response, mut body) = server.accept_bi().await.unwrap();
    let header = streams::Header::read(&mut body).await.unwrap();
    let slot = capacity.acquire(header.method).unwrap();
    let received: Upload = wire::read(&mut body, wire::MAX_PREFACE).await.unwrap();
    let AttachmentUploadStart::Writing(writer) = store
        .begin_attachment_upload(account, TASK, OP, &received.metadata)
        .unwrap()
    else {
        panic!()
    };
    send_admission(&mut response, Ok(Admission::Ready), false)
        .await
        .unwrap();
    let _: Result<Admission, String> = wire::read(&mut rx, wire::MAX_MANAGEMENT_REPLY)
        .await
        .unwrap();
    tx.write_all(&[7; 128]).await.unwrap();
    let upload_task = tokio::spawn(async move {
        let _slot = slot;
        receive_bytes(&mut body, *writer).await
    });
    for (id, method) in [
        (2, wire::Method::CancelRun),
        (3, wire::Method::AnswerQuestion),
    ] {
        let caller = streams::call(&client, command(id, method));
        let answer = async {
            let (mut tx, mut rx) = server.accept_bi().await.unwrap();
            let header = streams::Header::read(&mut rx).await.unwrap();
            let _slot = capacity.acquire(header.method).unwrap();
            header.body(&mut rx).await.unwrap();
            streams::reply(
                &mut tx,
                &Message::Response {
                    id,
                    result: Ok(serde_json::Value::Null),
                },
            )
            .await
            .unwrap();
        };
        let (reply, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(caller, answer)
        })
        .await
        .unwrap();
        assert!(matches!(
            reply.unwrap(),
            Message::Response { result: Ok(_), .. }
        ));
        assert!(!upload_task.is_finished());
    }
    tx.reset(1u32.into()).unwrap();
    assert!(upload_task.await.unwrap().is_err());
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn raw_truncation_digest_mismatch_and_cancel_never_publish() {
    for kind in ["truncated", "digest", "cancelled", "oversized"] {
        let root = tempfile::tempdir().unwrap();
        let (store, account) = store(root.path());
        let (a, b, client, server) = endpoints().await;
        let declared = metadata(b"abcd");
        let AttachmentUploadStart::Writing(writer) = store
            .begin_attachment_upload(account, TASK, OP, &declared)
            .unwrap()
        else {
            panic!()
        };
        let (mut tx, _rx) = client.open_bi().await.unwrap();
        tx.write_all(if kind == "truncated" {
            b"ab"
        } else if kind == "oversized" {
            b"abcde"
        } else {
            b"zzzz"
        })
        .await
        .unwrap();
        tx.finish().unwrap();
        let (_reply, mut rx) = server.accept_bi().await.unwrap();
        if kind == "cancelled" {
            store.cancel_attachment_upload(account, TASK, OP).unwrap();
        }
        let result = match receive_bytes(&mut rx, *writer).await {
            Ok(writer) => writer.finish(&store).map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        assert!(result.is_err(), "{kind}");
        assert!(
            !matches!(
                store.attachment_upload_status(account, TASK, OP).unwrap(),
                AttachmentUploadStatus::Complete(_)
            ),
            "{kind}"
        );
        // A failed bulk request does not terminate the underlying connection.
        assert!(client.close_reason().is_none());
        a.close().await;
        b.close().await;
    }
}

#[tokio::test]
async fn lost_final_ack_is_resolved_by_the_upload_receipt_without_prompt_replay() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(crate::backend::attachments::tests::fixture(root.path()));
    let store = backend.store().unwrap().clone();
    let account = store.account_id("account").unwrap().unwrap();
    let path = root.path().join("stable-upload.bin");
    std::fs::write(&path, b"abcd").unwrap();
    let request = ImportAttachmentRequest {
        session_id: TASK.into(),
        operation_id: OP.into(),
        path: path.to_string_lossy().into_owned(),
        name: "notes.bin".into(),
        mime_type: "application/octet-stream".into(),
    };
    let (a, b, client, server) = endpoints().await;
    let serving = tokio::spawn(async move {
        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let header = streams::Header::read(&mut rx).await.unwrap();
        assert_eq!(header.method, wire::Method::UploadAttachment);
        let upload: Upload = read_metadata(&mut rx, streams::IO_TIMEOUT).await.unwrap();
        let AttachmentUploadStart::Writing(writer) = store
            .begin_attachment_upload(account, TASK, OP, &upload.metadata)
            .unwrap()
        else {
            panic!()
        };
        send_admission(&mut tx, Ok(Admission::Ready), false)
            .await
            .unwrap();
        let writer = receive_bytes(&mut rx, *writer).await.unwrap();
        let attachment = writer.finish(&store).unwrap();
        // Lose the response after durable publication, not before admission.
        tx.reset(1u32.into()).unwrap();

        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let header = streams::Header::read(&mut rx).await.unwrap();
        assert_eq!(header.method, wire::Method::AttachmentUploadStatus);
        let command = header.body(&mut rx).await.unwrap();
        let result = crate::backend::dispatch(
            &backend,
            crate::host::protocol::Request {
                id: command.id,
                method: command.method.name(),
                send_id: None,
                params: command.params,
            },
        )
        .await;
        streams::reply(
            &mut tx,
            &Message::Response {
                id: command.id,
                result,
            },
        )
        .await
        .unwrap();

        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let header = streams::Header::read(&mut rx).await.unwrap();
        assert_eq!(header.method, wire::Method::UploadAttachment);
        let upload: Upload = read_metadata(&mut rx, streams::IO_TIMEOUT).await.unwrap();
        let AttachmentUploadStart::Complete(repeated) = store
            .begin_attachment_upload(account, TASK, OP, &upload.metadata)
            .unwrap()
        else {
            panic!("retry must not admit another byte transfer")
        };
        assert_eq!(attachment.id, repeated.id);
        send_admission(&mut tx, Ok(Admission::Complete(repeated)), true)
            .await
            .unwrap();
        // All three requests were uploads/status: none can enter send_message.
        attachment
    });
    assert!(
        call(
            &client,
            1,
            "account".into(),
            request.clone(),
            attachments::prepare(&request).await.unwrap()
        )
        .await
        .is_err()
    );
    let status = streams::call(
        &client,
        wire::Command {
            id: 2,
            method: wire::Method::AttachmentUploadStatus,
            send_id: None,
            params: serde_json::json!([
                "account",
                crate::backend::AttachmentUploadRequest {
                    session_id: TASK.into(),
                    operation_id: OP.into()
                }
            ]),
        },
    )
    .await
    .unwrap();
    let Message::Response {
        result: Ok(value), ..
    } = status
    else {
        panic!("status lookup failed")
    };
    let AttachmentUploadStatus::Complete(recovered) = serde_json::from_value(value).unwrap() else {
        panic!("committed receipt missing")
    };
    let retry = call(
        &client,
        3,
        "account".into(),
        request.clone(),
        attachments::prepare(&request).await.unwrap(),
    )
    .await
    .unwrap();
    let Message::Response {
        result: Ok(value), ..
    } = retry
    else {
        panic!("idempotent retry failed")
    };
    assert_eq!(
        serde_json::from_value::<StoredAttachment>(value)
            .unwrap()
            .id,
        recovered.id
    );
    assert_eq!(serving.await.unwrap().id, recovered.id);
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn cancelling_a_stalled_upload_releases_its_stream_without_waiting_for_idle_timeout() {
    let root = tempfile::tempdir().unwrap();
    let (store, account) = store(root.path());
    let (a, b, client, server) = endpoints().await;
    let AttachmentUploadStart::Writing(writer) = store
        .begin_attachment_upload(account, TASK, OP, &metadata(b"abcd"))
        .unwrap()
    else {
        panic!()
    };
    let (mut tx, _rx) = client.open_bi().await.unwrap();
    tx.write_all(b"a").await.unwrap();
    let (_response, mut body) = server.accept_bi().await.unwrap();
    let receiving = tokio::spawn(async move { receive_bytes(&mut body, *writer).await });
    store.cancel_attachment_upload(account, TASK, OP).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), receiving)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(error) if error=="Attachment upload cancelled"));
    assert_eq!(
        store.attachment_upload_status(account, TASK, OP).unwrap(),
        AttachmentUploadStatus::Cancelled
    );
    a.close().await;
    b.close().await;
}

#[tokio::test(start_paused = true)]
async fn slowly_dripped_upload_metadata_has_an_overall_deadline() {
    use tokio::io::AsyncWriteExt;
    let (mut tx, mut rx) = tokio::io::duplex(64);
    let send = tokio::spawn(async move {
        tx.write_u32(100).await.unwrap();
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if tx.write_all(b" ").await.is_err() {
                break;
            }
        }
    });
    let started = tokio::time::Instant::now();
    let error = read_metadata(&mut rx, Duration::from_secs(3))
        .await
        .unwrap_err();
    assert_eq!(error, "Attachment metadata timed out");
    assert_eq!(started.elapsed(), Duration::from_secs(3));
    send.abort();
}

#[test]
fn revocation_disable_and_repair_do_not_publish_an_upload_admitted_under_an_old_grant() {
    for mode in ["revoke", "disable", "re-pair", "account stopped"] {
        let root = tempfile::tempdir().unwrap();
        let (store, account) = store(root.path());
        let mut policy = crate::remote::store::Store::open(
            store.clone(),
            "local",
            "project",
            "unpackaged",
            "account",
        )
        .unwrap();
        let peer = iroh::SecretKey::generate().public();
        let grant = crate::remote::store::Peer {
            name: "Controller".into(),
            grant: [1; 16],
            controls_this_computer: true,
            can_connect: false,
            pending_confirmation: None,
        };
        policy.set_enabled(true).unwrap();
        policy.grant(peer, grant.clone()).unwrap();
        let AttachmentUploadStart::Writing(mut writer) = store
            .begin_attachment_upload(account, TASK, OP, &metadata(b"abcd"))
            .unwrap()
        else {
            panic!()
        };
        writer.write_chunk(b"abcd").unwrap();
        let prepared = writer.prepare(&store).unwrap();
        match mode {
            "revoke" => policy.revoke(peer).unwrap(),
            "disable" => policy.set_enabled(false).unwrap(),
            "re-pair" => {
                policy.revoke(peer).unwrap();
                let mut next = grant;
                next.grant = [2; 16];
                policy.grant(peer, next).unwrap();
            }
            _ => {}
        }
        assert!(
            finish_authorized(
                prepared,
                &store,
                &policy,
                peer,
                [1; 16],
                mode == "account stopped"
            )
            .is_err(),
            "{mode}"
        );
        assert!(
            !matches!(
                store.attachment_upload_status(account, TASK, OP).unwrap(),
                AttachmentUploadStatus::Complete(_)
            ),
            "{mode}"
        );
    }
}

struct PublicationPause {
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

struct ReceiverFixture {
    backend: Arc<LocalBackend>,
    policy: RwLock<crate::remote::store::Store>,
    stopped: std::sync::atomic::AtomicBool,
    publication: Option<PublicationPause>,
}

impl UploadReceiver for ReceiverFixture {
    fn backend(&self) -> &LocalBackend {
        &self.backend
    }
    fn account(&self) -> &str {
        "account"
    }
    fn policy(&self) -> &RwLock<crate::remote::store::Store> {
        &self.policy
    }
    fn stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }
    async fn ensure_valid(&self) -> Result<(), String> {
        if self.stopped() {
            Err("The remote account is no longer active".into())
        } else {
            Ok(())
        }
    }
    fn before_publish(&self) {
        if let Some(pause) = &self.publication {
            pause
                .entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            // A dropped release sender also unblocks the worker after a failed
            // assertion; no test may strand a blocking publication thread.
            let _ = pause
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
        }
    }
}

fn receiver_fixture(
    root: &std::path::Path,
    peer: EndpointId,
    publication: Option<PublicationPause>,
) -> Arc<ReceiverFixture> {
    let backend = Arc::new(attachments::tests::fixture(root));
    let mut policy = crate::remote::store::Store::open(
        backend.store().unwrap().clone(),
        "local",
        "project",
        "unpackaged",
        "account",
    )
    .unwrap();
    policy.set_enabled(true).unwrap();
    policy
        .grant(
            peer,
            crate::remote::store::Peer {
                name: "Controller".into(),
                grant: [1; 16],
                controls_this_computer: true,
                can_connect: false,
                pending_confirmation: None,
            },
        )
        .unwrap();
    Arc::new(ReceiverFixture {
        backend,
        policy: RwLock::new(policy),
        stopped: std::sync::atomic::AtomicBool::new(false),
        publication,
    })
}

fn fixture_import(root: &std::path::Path) -> ImportAttachmentRequest {
    let path = root.join("staged.bin");
    std::fs::write(&path, b"abcd").unwrap();
    ImportAttachmentRequest {
        session_id: TASK.into(),
        operation_id: OP.into(),
        path: path.to_string_lossy().into_owned(),
        name: "notes.bin".into(),
        mime_type: "application/octet-stream".into(),
    }
}

#[tokio::test]
async fn production_receiver_publication_keeps_control_authorization_responsive() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, client, server) = endpoints().await;
    let peer = a.id();
    let (entered, publishing) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let receiver = receiver_fixture(
        root.path(),
        peer,
        Some(PublicationPause {
            entered: std::sync::Mutex::new(Some(entered)),
            release: std::sync::Mutex::new(held),
        }),
    );
    let destination = receiver.clone();
    let capacity = Arc::new(streams::Capacity::new());
    let serving = tokio::spawn(streams::serve_requests(server, move |mut tx, mut rx| {
        let receiver = destination.clone();
        let capacity = capacity.clone();
        async move {
            let header = capacity.read_header(&mut rx).await.unwrap();
            let _slot = capacity.acquire(header.method).unwrap();
            if header.method == wire::Method::UploadAttachment {
                serve_receiver(receiver, peer, header.id, &mut tx, &mut rx)
                    .await
                    .unwrap();
            } else {
                let request = header.body(&mut rx).await.unwrap();
                assert!(matches!(
                    request.method,
                    wire::Method::CancelRun | wire::Method::AnswerQuestion
                ));
                // Exercise the same current account/grant check while the real
                // upload receiver owns its publication guard. SDK session
                // validation belongs to Node and the authenticated live probe.
                check_receiver(receiver.as_ref(), peer, [1; 16])
                    .await
                    .unwrap();
                let result = crate::backend::dispatch(
                    &receiver.backend,
                    crate::host::protocol::Request {
                        id: request.id,
                        method: request.method.name(),
                        send_id: None,
                        params: request.params,
                    },
                )
                .await;
                streams::reply(
                    &mut tx,
                    &Message::Response {
                        id: request.id,
                        result,
                    },
                )
                .await
                .unwrap();
            }
        }
    }));
    let request = fixture_import(root.path());
    let upload_client = client.clone();
    let upload = tokio::spawn(async move {
        let prepared = attachments::prepare(&request).await.unwrap();
        call(&upload_client, 1, "account".into(), request, prepared).await
    });
    tokio::time::timeout(Duration::from_secs(3), publishing)
        .await
        .unwrap()
        .unwrap();
    assert!(
        receiver.policy.try_write().is_err(),
        "publication must exclude revoke"
    );
    for (id, method, params) in [
        (
            2,
            wire::Method::CancelRun,
            serde_json::json!(["account", "run"]),
        ),
        (
            3,
            wire::Method::AnswerQuestion,
            serde_json::json!(["account", "question", "answer"]),
        ),
    ] {
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            streams::call(
                &client,
                wire::Command {
                    id,
                    method,
                    send_id: None,
                    params,
                },
            ),
        )
        .await
        .expect("control authorization waited for disk publication")
        .unwrap();
        // The fixture has no running agent/question; a backend response still
        // proves the control reached dispatch while publication was held.
        assert!(matches!(response, Message::Response { id: received, .. } if received == id));
        assert!(!upload.is_finished());
    }
    release.send(()).unwrap();
    assert!(matches!(
        upload.await.unwrap().unwrap(),
        Message::Response { result: Ok(_), .. }
    ));
    client.close(0u32.into(), b"done");
    serving.await.unwrap();
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn production_receiver_rechecks_revoke_disable_and_repair_after_payload() {
    for mode in ["revoke", "disable", "re-pair", "stopped"] {
        let root = tempfile::tempdir().unwrap();
        let (a, b, client, server) = endpoints().await;
        let peer = a.id();
        let receiver = receiver_fixture(root.path(), peer, None);
        let destination = receiver.clone();
        let serving = tokio::spawn(async move {
            let (mut tx, mut rx) = server.accept_bi().await.unwrap();
            let header = streams::Header::read(&mut rx).await.unwrap();
            serve_receiver(destination, peer, header.id, &mut tx, &mut rx).await
        });
        let (mut tx, mut rx) = client.open_bi().await.unwrap();
        wire::write(
            &mut tx,
            &streams::Header {
                id: 1,
                method: wire::Method::UploadAttachment,
                send_id: None,
            },
            wire::MAX_PREFACE,
        )
        .await
        .unwrap();
        wire::write(
            &mut tx,
            &Upload {
                user: "account".into(),
                session_id: TASK.into(),
                operation_id: OP.into(),
                metadata: metadata(b"abcd"),
            },
            wire::MAX_PREFACE,
        )
        .await
        .unwrap();
        let admission: Result<Admission, String> = wire::read(&mut rx, wire::MAX_MANAGEMENT_REPLY)
            .await
            .unwrap();
        assert!(matches!(admission, Ok(Admission::Ready)));
        tx.write_all(b"abcd").await.unwrap();
        {
            let mut policy = receiver.policy.write().unwrap();
            match mode {
                "revoke" => policy.revoke(peer).unwrap(),
                "disable" => policy.set_enabled(false).unwrap(),
                "re-pair" => {
                    let mut grant = policy.peers().get(&peer.to_string()).unwrap().clone();
                    grant.grant = [2; 16];
                    policy.revoke(peer).unwrap();
                    policy.grant(peer, grant).unwrap();
                }
                _ => receiver
                    .stopped
                    .store(true, std::sync::atomic::Ordering::Release),
            }
        }
        tx.finish().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), serving)
                .await
                .unwrap()
                .unwrap()
                .is_err(),
            "{mode}"
        );
        let store = receiver.backend.store().unwrap();
        let account = store.account_id("account").unwrap().unwrap();
        assert!(
            !matches!(
                store.attachment_upload_status(account, TASK, OP).unwrap(),
                AttachmentUploadStatus::Complete(_)
            ),
            "{mode}"
        );
        a.close().await;
        b.close().await;
    }
}

#[tokio::test]
async fn aborted_receiver_keeps_account_guard_until_publication_worker_exits() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, client, server) = endpoints().await;
    let peer = a.id();
    let (entered, publishing) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let receiver = receiver_fixture(
        root.path(),
        peer,
        Some(PublicationPause {
            entered: std::sync::Mutex::new(Some(entered)),
            release: std::sync::Mutex::new(held),
        }),
    );
    let destination = receiver.clone();
    let serving = tokio::spawn(async move {
        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let header = streams::Header::read(&mut rx).await.unwrap();
        serve_receiver(destination, peer, header.id, &mut tx, &mut rx).await
    });
    let request = fixture_import(root.path());
    let upload_client = client.clone();
    let upload = tokio::spawn(async move {
        let prepared = attachments::prepare(&request).await.unwrap();
        call(&upload_client, 1, "account".into(), request, prepared).await
    });
    tokio::time::timeout(Duration::from_secs(3), publishing)
        .await
        .unwrap()
        .unwrap();
    serving.abort();
    assert!(serving.await.unwrap_err().is_cancelled());
    assert!(
        receiver.backend.account_gate.try_write().is_err(),
        "aborting the stream released account ownership before its disk worker"
    );
    receiver
        .stopped
        .store(true, std::sync::atomic::Ordering::Release);
    release.send(()).unwrap();
    let transition = tokio::time::timeout(
        Duration::from_secs(3),
        receiver.backend.account_gate.write(),
    )
    .await
    .unwrap();
    let store = receiver.backend.store().unwrap();
    let account = store.account_id("account").unwrap().unwrap();
    assert!(!matches!(
        store.attachment_upload_status(account, TASK, OP).unwrap(),
        AttachmentUploadStatus::Complete(_)
    ));
    drop(transition);
    client.close(0u32.into(), b"done");
    assert!(upload.await.unwrap().is_err());
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn production_receiver_requires_authority_for_begin_crash_reconciliation() {
    fn partial_file(directory: &std::path::Path) -> Option<std::path::PathBuf> {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                if let Some(path) = partial_file(&entry.path()) {
                    return Some(path);
                }
            } else if entry.file_name() == ".part" {
                return Some(entry.path());
            }
        }
        None
    }
    for permitted in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let (a, b, client, server) = endpoints().await;
        let peer = a.id();
        let receiver = receiver_fixture(root.path(), peer, None);
        let store = receiver.backend.store().unwrap().clone();
        let account = store.account_id("account").unwrap().unwrap();
        let AttachmentUploadStart::Writing(mut writer) = store
            .begin_attachment_upload(account, TASK, OP, &metadata(b"abcd"))
            .unwrap()
        else {
            panic!()
        };
        writer.write_chunk(b"abcd").unwrap();
        drop(writer);
        // Simulate a crash after rename but before the ready-row commit, using
        // only this disposable fixture's files. Begin must authorize recovery.
        let partial = partial_file(root.path()).unwrap();
        std::fs::rename(&partial, partial.with_file_name("notes.bin")).unwrap();
        if !permitted {
            receiver.policy.write().unwrap().revoke(peer).unwrap();
        }
        let destination = receiver.clone();
        let serving = tokio::spawn(async move {
            let (mut tx, mut rx) = server.accept_bi().await.unwrap();
            let header = streams::Header::read(&mut rx).await.unwrap();
            serve_receiver(destination, peer, header.id, &mut tx, &mut rx).await
        });
        let request = fixture_import(root.path());
        let prepared = attachments::prepare(&request).await.unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(3),
            call(&client, 1, "account".into(), request, prepared),
        )
        .await
        .unwrap();
        assert_eq!(response.is_ok(), permitted);
        assert_eq!(
            matches!(
                store.attachment_upload_status(account, TASK, OP).unwrap(),
                AttachmentUploadStatus::Complete(_)
            ),
            permitted
        );
        let _ = tokio::time::timeout(Duration::from_secs(3), serving)
            .await
            .unwrap()
            .unwrap();
        a.close().await;
        b.close().await;
    }
}
